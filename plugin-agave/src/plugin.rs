use {
    crate::{channel::Sender, config::Config, metrics, version::VERSION},
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        GeyserPlugin, GeyserPluginError, ReplicaAccountInfoVersions, ReplicaBlockInfoVersions,
        ReplicaEntryInfoVersions, ReplicaTransactionInfoVersions, Result as PluginResult,
        SlotStatus,
    },
    futures::future::BoxFuture,
    log::error,
    metrics_exporter_prometheus::PrometheusRecorder,
    richat_metrics::{MaybeRecorder, counter, gauge},
    richat_shared::transports::{grpc::GrpcServer, quic::QuicServer},
    solana_clock::Slot,
    std::{fmt, io, sync::Arc, time::Duration},
    tokio::{runtime::Runtime, task::JoinError},
    tokio_util::sync::CancellationToken,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginNotification {
    Slot,
    Account,
    Transaction,
    Entry,
    BlockMeta,
}

impl PluginNotification {
    pub const fn bit(self) -> u8 {
        match self {
            Self::Slot => 1 << 0,
            Self::Account => 1 << 1,
            Self::Transaction => 1 << 2,
            Self::Entry => 1 << 3,
            Self::BlockMeta => 1 << 4,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Slot => "slot",
            Self::Account => "account",
            Self::Transaction => "transaction",
            Self::Entry => "entry",
            Self::BlockMeta => "block_meta",
        }
    }
}

pub struct OwnedUpdate {
    pub notification: PluginNotification,
    pub created_at: std::time::SystemTime,
    pub slot: solana_clock::Slot,
    pub payload: richat_proto::geyser::subscribe_update::UpdateOneof,
    /// Only meaningful when `notification == PluginNotification::Slot`.
    /// Maps to the integer value used in `push_msg_encoded` and `update_metrics`.
    /// Must be 0 (zero-value) for all non-Slot messages.
    pub slot_status_i32: i32,
    /// Only meaningful when `notification == PluginNotification::Slot`.
    /// Must be `false` for all non-Slot messages.
    pub slot_confirmed: bool,
    /// Only meaningful when `notification == PluginNotification::Slot`.
    /// Must be `false` for all non-Slot messages.
    pub slot_finalized: bool,
}

pub(crate) const fn slot_status_fields(
    status: &agave_geyser_plugin_interface::geyser_plugin_interface::SlotStatus,
) -> (i32, bool, bool, richat_proto::geyser::SlotStatus) {
    use {
        agave_geyser_plugin_interface::geyser_plugin_interface::SlotStatus,
        richat_proto::geyser::SlotStatus as ProtoSlotStatus,
    };
    match status {
        SlotStatus::Processed => (0, false, false, ProtoSlotStatus::SlotProcessed),
        SlotStatus::Confirmed => (1, true, false, ProtoSlotStatus::SlotConfirmed),
        SlotStatus::Rooted => (2, true, true, ProtoSlotStatus::SlotFinalized),
        SlotStatus::FirstShredReceived => {
            (3, false, false, ProtoSlotStatus::SlotFirstShredReceived)
        }
        SlotStatus::Completed => (4, false, false, ProtoSlotStatus::SlotCompleted),
        SlotStatus::CreatedBank => (5, false, false, ProtoSlotStatus::SlotCreatedBank),
        SlotStatus::Dead(_) => (6, false, false, ProtoSlotStatus::SlotDead),
    }
}

#[derive(Debug)]
struct EncoderState {
    messages: Sender,
    recorder: Arc<MaybeRecorder<PrometheusRecorder>>,
}

#[derive(Clone)]
struct EncoderQueue {
    tx: tokio::sync::mpsc::Sender<OwnedUpdate>,
    capacity: usize,
    state: Arc<EncoderState>,
}

