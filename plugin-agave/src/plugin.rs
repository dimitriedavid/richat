use {
    crate::{
        channel::Sender,
        config::Config,
        metrics,
        version::VERSION,
    },
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        GeyserPlugin, GeyserPluginError, ReplicaAccountInfoVersions, ReplicaBlockInfoVersions,
        ReplicaEntryInfoVersions, ReplicaTransactionInfoVersions, Result as PluginResult,
        SlotStatus,
    },
    futures::future::BoxFuture,
    log::error,
    richat_metrics::{MaybeRecorder, gauge},
    richat_shared::transports::{grpc::GrpcServer, quic::QuicServer},
    solana_clock::Slot,
    std::{fmt, sync::Arc, time::Duration},
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
            Self::Slot        => 1 << 0,
            Self::Account     => 1 << 1,
            Self::Transaction => 1 << 2,
            Self::Entry       => 1 << 3,
            Self::BlockMeta   => 1 << 4,
        }
    }
}

pub struct OwnedUpdate {
    pub notification: PluginNotification,
    pub created_at: std::time::SystemTime,
    pub slot: solana_clock::Slot,
    pub payload: richat_proto::geyser::subscribe_update::UpdateOneof,
    /// Only meaningful when notification == Slot
    pub slot_status_i32: i32,
    pub slot_confirmed: bool,
    pub slot_finalized: bool,
}

struct PluginTask(BoxFuture<'static, Result<(), JoinError>>);

unsafe impl Sync for PluginTask {}

impl fmt::Debug for PluginTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginTask").finish()
    }
}

async fn encoder_task(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<OwnedUpdate>,
    sender: crate::channel::Sender,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use {prost::Message, prost_types::Timestamp, richat_proto::geyser::SubscribeUpdate, std::sync::Arc};

    loop {
        tokio::select! {
            biased;
            Some(owned) = rx.recv() => {
                let notification = owned.notification;
                let slot = owned.slot;
                let slot_status_i32 = owned.slot_status_i32;
                let slot_confirmed = owned.slot_confirmed;
                let slot_finalized = owned.slot_finalized;
                let bytes = SubscribeUpdate {
                    filters: vec![],
                    update_oneof: Some(owned.payload),
                    created_at: Some(Timestamp::from(owned.created_at)),
                }
                .encode_to_vec();
                sender.push_encoded(notification, slot, slot_status_i32, slot_confirmed, slot_finalized, Arc::new(bytes));
            }
            _ = shutdown.cancelled() => break,
        }
    }
}

#[derive(Debug)]
pub struct PluginInner {
    runtime: Runtime,
    messages: Sender,
    tx: tokio::sync::mpsc::UnboundedSender<OwnedUpdate>,
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
        let (messages, tx, shutdown, tasks) = runtime
            .block_on(async move {
                let shutdown = CancellationToken::new();
                let mut tasks = Vec::with_capacity(4);

                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<OwnedUpdate>();
                let encoder_sender = messages.clone();
                tasks.push((
                    "Encoder Task",
                    PluginTask(Box::pin(
                        tokio::spawn(encoder_task(rx, encoder_sender, shutdown.clone()))
                    )),
                ));

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

                Ok::<_, anyhow::Error>((messages, tx, shutdown, tasks))
            })
            .map_err(|error| GeyserPluginError::Custom(format!("{error:?}").into()))?;

        Ok(Self {
            runtime,
            messages,
            tx,
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
            inner.messages.close();

            inner.shutdown.cancel();
            inner.runtime.block_on(async {
                for (name, task) in inner.tasks {
                    if let Err(error) = task.0.await {
                        error!("failed to join `{name}` task: {error:?}");
                    }
                }
            });

            inner.runtime.shutdown_timeout(Duration::from_secs(10));
        }
    }

