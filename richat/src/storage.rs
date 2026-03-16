use {
    crate::{
        channel::{IndexLocation, ParsedMessage, SharedChannel},
        config::ConfigStorage,
        grpc::server::SubscribeClient,
        metrics::{
            CHANNEL_STORAGE_ROCKSDB_COMPACTION_PENDING,
            CHANNEL_STORAGE_ROCKSDB_ESTIMATE_PENDING_COMPACTION_BYTES,
            CHANNEL_STORAGE_ROCKSDB_LIVE_SST_FILES_SIZE, CHANNEL_STORAGE_ROCKSDB_MEMTABLE_SIZE,
            CHANNEL_STORAGE_ROCKSDB_NUM_FILES_AT_LEVEL, CHANNEL_STORAGE_TRIM_FROM_INDEX,
            CHANNEL_STORAGE_TRIM_REQUESTS_MERGED_TOTAL, CHANNEL_STORAGE_TRIM_REQUESTS_TOTAL,
            CHANNEL_STORAGE_TRIM_SLOTS, CHANNEL_STORAGE_TRIM_TO_INDEX,
            CHANNEL_STORAGE_WRITE_BATCH_SIZE, CHANNEL_STORAGE_WRITE_DURATION_SECONDS,
            CHANNEL_STORAGE_WRITE_INDEX, CHANNEL_STORAGE_WRITE_SER_BATCH_SIZE,
            CHANNEL_STORAGE_WRITE_SER_INDEX, CHANNEL_STORAGE_WRITE_SER_QUEUE_BYTES,
            CHANNEL_STORAGE_WRITE_SER_QUEUE_SIZE, GrpcSubscribeMessage,
        },
        util::SpawnedThreads,
    },
    ::metrics::{Gauge, counter, gauge},
    anyhow::Context,
    hyper::body::Buf,
    prost::{
        bytes::BufMut,
        encoding::{decode_varint, encode_varint},
    },
    quanta::Instant as QuantaInstant,
    richat_filter::{
        filter::{FilteredUpdate, FilteredUpdateFilters},
        message::{Message, MessageParserEncoding, MessageRef},
    },
    richat_metrics::duration_to_seconds,
    richat_proto::geyser::SlotStatus,
    richat_shared::mutex_lock,
    rocksdb::{
        ColumnFamily, ColumnFamilyDescriptor, DB, DBCompressionType, Direction, IteratorMode,
        Options, WriteBatch,
    },
    smallvec::SmallVec,
    solana_clock::Slot,
    solana_commitment_config::CommitmentLevel,
    std::{
        borrow::Cow,
        collections::{BTreeMap, VecDeque},
        mem,
        sync::{Arc, Condvar, Mutex},
        thread,
        time::{Duration, Instant as StdInstant},
    },
    tokio_util::sync::CancellationToken,
    tonic::Status,
};

trait ColumnName {
    const NAME: &'static str;
}

#[derive(Debug)]
struct MessageIndex;

impl ColumnName for MessageIndex {
    const NAME: &'static str = "message_index";
}

impl MessageIndex {
    const fn encode(key: u64) -> [u8; 8] {
        key.to_be_bytes()
    }

    fn decode(slice: &[u8]) -> anyhow::Result<u64> {
        slice
            .try_into()
            .map(u64::from_be_bytes)
            .context("invalid slice size")
    }
}

#[derive(Debug)]
struct MessageIndexValue;

impl MessageIndexValue {
    fn encode(slot: Slot, message: FilteredUpdate, buf: &mut impl BufMut) {
        encode_varint(slot, buf);
        message.encode(buf);
    }

    fn decode(mut slice: &[u8], parser: MessageParserEncoding) -> anyhow::Result<(Slot, Message)> {
        let slot =
            decode_varint(&mut slice).context("invalid slice size, failed to decode slot")?;
        let message =
            Message::parse(Cow::Borrowed(slice), parser).context("failed to parse message")?;
        Ok((slot, message))
    }
}

#[derive(Debug)]
struct SlotIndex;

impl ColumnName for SlotIndex {
    const NAME: &'static str = "slot_index";
}