impl fmt::Debug for EncoderQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncoderQueue")
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl EncoderQueue {
    fn update_queue_metric(&self) {
        let queued = self.tx.max_capacity().saturating_sub(self.tx.capacity());
        gauge!(&self.state.recorder, metrics::ENCODER_QUEUE_SIZE).set(queued as f64);
    }

    fn push(&self, owned: OwnedUpdate) -> PluginResult<()> {
        if let Err(error) = self.tx.blocking_send(owned) {
            counter!(
                &self.state.recorder,
                metrics::ENCODER_QUEUE_FAILURE_TOTAL,
                "notification" => error.0.notification.as_str(),
                "reason" => "closed"
            )
            .increment(1);
            return Err(GeyserPluginError::Custom(Box::new(io::Error::other(
                format!(
                    "encoder queue is closed; failed to process {} update for slot {}",
                    error.0.notification.as_str(),
                    error.0.slot
                ),
            ))));
        }
        self.update_queue_metric();
        Ok(())
    }
}

struct PluginTask(BoxFuture<'static, Result<(), JoinError>>);

unsafe impl Sync for PluginTask {}

impl fmt::Debug for PluginTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginTask").finish()
    }
}

async fn encoder_task(
    mut rx: tokio::sync::mpsc::Receiver<OwnedUpdate>,
    state: Arc<EncoderState>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use {
        prost::Message, prost_types::Timestamp, richat_proto::geyser::SubscribeUpdate,
        std::sync::Arc,
    };

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            maybe_owned = rx.recv() => match maybe_owned {
                Some(owned) => {
                    let notification = owned.notification;
                    let slot_meta = crate::channel::SlotMeta {
                        slot: owned.slot,
                        status_i32: owned.slot_status_i32,
                        confirmed: owned.slot_confirmed,
                        finalized: owned.slot_finalized,
                    };
                    let bytes = SubscribeUpdate {
                        filters: vec![],
                        update_oneof: Some(owned.payload),
                        created_at: Some(Timestamp::from(owned.created_at)),
                    }
                    .encode_to_vec();
                    state.messages.push_encoded(notification, slot_meta, Arc::new(bytes));
                    gauge!(&state.recorder, metrics::ENCODER_QUEUE_SIZE).set(rx.len() as f64);
                }
                None => break,
            }
        }
    }
    gauge!(&state.recorder, metrics::ENCODER_QUEUE_SIZE).set(0.0);
}