    fn update_account(&self, account: ReplicaAccountInfoVersions, slot: u64, is_startup: bool) -> PluginResult<()> {
        if !is_startup {
            let account = match account {
                ReplicaAccountInfoVersions::V0_0_1(_) => unreachable!("ReplicaAccountInfoVersions::V0_0_1 is not supported"),
                ReplicaAccountInfoVersions::V0_0_2(_) => unreachable!("ReplicaAccountInfoVersions::V0_0_2 is not supported"),
                ReplicaAccountInfoVersions::V0_0_3(info) => info,
            };
            let inner = self.inner.as_ref().expect("initialized");
            inner.tx.send(OwnedUpdate {
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
                            txn_signature: account.txn.as_ref().map(|txn| txn.signature().as_ref().to_vec()),
                        }),
                        slot,
                        is_startup: false,
                    },
                ),
                slot_status_i32: 0,
                slot_confirmed: false,
                slot_finalized: false,
            }).ok();
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
        use richat_proto::geyser::SlotStatus as ProtoSlotStatus;
        let (status_i32, confirmed, finalized) = match status {
            SlotStatus::Processed           => (0i32, false, false),
            SlotStatus::Confirmed           => (1,    true,  false),
            SlotStatus::Rooted              => (2,    true,  true),
            SlotStatus::FirstShredReceived  => (3,    false, false),
            SlotStatus::Completed           => (4,    false, false),
            SlotStatus::CreatedBank         => (5,    false, false),
            SlotStatus::Dead(_)             => (6,    false, false),
        };
        let inner = self.inner.as_ref().expect("initialized");
        inner.tx.send(OwnedUpdate {
            notification: PluginNotification::Slot,
            created_at: std::time::SystemTime::now(),
            slot,
            payload: richat_proto::geyser::subscribe_update::UpdateOneof::Slot(
                richat_proto::geyser::SubscribeUpdateSlot {
                    slot,
                    parent,
                    status: match status {
                        SlotStatus::Processed           => ProtoSlotStatus::SlotProcessed,
                        SlotStatus::Confirmed           => ProtoSlotStatus::SlotConfirmed,
                        SlotStatus::Rooted              => ProtoSlotStatus::SlotFinalized,
                        SlotStatus::FirstShredReceived  => ProtoSlotStatus::SlotFirstShredReceived,
                        SlotStatus::Completed           => ProtoSlotStatus::SlotCompleted,
                        SlotStatus::CreatedBank         => ProtoSlotStatus::SlotCreatedBank,
                        SlotStatus::Dead(_)             => ProtoSlotStatus::SlotDead,
                    } as i32,
                    dead_error: if let SlotStatus::Dead(err) = status { Some(err.clone()) } else { None },
                },
            ),
            slot_status_i32: status_i32,
            slot_confirmed: confirmed,
            slot_finalized: finalized,
        }).ok();
        Ok(())
    }

    fn notify_transaction(
        &self,
        transaction: ReplicaTransactionInfoVersions<'_>,
        slot: u64,
    ) -> PluginResult<()> {
        use richat_proto::convert_to;
        let transaction = match transaction {
            ReplicaTransactionInfoVersions::V0_0_1(_) => unreachable!("ReplicaAccountInfoVersions::V0_0_1 is not supported"),
            ReplicaTransactionInfoVersions::V0_0_2(_) => unreachable!("ReplicaAccountInfoVersions::V0_0_2 is not supported"),
            ReplicaTransactionInfoVersions::V0_0_3(info) => info,
        };
        let inner = self.inner.as_ref().expect("initialized");
        inner.tx.send(OwnedUpdate {
            notification: PluginNotification::Transaction,
            created_at: std::time::SystemTime::now(),
            slot,
            payload: richat_proto::geyser::subscribe_update::UpdateOneof::Transaction(
                richat_proto::geyser::SubscribeUpdateTransaction {
                    transaction: Some(richat_proto::geyser::SubscribeUpdateTransactionInfo {
                        signature: transaction.signature.as_ref().to_vec(),
                        is_vote: transaction.is_vote,
                        transaction: Some(convert_to::create_transaction(transaction.transaction)),
                        meta: Some(convert_to::create_transaction_meta(transaction.transaction_status_meta)),
                        index: transaction.index as u64,
                    }),
                    slot,
                },
            ),
            slot_status_i32: 0,
            slot_confirmed: false,
            slot_finalized: false,
        }).ok();
        Ok(())
    }

    fn notify_entry(&self, entry: ReplicaEntryInfoVersions) -> PluginResult<()> {
        let entry = match entry {
            ReplicaEntryInfoVersions::V0_0_1(_) => unreachable!("ReplicaEntryInfoVersions::V0_0_1 is not supported"),
            ReplicaEntryInfoVersions::V0_0_2(e) => e,
        };
        let inner = self.inner.as_ref().expect("initialized");
        inner.tx.send(OwnedUpdate {
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
        }).ok();
        Ok(())
    }

    fn notify_block_metadata(&self, blockinfo: ReplicaBlockInfoVersions<'_>) -> PluginResult<()> {
        use richat_proto::convert_to;
        let blockinfo = match blockinfo {
            ReplicaBlockInfoVersions::V0_0_1(_) => unreachable!("ReplicaBlockInfoVersions::V0_0_1 is not supported"),
            ReplicaBlockInfoVersions::V0_0_2(_) => unreachable!("ReplicaBlockInfoVersions::V0_0_2 is not supported"),
            ReplicaBlockInfoVersions::V0_0_3(_) => unreachable!("ReplicaBlockInfoVersions::V0_0_3 is not supported"),
            ReplicaBlockInfoVersions::V0_0_4(info) => info,
        };
        let inner = self.inner.as_ref().expect("initialized");
        inner.tx.send(OwnedUpdate {
            notification: PluginNotification::BlockMeta,
            created_at: std::time::SystemTime::now(),
            slot: blockinfo.slot,
            payload: richat_proto::geyser::subscribe_update::UpdateOneof::BlockMeta(
                richat_proto::geyser::SubscribeUpdateBlockMeta {
                    slot: blockinfo.slot,
                    blockhash: blockinfo.blockhash.to_string(),
                    rewards: Some(convert_to::create_rewards_obj(&blockinfo.rewards.rewards, blockinfo.rewards.num_partitions)),
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
        }).ok();
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
        super::{OwnedUpdate, PluginNotification},
        agave_geyser_plugin_interface::geyser_plugin_interface::SlotStatus,
        prost::Message,
        prost_types::Timestamp,
        richat_benches::fixtures::{
            generate_accounts, generate_block_metas, generate_entries, generate_slots,
            generate_transactions,
        },
        richat_proto::{
            convert_to,
            geyser::{
                SlotStatus as ProtoSlotStatus, SubscribeUpdate, SubscribeUpdateAccount,
                SubscribeUpdateAccountInfo, SubscribeUpdateBlockMeta, SubscribeUpdateEntry,
                SubscribeUpdateSlot, SubscribeUpdateTransaction, SubscribeUpdateTransactionInfo,
                subscribe_update::UpdateOneof,
            },
        },
        std::time::SystemTime,
    };

    fn encode_owned(payload: UpdateOneof, created_at: SystemTime) -> Vec<u8> {
        SubscribeUpdate {
            filters: vec![],
            update_oneof: Some(payload),
            created_at: Some(Timestamp::from(created_at)),
        }
        .encode_to_vec()
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
            let (status_i32, confirmed, finalized) = match status {
                SlotStatus::Processed           => (0i32, false, false),
                SlotStatus::Confirmed           => (1,    true,  false),
                SlotStatus::Rooted              => (2,    true,  true),
                SlotStatus::FirstShredReceived  => (3,    false, false),
                SlotStatus::Completed           => (4,    false, false),
                SlotStatus::CreatedBank         => (5,    false, false),
                SlotStatus::Dead(_)             => (6,    false, false),
            };
            let owned = OwnedUpdate {
                notification: PluginNotification::Slot,
                created_at,
                slot,
                payload: UpdateOneof::Slot(SubscribeUpdateSlot {
                    slot,
                    parent,
                    status: match status {
                        SlotStatus::Processed           => ProtoSlotStatus::SlotProcessed,
                        SlotStatus::Confirmed           => ProtoSlotStatus::SlotConfirmed,
                        SlotStatus::Rooted              => ProtoSlotStatus::SlotFinalized,
                        SlotStatus::FirstShredReceived  => ProtoSlotStatus::SlotFirstShredReceived,
                        SlotStatus::Completed           => ProtoSlotStatus::SlotCompleted,
                        SlotStatus::CreatedBank         => ProtoSlotStatus::SlotCreatedBank,
                        SlotStatus::Dead(_)             => ProtoSlotStatus::SlotDead,
                    } as i32,
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