impl SlotIndex {
    const fn encode(key: Slot) -> [u8; 8] {
        key.to_be_bytes()
    }

    fn decode(slice: &[u8]) -> anyhow::Result<Slot> {
        slice
            .try_into()
            .map(Slot::from_be_bytes)
            .context("invalid slice size")
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SlotIndexValue {
    pub finalized: bool,
    pub head: u64,
}

impl SlotIndexValue {
    fn encode(finalized: bool, head: u64, buf: &mut impl BufMut) {
        buf.put_u8(if finalized { 1 } else { 0 });
        encode_varint(head, buf);
    }

    fn decode(mut slice: &[u8]) -> anyhow::Result<Self> {
        Ok(Self {
            finalized: match slice.try_get_u8().context("failed to read finalized")? {
                0 => false,
                1 => true,
                value => anyhow::bail!("failed to read finalized, unknown value: {value}"),
            },
            head: decode_varint(&mut slice).context("failed to read head")?,
        })
    }
}

#[derive(Debug)]
struct QueueBudget {
    max_bytes: usize,
    queued_bytes: Mutex<usize>,
    cvar: Condvar,
}

impl QueueBudget {
    fn new(max_bytes: usize) -> Self {
        gauge!(CHANNEL_STORAGE_WRITE_SER_QUEUE_BYTES).set(0.0);
        Self {
            max_bytes,
            queued_bytes: Mutex::new(0),
            cvar: Condvar::new(),
        }
    }

    fn reserve(&self, bytes: usize) {
        if self.max_bytes == 0 {
            return;
        }

        let bytes = bytes.min(self.max_bytes);
        let mut queued_bytes = mutex_lock(&self.queued_bytes);
        while *queued_bytes + bytes > self.max_bytes {
            queued_bytes = self.cvar.wait(queued_bytes).expect("queue budget poisoned");
        }
        *queued_bytes += bytes;
        gauge!(CHANNEL_STORAGE_WRITE_SER_QUEUE_BYTES).set(*queued_bytes as f64);
    }

    fn release(&self, bytes: usize) {
        if self.max_bytes == 0 {
            return;
        }

        let bytes = bytes.min(self.max_bytes);
        let mut queued_bytes = mutex_lock(&self.queued_bytes);
        *queued_bytes = queued_bytes.saturating_sub(bytes);
        gauge!(CHANNEL_STORAGE_WRITE_SER_QUEUE_BYTES).set(*queued_bytes as f64);
        self.cvar.notify_all();
    }
}

#[derive(Debug)]
struct PendingTrim {
    slots: Vec<Slot>,
    index_from: u64,
    index_to: u64,
}

impl PendingTrim {
    fn new(slots: Vec<Slot>, index_from: u64, index_to: u64) -> Self {
        Self {
            slots,
            index_from,
            index_to,
        }
    }

    fn merge(&mut self, slots: Vec<Slot>, index_from: u64, index_to: u64) {
        debug_assert!(index_from >= self.index_from);
        self.slots.extend(slots);
        self.index_to = self.index_to.max(index_to);
    }

    fn apply(&self, db: &DB, batch: &mut WriteBatch) {
        for slot in &self.slots {
            batch.delete_cf(
                Storage::cf_handle::<SlotIndex>(db),
                SlotIndex::encode(*slot),
            );
        }
        if self.index_to > self.index_from {
            batch.delete_range_cf(
                Storage::cf_handle::<MessageIndex>(db),
                MessageIndex::encode(self.index_from),
                MessageIndex::encode(self.index_to),
            );
        }
    }

    fn metrics(&self) -> AppliedTrimMetrics {
        AppliedTrimMetrics {
            index_from: self.index_from,
            index_to: self.index_to,
            slots_removed: self.slots.len(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct AppliedTrimMetrics {
    index_from: u64,
    index_to: u64,
    slots_removed: usize,
}

struct WriteEnvelope {
    index: Option<u64>,
    batch: WriteBatch,
    trim: Option<AppliedTrimMetrics>,
}

#[derive(Debug, Clone)]
pub struct Storage {
    db: Arc<DB>,
    write_tx: kanal::Sender<WriteRequest>,
    replay_queue: Arc<Mutex<ReplayQueue>>,
    queue_budget: Arc<QueueBudget>,
}

impl Storage {
    pub fn open(
        config: ConfigStorage,
        parser: MessageParserEncoding,
        shutdown: CancellationToken,
    ) -> anyhow::Result<(Self, SpawnedThreads)> {
        let db_options = Self::get_db_options(&config);
        let cf_descriptors = Self::cf_descriptors(&config);

        let db = Arc::new(
            DB::open_cf_descriptors(&db_options, &config.path, cf_descriptors)
                .with_context(|| format!("failed to open rocksdb with path: {:?}", config.path))?,
        );

        let (ser_tx, ser_rx) = kanal::unbounded();
        let (write_tx, write_rx) = kanal::bounded(1);
        let replay_queue = Arc::new(Mutex::new(ReplayQueue::new(config.replay_inflight_max)));
        let queue_budget = Arc::new(QueueBudget::new(config.max_ingest_queue_bytes));

        let storage = Self {
            db: Arc::clone(&db),
            write_tx: ser_tx,
            replay_queue: Arc::clone(&replay_queue),
            queue_budget: Arc::clone(&queue_budget),
        };

        let mut threads = vec![];
        let write_ser_jh = thread::Builder::new()
            .name("richatStrgSer".to_owned())
            .spawn({
                let db = Arc::clone(&db);
                let queue_budget = Arc::clone(&queue_budget);
                let ser_config = config.clone();
                let serialize_affinity = config.serialize_affinity.clone();
                move || {
                    if let Some(cpus) = serialize_affinity {
                        affinity_linux::set_thread_affinity(cpus.into_iter())
                            .expect("failed to set affinity");
                    }
                    Self::spawn_ser(db, ser_rx, write_tx, queue_budget, ser_config);
                    Ok(())
                }
            })?;
        threads.push(("richatStrgSer".to_owned(), Some(write_ser_jh)));
        let write_jh = thread::Builder::new()
            .name("richatStrgWrt".to_owned())
            .spawn({
                let db = Arc::clone(&db);
                let write_affinity = config.write_affinity.clone();
                move || {
                    if let Some(cpus) = write_affinity {
                        affinity_linux::set_thread_affinity(cpus.into_iter())
                            .expect("failed to set affinity");
                    }
                    Self::spawn_write(db, write_rx)
                }
            })?;
        threads.push(("richatStrgWrt".to_owned(), Some(write_jh)));
        for index in 0..config.replay_threads {
            let th_name = format!("richatStrgRep{index:02}");
            let jh = thread::Builder::new().name(th_name.clone()).spawn({
                let affinity = config.replay_affinity.clone();
                let db = Arc::clone(&db);
                let replay_queue = Arc::clone(&replay_queue);
                let replay_decode_per_tick = config.replay_decode_per_tick;
                let shutdown = shutdown.clone();
                move || {
                    if let Some(cpus) = affinity {
                        affinity_linux::set_thread_affinity(cpus.into_iter())
                            .expect("failed to set affinity");
                    }
                    Self::spawn_replay(db, replay_queue, parser, replay_decode_per_tick, shutdown)
                }
            })?;
            threads.push((th_name, Some(jh)));
        }

        Ok((storage, threads))
    }

    fn get_db_options(config: &ConfigStorage) -> Options {
        let mut options = Options::default();

        // Create if not exists
        options.create_if_missing(true);
        options.create_missing_column_families(true);

        // Set_max_background_jobs(N), configures N/4 low priority threads and 3N/4 high priority threads
        options.set_max_background_jobs(num_cpus::get() as i32);

        options.set_max_total_wal_size(config.rocksdb_max_total_wal_size as u64);

        options
    }

    fn get_message_cf_options(config: &ConfigStorage) -> Options {
        let mut options = Options::default();

        options.set_max_write_buffer_number(config.rocksdb_message_max_write_buffer_number as i32);
        options.set_write_buffer_size(config.rocksdb_message_write_buffer_size);
        options.set_level_zero_file_num_compaction_trigger(
            config.rocksdb_message_l0_file_num_compaction_trigger as i32,
        );
        options
            .set_max_bytes_for_level_base(config.rocksdb_message_max_bytes_for_level_base as u64);
        options.set_target_file_size_base(config.rocksdb_message_target_file_size_base as u64);
        options.set_max_subcompactions(config.rocksdb_message_max_subcompactions as u32);
        options.set_bytes_per_sync(config.rocksdb_message_bytes_per_sync as u64);
        options.set_wal_bytes_per_sync(config.rocksdb_message_wal_bytes_per_sync as u64);
        options.set_level_compaction_dynamic_level_bytes(true);

        let cold = DBCompressionType::from(config.messages_compression);
        let hot = DBCompressionType::from(config.get_messages_hot_compression());
        if cold == hot {
            options.set_compression_type(cold);
        } else {
            options.set_compression_per_level(&[hot, hot, hot, cold, cold, cold, cold]);
            options.set_bottommost_compression_type(cold);
        }

        options
    }

    fn get_slot_cf_options() -> Options {
        let mut options = Options::default();
        options.set_compression_type(DBCompressionType::None);
        options
    }

    fn cf_descriptors(config: &ConfigStorage) -> Vec<ColumnFamilyDescriptor> {
        vec![
            Self::cf_descriptor::<MessageIndex>(Self::get_message_cf_options(config)),
            Self::cf_descriptor::<SlotIndex>(Self::get_slot_cf_options()),
        ]
    }

    fn cf_descriptor<C: ColumnName>(options: Options) -> ColumnFamilyDescriptor {
        ColumnFamilyDescriptor::new(C::NAME, options)
    }

    fn cf_handle<C: ColumnName>(db: &DB) -> &ColumnFamily {
        db.cf_handle(C::NAME)
            .expect("should never get an unknown column")
    }

    fn spawn_ser(
        db: Arc<DB>,
        rx: kanal::Receiver<WriteRequest>,
        tx: kanal::Sender<WriteEnvelope>,
        queue_budget: Arc<QueueBudget>,
        config: ConfigStorage,
    ) {
        let mut gindex = 0;
        let mut buf = Vec::with_capacity(16 * 1024 * 1024);
        let mut batch = WriteBatch::new();
        let mut batch_started = None;
        let mut pending_trim = None;

        loop {
            if batch.size_in_bytes() >= config.max_write_batch_bytes {
                if !Self::flush_data_batch(&tx, &mut batch, &mut batch_started, gindex) {
                    return;
                }
                continue;
            }

            if batch.is_empty() {
                if pending_trim.is_some() {
                    if !Self::flush_trim_batch(&db, &tx, &mut pending_trim) {
                        return;
                    }
                    continue;
                }

                let request = match rx.recv() {
                    Ok(request) => request,
                    Err(_) => break,
                };
                queue_budget.release(request.queue_bytes());
                gauge!(CHANNEL_STORAGE_WRITE_SER_QUEUE_SIZE).set(rx.len() as f64);
                Self::apply_write_request(
                    &db,
                    request,
                    &mut buf,
                    &mut batch,
                    &mut pending_trim,
                    &mut gindex,
                    &mut batch_started,
                );
                gauge!(CHANNEL_STORAGE_WRITE_SER_BATCH_SIZE).set(batch.size_in_bytes() as f64);
                continue;
            }

            let batch_elapsed = batch_started
                .map(|started| started.elapsed())
                .unwrap_or_default();
            let timeout = config.max_write_batch_delay.saturating_sub(batch_elapsed);
            match rx.recv_timeout(timeout) {
                Ok(request) => {
                    queue_budget.release(request.queue_bytes());
                    gauge!(CHANNEL_STORAGE_WRITE_SER_QUEUE_SIZE).set(rx.len() as f64);
                    Self::apply_write_request(
                        &db,
                        request,
                        &mut buf,
                        &mut batch,
                        &mut pending_trim,
                        &mut gindex,
                        &mut batch_started,
                    );
                }
                Err(kanal::ReceiveErrorTimeout::Timeout) => {
                    if !Self::flush_data_batch(&tx, &mut batch, &mut batch_started, gindex) {
                        return;
                    }
                }
                Err(
                    kanal::ReceiveErrorTimeout::Closed | kanal::ReceiveErrorTimeout::SendClosed,
                ) => {
                    break;
                }
            }

            gauge!(CHANNEL_STORAGE_WRITE_SER_BATCH_SIZE).set(batch.size_in_bytes() as f64);
        }

        if !batch.is_empty() {
            let _ = Self::flush_data_batch(&tx, &mut batch, &mut batch_started, gindex);
        }
        if pending_trim.is_some() {
            let _ = Self::flush_trim_batch(&db, &tx, &mut pending_trim);
        }
    }

    fn spawn_write(db: Arc<DB>, rx: kanal::Receiver<WriteEnvelope>) -> anyhow::Result<()> {
        while let Ok(WriteEnvelope { index, batch, trim }) = rx.recv() {
            let batch_size = batch.size_in_bytes();
            let ts = QuantaInstant::now();
            db.write(batch)?;
            let elapsed = ts.elapsed();
            if let Some(index) = index {
                counter!(CHANNEL_STORAGE_WRITE_INDEX).absolute(index);
            }
            gauge!(CHANNEL_STORAGE_WRITE_DURATION_SECONDS).set(elapsed.as_secs_f64());
            gauge!(CHANNEL_STORAGE_WRITE_BATCH_SIZE).set(batch_size as f64);
            if let Some(trim) = trim {
                gauge!(CHANNEL_STORAGE_TRIM_FROM_INDEX).set(trim.index_from as f64);
                gauge!(CHANNEL_STORAGE_TRIM_TO_INDEX).set(trim.index_to as f64);
                gauge!(CHANNEL_STORAGE_TRIM_SLOTS).set(trim.slots_removed as f64);
            }

            // RocksDB properties (per column family — MessageIndex holds all the data)
            let cf = Self::cf_handle::<MessageIndex>(&db);
            for level in 0..=6 {
                if let Ok(Some(count)) =
                    db.property_int_value_cf(cf, &format!("rocksdb.num-files-at-level{level}"))
                {
                    gauge!(
                        CHANNEL_STORAGE_ROCKSDB_NUM_FILES_AT_LEVEL,
                        "level" => format!("{level}")
                    )
                    .set(count as f64);
                }
            }
            if let Ok(Some(v)) = db.property_int_value_cf(cf, "rocksdb.compaction-pending") {
                gauge!(CHANNEL_STORAGE_ROCKSDB_COMPACTION_PENDING).set(v as f64);
            }
            if let Ok(Some(v)) = db.property_int_value_cf(cf, "rocksdb.cur-size-all-mem-tables") {
                gauge!(CHANNEL_STORAGE_ROCKSDB_MEMTABLE_SIZE).set(v as f64);
            }
            if let Ok(Some(v)) = db.property_int_value_cf(cf, "rocksdb.live-sst-files-size") {
                gauge!(CHANNEL_STORAGE_ROCKSDB_LIVE_SST_FILES_SIZE).set(v as f64);
            }
            if let Ok(Some(v)) =
                db.property_int_value_cf(cf, "rocksdb.estimate-pending-compaction-bytes")
            {
                gauge!(CHANNEL_STORAGE_ROCKSDB_ESTIMATE_PENDING_COMPACTION_BYTES).set(v as f64);
            }
        }
        Ok(())
    }

    fn apply_write_request(
        db: &DB,
        request: WriteRequest,
        buf: &mut Vec<u8>,
        batch: &mut WriteBatch,
        pending_trim: &mut Option<PendingTrim>,
        gindex: &mut u64,
        batch_started: &mut Option<StdInstant>,
    ) {
        match request {
            WriteRequest::PushMessage {
                init,
                slot,
                head,
                index,
                message,
            } => {
                if batch.is_empty() {
                    *batch_started = Some(StdInstant::now());
                }

                if init {
                    buf.clear();
                    SlotIndexValue::encode(false, head, buf);
                    batch.put_cf(
                        Self::cf_handle::<SlotIndex>(db),
                        SlotIndex::encode(slot),
                        &*buf,
                    );
                }

                if let ParsedMessage::Slot(message) = &message {
                    if message.status() == SlotStatus::SlotFinalized {
                        buf.clear();
                        SlotIndexValue::encode(true, head, buf);
                        batch.put_cf(
                            Self::cf_handle::<SlotIndex>(db),
                            SlotIndex::encode(slot),
                            &*buf,
                        );
                    }
                }

                let message_ref: MessageRef = (&message).into();
                let message = FilteredUpdate {
                    filters: FilteredUpdateFilters::new(),
                    filtered_update: message_ref.into(),
                };
                buf.clear();
                MessageIndexValue::encode(slot, message, buf);
                batch.put_cf(
                    Self::cf_handle::<MessageIndex>(db),
                    MessageIndex::encode(index),
                    &*buf,
                );

                counter!(CHANNEL_STORAGE_WRITE_SER_INDEX).absolute(index);
                *gindex = index;
            }
            WriteRequest::TrimReplay {
                slots,
                index_from,
                index_to,
            } => {
                counter!(CHANNEL_STORAGE_TRIM_REQUESTS_TOTAL).increment(1);
                match pending_trim {
                    Some(pending) => {
                        pending.merge(slots, index_from, index_to);
                        counter!(CHANNEL_STORAGE_TRIM_REQUESTS_MERGED_TOTAL).increment(1);
                    }
                    None => *pending_trim = Some(PendingTrim::new(slots, index_from, index_to)),
                }
            }
        }
    }

    fn flush_data_batch(
        tx: &kanal::Sender<WriteEnvelope>,
        batch: &mut WriteBatch,
        batch_started: &mut Option<StdInstant>,
        index: u64,
    ) -> bool {
        if batch.is_empty() {
            *batch_started = None;
            gauge!(CHANNEL_STORAGE_WRITE_SER_BATCH_SIZE).set(0.0);
            return true;
        }

        let batch_to_send = mem::replace(batch, WriteBatch::new());
        *batch_started = None;
        gauge!(CHANNEL_STORAGE_WRITE_SER_BATCH_SIZE).set(0.0);
        tx.send(WriteEnvelope {
            index: Some(index),
            batch: batch_to_send,
            trim: None,
        })
        .is_ok()
    }

    fn flush_trim_batch(
        db: &DB,
        tx: &kanal::Sender<WriteEnvelope>,
        pending_trim: &mut Option<PendingTrim>,
    ) -> bool {
        let Some(trim) = pending_trim.take() else {
            return true;
        };

        let mut batch = WriteBatch::new();
        trim.apply(db, &mut batch);
        if batch.is_empty() {
            return true;
        }

        tx.send(WriteEnvelope {
            index: None,
            batch,
            trim: Some(trim.metrics()),
        })
        .is_ok()
    }

    fn spawn_replay(
        db: Arc<DB>,
        replay_queue: Arc<Mutex<ReplayQueue>>,
        parser: MessageParserEncoding,
        messages_decode_per_tick: usize,
        shutdown: CancellationToken,
    ) -> anyhow::Result<()> {
        let mut shutdown_ts = QuantaInstant::now();
        let mut prev_request = None;
        loop {
            // get request and lock state
            let Some(mut req) = ReplayQueue::pop_next(&replay_queue, prev_request.take()) else {
                if shutdown.is_cancelled() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
                continue;
            };
            let ts = QuantaInstant::now();
            if ts.duration_since(shutdown_ts) > Duration::from_millis(100) {
                shutdown_ts = ts;
                if shutdown.is_cancelled() {
                    break;
                }
            }
            let mut locked_state = req.client.state_lock();

            // drop request if stream is finished
            if locked_state.finished {
                ReplayQueue::drop_req(&replay_queue);
                continue;
            }

            // send error
            if let Some(error) = req.state.read_error.take() {
                locked_state.push_error(error);
                ReplayQueue::drop_req(&replay_queue);
                continue;
            }

            // check replay head
            let IndexLocation::Storage(head) = locked_state.head else {
                ReplayQueue::drop_req(&replay_queue);
                continue;
            };

            // check that filter is same
            let mut current_head = *req.state.head.get_or_insert(head);
            if current_head != head {
                req.state.messages.clear();
            }

            // filter messages
            let ts = QuantaInstant::now();
            while !locked_state.is_full_replay() {
                let Some((index, message)) = req.state.messages.pop_front() else {
                    break;
                };

                let filter = locked_state.filter.as_ref().expect("defined filter");
                let message_ref: MessageRef = (&message).into();
                let items = filter
                    .get_updates_ref(message_ref, CommitmentLevel::Processed)
                    .iter()
                    .map(|msg| ((&msg.filtered_update).into(), msg.encode_to_vec()))
                    .collect::<SmallVec<[(GrpcSubscribeMessage, Vec<u8>); 2]>>();

                for (message, data) in items {
                    locked_state.push_message(message, data);
                }

                current_head = index;
            }

            // update head
            locked_state.head = IndexLocation::Storage(current_head);
            req.state.head = Some(current_head);

            // check read_finished and empty; update index and drop from replay queue
            if req.state.read_finished && req.state.messages.is_empty() {
                if let Some(head) = req.messages.get_head_by_replay_index(current_head + 1) {
                    locked_state.head = IndexLocation::Memory(head);
                } else {
                    req.state.read_error = Some(Status::internal(
                        "failed to connect replay index to memory channel",
                    ));
                }

                req.metric_cpu_usage
                    .increment(duration_to_seconds(ts.elapsed()));
                ReplayQueue::drop_req(&replay_queue);
                continue;
            }

            // drop locked state before sync read
            drop(locked_state);

            // read messages
            if !req.state.read_finished && req.state.messages.len() < messages_decode_per_tick {
                let mut messages_decoded = 0;
                for item in db.iterator_cf(
                    Self::cf_handle::<MessageIndex>(&db),
                    IteratorMode::From(&MessageIndex::encode(current_head + 1), Direction::Forward),
                ) {
                    let item = match item {
                        Ok((key, value)) => {
                            match (
                                MessageIndex::decode(&key),
                                MessageIndexValue::decode(&value, parser),
                            ) {
                                (Ok(index), Ok((_slot, message))) => Ok((index, message.into())),
                                (Err(_error), _) => Err("failed to decode key"),
                                (_, Err(_error)) => Err("failed to parse message"),
                            }
                        }
                        Err(_error) => Err("failed to read message from the storage"),
                    };

                    match item {
                        Ok(item) => {
                            req.state.messages.push_back(item);
                            messages_decoded += 1;
                            if messages_decoded >= messages_decode_per_tick {
                                break;
                            }
                        }
                        Err(error) => {
                            req.state.read_error = Some(Status::internal(error));
                            break;
                        }
                    };
                }

                if messages_decoded < messages_decode_per_tick {
                    req.state.read_finished = true;
                }
            }

            req.metric_cpu_usage
                .increment(duration_to_seconds(ts.elapsed()));
            prev_request = Some(req);
        }
        ReplayQueue::shutdown(&replay_queue);
        Ok(())
    }

    pub fn push_message(
        &self,
        init: bool,
        slot: Slot,
        head: u64,
        index: u64,
        message: ParsedMessage,
    ) {
        let request = WriteRequest::PushMessage {
            init,
            slot,
            head,
            index,
            message,
        };
        let queue_bytes = request.queue_bytes();
        self.queue_budget.reserve(queue_bytes);
        if self.write_tx.send(request).is_err() {
            self.queue_budget.release(queue_bytes);
        }
    }

    pub fn trim_replay(&self, slots: Vec<Slot>, index_from: u64, index_to: u64) {
        let request = WriteRequest::TrimReplay {
            slots,
            index_from,
            index_to,
        };
        let queue_bytes = request.queue_bytes();
        self.queue_budget.reserve(queue_bytes);
        if self.write_tx.send(request).is_err() {
            self.queue_budget.release(queue_bytes);
        }
    }

    pub fn read_slots(&self) -> anyhow::Result<BTreeMap<Slot, SlotIndexValue>> {
        let mut slots = BTreeMap::new();
        for item in self
            .db
            .iterator_cf(Self::cf_handle::<SlotIndex>(&self.db), IteratorMode::Start)
        {
            let (key, value) = item.context("failed to read next row")?;
            slots.insert(
                SlotIndex::decode(&key).context("failed to decode key")?,
                SlotIndexValue::decode(&value).context("failed to decode value")?,
            );
        }
        Ok(slots)
    }

    pub fn read_messages_from_index(
        &self,
        index: u64,
        parser: MessageParserEncoding,
    ) -> impl Iterator<Item = anyhow::Result<(u64, ParsedMessage)>> + use<'_> {
        self.db
            .iterator_cf(
                Self::cf_handle::<MessageIndex>(&self.db),
                IteratorMode::From(&MessageIndex::encode(index), Direction::Forward),
            )
            .map(move |item| {
                let (key, value) = item.context("failed to read next row")?;
                let index = MessageIndex::decode(&key).context("failed to decode key")?;
                let (_slot, message) = MessageIndexValue::decode(&value, parser)?;
                Ok((index, message.into()))
            })
    }

    pub fn replay(
        &self,
        client: SubscribeClient,
        messages: Arc<SharedChannel>,
        metric_cpu_usage: Gauge,
    ) -> Result<(), &'static str> {
        ReplayQueue::push_new(
            &self.replay_queue,
            ReplayRequest {
                state: ReplayState::default(),
                client,
                messages,
                metric_cpu_usage,
            },
        )
        .map_err(|()| "replay queue is full; try again later")
    }
}

#[derive(Debug)]
enum WriteRequest {
    PushMessage {
        init: bool,
        slot: Slot,
        head: u64,
        index: u64,
        message: ParsedMessage,
    },
    TrimReplay {
        slots: Vec<Slot>,
        index_from: u64,
        index_to: u64,
    },
}

impl WriteRequest {
    fn queue_bytes(&self) -> usize {
        const BASE_OVERHEAD: usize = 128;

        match self {
            Self::PushMessage { message, .. } => BASE_OVERHEAD + message.size(),
            Self::TrimReplay { slots, .. } => BASE_OVERHEAD + slots.len() * mem::size_of::<Slot>(),
        }
    }
}

#[derive(Debug)]
struct ReplayRequest {
    state: ReplayState,
    client: SubscribeClient,
    messages: Arc<SharedChannel>,
    metric_cpu_usage: Gauge,
}

#[derive(Debug, Default)]
struct ReplayState {
    head: Option<u64>,
    messages: VecDeque<(u64, ParsedMessage)>,
    read_error: Option<Status>,
    read_finished: bool,
}

#[derive(Debug)]
struct ReplayQueue {
    capacity: usize,
    len: usize,
    requests: VecDeque<ReplayRequest>,
}

impl ReplayQueue {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            len: 0,
            requests: VecDeque::with_capacity(capacity),
        }
    }

    fn pop_next(queue: &Mutex<Self>, prev_request: Option<ReplayRequest>) -> Option<ReplayRequest> {
        let mut locked = mutex_lock(queue);
        if locked.len > 0 {
            if let Some(request) = prev_request {
                locked.requests.push_back(request);
            }
        }
        locked.requests.pop_front()
    }

    fn push_new(queue: &Mutex<Self>, request: ReplayRequest) -> Result<(), ()> {
        let mut locked = mutex_lock(queue);
        if locked.len < locked.capacity {
            locked.len += 1;
            locked.requests.push_back(request);
            Ok(())
        } else {
            Err(())
        }
    }

    fn drop_req(queue: &Mutex<Self>) {
        let mut locked = mutex_lock(queue);
        locked.len -= 1;
    }

    fn shutdown(queue: &Mutex<Self>) {
        let mut locked = mutex_lock(queue);
        locked.capacity = 0;
        locked.len = 0;
        locked.requests.clear();
    }
}