#[derive(Debug)]
pub struct PluginInner {
    runtime: Runtime,
    messages: Sender,
    encoder: EncoderQueue,
    shutdown: CancellationToken,
    tasks: Vec<(&'static str, PluginTask)>,
}

impl PluginInner {
    fn new(config: Config) -> PluginResult<Self> {
        let (metrics_recorder, metrics_handle) = if config.metrics.is_some() {
            let recorder = metrics::setup();
            let handle = recorder.handle();
            (Arc::new(recorder.into()), Some(handle))
        } else {
            (Arc::new(MaybeRecorder::Noop), None)
        };

        // Create Tokio runtime
        let runtime = config
            .tokio
            .build_runtime("richatPlugin")
            .map_err(|error| GeyserPluginError::Custom(Box::new(error)))?;

        // Create messages store
        let messages = Sender::new(config.channel, Arc::clone(&metrics_recorder));

        // Spawn servers
        let (messages, encoder, shutdown, tasks) = runtime
            .block_on(async move {
                let shutdown = CancellationToken::new();
                let mut tasks = Vec::with_capacity(4);
                let encoder_state = Arc::new(EncoderState {
                    messages: messages.clone(),
                    recorder: Arc::clone(&metrics_recorder),
                });
                let encoder_queue_size = config.channel.encoder_queue_size.max(1);

                let (tx, rx) = tokio::sync::mpsc::channel::<OwnedUpdate>(encoder_queue_size);
                tasks.push((
                    "Encoder Task",
                    PluginTask(Box::pin(
                        tokio::spawn(encoder_task(rx, Arc::clone(&encoder_state), shutdown.clone()))
                    )),
                ));
                let encoder = EncoderQueue {
                    tx,
                    capacity: encoder_queue_size,
                    state: encoder_state,
                };

                // Start gRPC
                if let Some(config) = config.grpc {
                    let connections_inc = gauge!(&metrics_recorder, metrics::CONNECTIONS_TOTAL, "transport" => "grpc");
                    let connections_dec = connections_inc.clone();
                    tasks.push((
                        "gRPC Server",
                        PluginTask(Box::pin(
                            GrpcServer::spawn(
                                config,
                                messages.clone(),
                                move || connections_inc.increment(1), // on_conn_new_cb
                                move || connections_dec.decrement(1), // on_conn_drop_cb
                                VERSION,
                                shutdown.clone(),
                            )
                            .await?,
                        )),
                    ));
                }

                // Start Quic
                if let Some(config) = config.quic {
                    let connections_inc = gauge!(&metrics_recorder, metrics::CONNECTIONS_TOTAL, "transport" => "quic");
                    let connections_dec = connections_inc.clone();
                    tasks.push((
                        "Quic Server",
                        PluginTask(Box::pin(
                            QuicServer::spawn(
                                config,
                                messages.clone(),
                                move || connections_inc.increment(1), // on_conn_new_cb
                                move || connections_dec.decrement(1), // on_conn_drop_cb
                                VERSION,
                                shutdown.clone(),
                            )
                            .await?,
                        )),
                    ));
                }

                // Start prometheus server
                if let (Some(config), Some(metrics_handle)) = (config.metrics, metrics_handle) {
                    tasks.push((
                        "Prometheus Server",
                        PluginTask(Box::pin(
                            metrics::spawn_server(config, metrics_handle, shutdown.clone().cancelled_owned()).await?,
                        )),
                    ));
                }

                Ok::<_, anyhow::Error>((messages, encoder, shutdown, tasks))
            })
            .map_err(|error| GeyserPluginError::Custom(format!("{error:?}").into()))?;

        Ok(Self {
            runtime,
            messages,
            encoder,
            shutdown,
            tasks,
        })
    }
}

#[derive(Debug, Default)]
pub struct Plugin {
    inner: Option<PluginInner>,
}

impl GeyserPlugin for Plugin {
    fn name(&self) -> &'static str {
        concat!(env!("CARGO_PKG_NAME"), "-", env!("CARGO_PKG_VERSION"))
    }

    fn on_load(&mut self, config_file: &str, _is_reload: bool) -> PluginResult<()> {
        solana_logger::setup_with_default("info");
        let config = Config::load_from_file(config_file).inspect_err(|error| {
            error!("failed to load config: {error:?}");
        })?;

        // Setup logger from the config
        solana_logger::setup_with_default(&config.logs.level);

        // Create inner
        self.inner = Some(PluginInner::new(config).inspect_err(|error| {
            error!("failed to load plugin from the config: {error:?}");
        })?);

        Ok(())
    }

    fn on_unload(&mut self) {
        if let Some(inner) = self.inner.take() {
            let PluginInner {
                runtime,
                messages,
                encoder,
                shutdown,
                tasks,
            } = inner;

            messages.close();
            drop(encoder);
            shutdown.cancel();
            runtime.block_on(async {
                for (name, task) in tasks {
                    if let Err(error) = task.0.await {
                        error!("failed to join `{name}` task: {error:?}");
                    }
                }
            });

            runtime.shutdown_timeout(Duration::from_secs(10));
        }
    }

    fn update_account(
        &self,
        account: ReplicaAccountInfoVersions,
        slot: u64,
        is_startup: bool,
    ) -> PluginResult<()> {
        if !is_startup {
            let account = match account {
                ReplicaAccountInfoVersions::V0_0_1(_) => {
                    unreachable!("ReplicaAccountInfoVersions::V0_0_1 is not supported")
                }
                ReplicaAccountInfoVersions::V0_0_2(_) => {
                    unreachable!("ReplicaAccountInfoVersions::V0_0_2 is not supported")
                }
                ReplicaAccountInfoVersions::V0_0_3(info) => info,
            };
            let inner = self.inner.as_ref().expect("initialized");
            inner.encoder.push(OwnedUpdate {
                notification: PluginNotification::Account,
                created_at: std::time::SystemTime::now(),
                slot,
                payload: richat_proto::geyser::subscribe_update::UpdateOneof::Account(
                    richat_proto::geyser::SubscribeUpdateAccount {
                        account: Some(richat_proto::geyser::SubscribeUpdateAccountInfo {
                            pubkey: account.pubkey.to_vec(),
                            lamports: account.lamports,
                            owner: account.owner.to_vec(),
                            executable: account.executable,
                            rent_epoch: account.rent_epoch,
                            data: account.data.to_vec(),
                            write_version: account.write_version,
                            txn_signature: account
                                .txn
                                .as_ref()
                                .map(|txn| txn.signature().as_ref().to_vec()),
                        }),
                        slot,
                        is_startup: false,
                    },
                ),
                slot_status_i32: 0,
                slot_confirmed: false,
                slot_finalized: false,
            })?;
        }
        Ok(())
    }

    fn notify_end_of_startup(&self) -> PluginResult<()> {
        Ok(())
    }

    fn update_slot_status(
        &self,
        slot: Slot,
        parent: Option<u64>,
        status: &SlotStatus,
    ) -> PluginResult<()> {
        let (status_i32, confirmed, finalized, proto_status) = slot_status_fields(status);
        let inner = self.inner.as_ref().expect("initialized");
        inner.encoder.push(OwnedUpdate {
            notification: PluginNotification::Slot,
            created_at: std::time::SystemTime::now(),
            slot,
            payload: richat_proto::geyser::subscribe_update::UpdateOneof::Slot(
                richat_proto::geyser::SubscribeUpdateSlot {
                    slot,
                    parent,
                    status: proto_status as i32,
                    dead_error: if let SlotStatus::Dead(err) = status {
                        Some(err.clone())
                    } else {
                        None
                    },
                },
            ),
            slot_status_i32: status_i32,
            slot_confirmed: confirmed,
            slot_finalized: finalized,
        })?;
        Ok(())
    }

    fn notify_transaction(
        &self,
        transaction: ReplicaTransactionInfoVersions<'_>,
        slot: u64,
    ) -> PluginResult<()> {
        use richat_proto::convert_to;
        let transaction = match transaction {
            ReplicaTransactionInfoVersions::V0_0_1(_) => {
                unreachable!("ReplicaAccountInfoVersions::V0_0_1 is not supported")
            }
            ReplicaTransactionInfoVersions::V0_0_2(_) => {
                unreachable!("ReplicaAccountInfoVersions::V0_0_2 is not supported")
            }
            ReplicaTransactionInfoVersions::V0_0_3(info) => info,
        };
        let inner = self.inner.as_ref().expect("initialized");
        inner.encoder.push(OwnedUpdate {
            notification: PluginNotification::Transaction,
            created_at: std::time::SystemTime::now(),
            slot,
            payload: richat_proto::geyser::subscribe_update::UpdateOneof::Transaction(
                richat_proto::geyser::SubscribeUpdateTransaction {
                    transaction: Some(richat_proto::geyser::SubscribeUpdateTransactionInfo {
                        signature: transaction.signature.as_ref().to_vec(),
                        is_vote: transaction.is_vote,
                        transaction: Some(convert_to::create_transaction(transaction.transaction)),
                        meta: Some(convert_to::create_transaction_meta(
                            transaction.transaction_status_meta,
                        )),
                        index: transaction.index as u64,
                    }),
                    slot,
                },
            ),
            slot_status_i32: 0,
            slot_confirmed: false,
            slot_finalized: false,
        })?;
        Ok(())
    }

    fn notify_entry(&self, entry: ReplicaEntryInfoVersions) -> PluginResult<()> {
        let entry = match entry {
            ReplicaEntryInfoVersions::V0_0_1(_) => {
                unreachable!("ReplicaEntryInfoVersions::V0_0_1 is not supported")
            }
            ReplicaEntryInfoVersions::V0_0_2(e) => e,
        };
        let inner = self.inner.as_ref().expect("initialized");
        inner.encoder.push(OwnedUpdate {
            notification: PluginNotification::Entry,
            created_at: std::time::SystemTime::now(),
            slot: entry.slot,
            payload: richat_proto::geyser::subscribe_update::UpdateOneof::Entry(
                richat_proto::geyser::SubscribeUpdateEntry {
                    slot: entry.slot,
                    index: entry.index as u64,
                    num_hashes: entry.num_hashes,
                    hash: entry.hash.to_vec(),
                    executed_transaction_count: entry.executed_transaction_count,
                    starting_transaction_index: entry.starting_transaction_index as u64,
                },
            ),
            slot_status_i32: 0,
            slot_confirmed: false,
            slot_finalized: false,
        })?;
        Ok(())
    }

    fn notify_block_metadata(&self, blockinfo: ReplicaBlockInfoVersions<'_>) -> PluginResult<()> {
        use richat_proto::convert_to;
        let blockinfo = match blockinfo {
            ReplicaBlockInfoVersions::V0_0_1(_) => {
                unreachable!("ReplicaBlockInfoVersions::V0_0_1 is not supported")
            }
            ReplicaBlockInfoVersions::V0_0_2(_) => {
                unreachable!("ReplicaBlockInfoVersions::V0_0_2 is not supported")
            }
            ReplicaBlockInfoVersions::V0_0_3(_) => {
                unreachable!("ReplicaBlockInfoVersions::V0_0_3 is not supported")
            }
            ReplicaBlockInfoVersions::V0_0_4(info) => info,
        };
        let inner = self.inner.as_ref().expect("initialized");
        inner.encoder.push(OwnedUpdate {
            notification: PluginNotification::BlockMeta,
            created_at: std::time::SystemTime::now(),
            slot: blockinfo.slot,
            payload: richat_proto::geyser::subscribe_update::UpdateOneof::BlockMeta(
                richat_proto::geyser::SubscribeUpdateBlockMeta {
                    slot: blockinfo.slot,
                    blockhash: blockinfo.blockhash.to_string(),
                    rewards: Some(convert_to::create_rewards_obj(
                        &blockinfo.rewards.rewards,
                        blockinfo.rewards.num_partitions,
                    )),
                    block_time: blockinfo.block_time.map(convert_to::create_timestamp),
                    block_height: blockinfo.block_height.map(convert_to::create_block_height),
                    parent_slot: blockinfo.parent_slot,
                    parent_blockhash: blockinfo.parent_blockhash.to_string(),
                    executed_transaction_count: blockinfo.executed_transaction_count,
                    entries_count: blockinfo.entry_count,
                },
            ),
            slot_status_i32: 0,
            slot_confirmed: false,
            slot_finalized: false,
        })?;
        Ok(())
    }

    fn account_data_notifications_enabled(&self) -> bool {
        true
    }

    fn account_data_snapshot_notifications_enabled(&self) -> bool {
        false
    }

    fn transaction_notifications_enabled(&self) -> bool {
        true
    }

    fn entry_notifications_enabled(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{
            EncoderQueue, EncoderState, OwnedUpdate, PluginNotification, encoder_task,
            slot_status_fields,
        },
        crate::channel::Sender,
        agave_geyser_plugin_interface::geyser_plugin_interface::SlotStatus,
        metrics_exporter_prometheus::PrometheusRecorder,
        prost::Message,
        prost_types::Timestamp,
        richat_benches::fixtures::{
            generate_accounts, generate_block_metas, generate_entries, generate_slots,
            generate_transactions,
        },
        richat_metrics::MaybeRecorder,
        richat_proto::{
            convert_to,
            geyser::{
                SubscribeUpdate, SubscribeUpdateAccount, SubscribeUpdateAccountInfo,
                SubscribeUpdateBlockMeta, SubscribeUpdateEntry, SubscribeUpdateSlot,
                SubscribeUpdateTransaction, SubscribeUpdateTransactionInfo,
                subscribe_update::UpdateOneof,
            },
        },
        richat_shared::transports::Subscribe,
        std::{
            sync::{Arc, mpsc as std_mpsc},
            thread,
            time::{Duration, SystemTime},
        },
        tokio_util::sync::CancellationToken,
    };

    fn encode_owned(payload: UpdateOneof, created_at: SystemTime) -> Vec<u8> {
        SubscribeUpdate {
            filters: vec![],
            update_oneof: Some(payload),
            created_at: Some(Timestamp::from(created_at)),
        }
        .encode_to_vec()
    }

    fn sample_slot_update(slot: u64) -> OwnedUpdate {
        OwnedUpdate {
            notification: PluginNotification::Slot,
            created_at: SystemTime::UNIX_EPOCH,
            slot,
            payload: UpdateOneof::Slot(SubscribeUpdateSlot {
                slot,
                parent: slot.checked_sub(1),
                status: richat_proto::geyser::SlotStatus::SlotProcessed as i32,
                dead_error: None,
            }),
            slot_status_i32: 0,
            slot_confirmed: false,
            slot_finalized: false,
        }
    }

    #[test]
    fn test_encode_account() {
        let created_at = SystemTime::now();
        for item in generate_accounts() {
            let (slot, replica) = item.to_replica();
            let owned = OwnedUpdate {
                notification: PluginNotification::Account,
                created_at,
                slot,
                payload: UpdateOneof::Account(SubscribeUpdateAccount {
                    account: Some(SubscribeUpdateAccountInfo {
                        pubkey: replica.pubkey.to_vec(),
                        lamports: replica.lamports,
                        owner: replica.owner.to_vec(),
                        executable: replica.executable,
                        rent_epoch: replica.rent_epoch,
                        data: replica.data.to_vec(),
                        write_version: replica.write_version,
                        txn_signature: replica
                            .txn
                            .as_ref()
                            .map(|txn| txn.signature().as_ref().to_vec()),
                    }),
                    slot,
                    is_startup: false,
                }),
                slot_status_i32: 0,
                slot_confirmed: false,
                slot_finalized: false,
            };
            let expected = SubscribeUpdate {
                filters: vec![],
                update_oneof: Some(UpdateOneof::Account(item.to_prost())),
                created_at: Some(Timestamp::from(created_at)),
            }
            .encode_to_vec();
            assert_eq!(
                encode_owned(owned.payload, created_at),
                expected,
                "account: {item:?}"
            );
        }
    }

    #[test]
    fn test_encode_transaction() {
        let created_at = SystemTime::now();
        for item in generate_transactions() {
            let (slot, replica) = item.to_replica();
            let owned = OwnedUpdate {
                notification: PluginNotification::Transaction,
                created_at,
                slot,
                payload: UpdateOneof::Transaction(SubscribeUpdateTransaction {
                    transaction: Some(SubscribeUpdateTransactionInfo {
                        signature: replica.signature.as_ref().to_vec(),
                        is_vote: replica.is_vote,
                        transaction: Some(convert_to::create_transaction(replica.transaction)),
                        meta: Some(convert_to::create_transaction_meta(
                            replica.transaction_status_meta,
                        )),
                        index: replica.index as u64,
                    }),
                    slot,
                }),
                slot_status_i32: 0,
                slot_confirmed: false,
                slot_finalized: false,
            };
            let expected = SubscribeUpdate {
                filters: vec![],
                update_oneof: Some(UpdateOneof::Transaction(item.to_prost())),
                created_at: Some(Timestamp::from(created_at)),
            }
            .encode_to_vec();
            assert_eq!(
                encode_owned(owned.payload, created_at),
                expected,
                "transaction: {item:?}"
            );
        }
    }

    #[test]
    fn test_encode_entry() {
        let created_at = SystemTime::now();
        for item in generate_entries() {
            let replica = item.to_replica();
            let owned = OwnedUpdate {
                notification: PluginNotification::Entry,
                created_at,
                slot: replica.slot,
                payload: UpdateOneof::Entry(SubscribeUpdateEntry {
                    slot: replica.slot,
                    index: replica.index as u64,
                    num_hashes: replica.num_hashes,
                    hash: replica.hash.to_vec(),
                    executed_transaction_count: replica.executed_transaction_count,
                    starting_transaction_index: replica.starting_transaction_index as u64,
                }),
                slot_status_i32: 0,
                slot_confirmed: false,
                slot_finalized: false,
            };
            let expected = SubscribeUpdate {
                filters: vec![],
                update_oneof: Some(UpdateOneof::Entry(item.to_prost())),
                created_at: Some(Timestamp::from(created_at)),
            }
            .encode_to_vec();
            assert_eq!(
                encode_owned(owned.payload, created_at),
                expected,
                "entry: {item:?}"
            );
        }
    }

    #[test]
    fn test_encode_slot() {
        let created_at = SystemTime::now();
        for item in generate_slots() {
            let (slot, parent, status) = item.to_replica();
            let (status_i32, confirmed, finalized, proto_status) = slot_status_fields(status);
            let owned = OwnedUpdate {
                notification: PluginNotification::Slot,
                created_at,
                slot,
                payload: UpdateOneof::Slot(SubscribeUpdateSlot {
                    slot,
                    parent,
                    status: proto_status as i32,
                    dead_error: if let SlotStatus::Dead(err) = status {
                        Some(err.clone())
                    } else {
                        None
                    },
                }),
                slot_status_i32: status_i32,
                slot_confirmed: confirmed,
                slot_finalized: finalized,
            };
            let expected = SubscribeUpdate {
                filters: vec![],
                update_oneof: Some(UpdateOneof::Slot(item.to_prost())),
                created_at: Some(Timestamp::from(created_at)),
            }
            .encode_to_vec();
            assert_eq!(
                encode_owned(owned.payload, created_at),
                expected,
                "slot: {item:?}"
            );
        }
    }

    #[test]
    fn test_encode_block_meta() {
        let created_at = SystemTime::now();
        for item in generate_block_metas() {
            let replica = item.to_replica();
            let owned = OwnedUpdate {
                notification: PluginNotification::BlockMeta,
                created_at,
                slot: replica.slot,
                payload: UpdateOneof::BlockMeta(SubscribeUpdateBlockMeta {
                    slot: replica.slot,
                    blockhash: replica.blockhash.to_string(),
                    rewards: Some(convert_to::create_rewards_obj(
                        &replica.rewards.rewards,
                        replica.rewards.num_partitions,
                    )),
                    block_time: replica.block_time.map(convert_to::create_timestamp),
                    block_height: replica.block_height.map(convert_to::create_block_height),
                    parent_slot: replica.parent_slot,
                    parent_blockhash: replica.parent_blockhash.to_string(),
                    executed_transaction_count: replica.executed_transaction_count,
                    entries_count: replica.entry_count,
                }),
                slot_status_i32: 0,
                slot_confirmed: false,
                slot_finalized: false,
            };
            let expected = SubscribeUpdate {
                filters: vec![],
                update_oneof: Some(UpdateOneof::BlockMeta(item.to_prost())),
                created_at: Some(Timestamp::from(created_at)),
            }
            .encode_to_vec();
            assert_eq!(
                encode_owned(owned.payload, created_at),
                expected,
                "block_meta: {item:?}"
            );
        }
    }

    #[test]
    fn encoder_queue_blocks_when_full_until_capacity_is_available() {
        let messages = Sender::new(
            crate::config::ConfigChannel {
                max_messages: 16,
                max_bytes: 1024 * 1024,
                encoder_queue_size: 1,
            },
            Arc::new(MaybeRecorder::<PrometheusRecorder>::Noop),
        );
        let state = Arc::new(EncoderState {
            messages: messages.clone(),
            recorder: Arc::new(MaybeRecorder::<PrometheusRecorder>::Noop),
        });
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(sample_slot_update(10)).unwrap();
        let queue = EncoderQueue {
            tx,
            capacity: 1,
            state,
        };
        let (done_tx, done_rx) = std_mpsc::channel();

        thread::spawn(move || {
            queue.push(sample_slot_update(11)).unwrap();
            done_tx.send(()).unwrap();
        });

        thread::sleep(Duration::from_millis(50));
        assert!(
            done_rx.try_recv().is_err(),
            "push should block while the queue is full"
        );

        let first = rx.blocking_recv().unwrap();
        assert_eq!(first.slot, 10);

        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let second = rx.blocking_recv().unwrap();
        assert_eq!(second.slot, 11);
    }

    #[test]
    fn encoder_queue_returns_error_when_closed() {
        let messages = Sender::new(
            crate::config::ConfigChannel {
                max_messages: 16,
                max_bytes: 1024 * 1024,
                encoder_queue_size: 1,
            },
            Arc::new(MaybeRecorder::<PrometheusRecorder>::Noop),
        );
        let state = Arc::new(EncoderState {
            messages,
            recorder: Arc::new(MaybeRecorder::<PrometheusRecorder>::Noop),
        });
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        let queue = EncoderQueue {
            tx,
            capacity: 1,
            state,
        };

        let error = queue.push(sample_slot_update(12)).unwrap_err();
        assert!(format!("{error}").contains("encoder queue is closed"));
    }

    #[tokio::test]
    async fn encoder_task_stops_on_shutdown_without_draining_backlog() {
        let messages = Sender::new(
            crate::config::ConfigChannel {
                max_messages: 16,
                max_bytes: 1024 * 1024,
                encoder_queue_size: 1,
            },
            Arc::new(MaybeRecorder::<PrometheusRecorder>::Noop),
        );
        let state = Arc::new(EncoderState {
            messages: messages.clone(),
            recorder: Arc::new(MaybeRecorder::<PrometheusRecorder>::Noop),
        });
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(sample_slot_update(13)).unwrap();
        drop(tx);
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        encoder_task(rx, Arc::clone(&state), shutdown).await;

        assert!(messages.subscribe(Some(13), None).is_err());
    }

    #[test]
    fn encode_owned_update_keeps_slot_metadata() {
        let owned = sample_slot_update(14);
        let notification = owned.notification;
        let slot_meta = crate::channel::SlotMeta {
            slot: owned.slot,
            status_i32: owned.slot_status_i32,
            confirmed: owned.slot_confirmed,
            finalized: owned.slot_finalized,
        };
        let bytes = SubscribeUpdate {
            filters: vec![],
            update_oneof: Some(owned.payload),
            created_at: Some(Timestamp::from(owned.created_at)),
        }
        .encode_to_vec();
        assert_eq!(notification, PluginNotification::Slot);
        assert_eq!(slot_meta.slot, 14);
        assert_eq!(slot_meta.status_i32, 0);
        let decoded = SubscribeUpdate::decode(bytes.as_slice()).unwrap();
        let Some(UpdateOneof::Slot(slot)) = decoded.update_oneof else {
            panic!("expected slot update");
        };
        assert_eq!(slot.slot, 14);
    }
}

#[cfg(feature = "plugin")]
#[unsafe(no_mangle)]
#[allow(improper_ctypes_definitions)]
/// # Safety
///
/// This function returns the Plugin pointer as trait GeyserPlugin.
pub unsafe extern "C" fn _create_plugin() -> *mut dyn GeyserPlugin {
    #[cfg(feature = "rustls-install-default-provider")]
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to call CryptoProvider::install_default()");

    let plugin = Plugin::default();
    let plugin: Box<dyn GeyserPlugin> = Box::new(plugin);
    Box::into_raw(plugin)
}
