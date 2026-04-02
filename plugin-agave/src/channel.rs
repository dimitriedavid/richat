// Based on https://github.com/tokio-rs/tokio/blob/master/tokio/src/sync/broadcast.rs
use {
    crate::{
        config::ConfigChannel,
        metrics,
        plugin::PluginNotification,
    },
    futures::stream::{Stream, StreamExt},
    log::debug,
    metrics_exporter_prometheus::PrometheusRecorder,
    richat_metrics::{MaybeRecorder, gauge},
    richat_proto::richat::RichatFilter,
    richat_shared::{
        mutex_lock,
        transports::{RecvError, RecvItem, RecvStream, Subscribe, SubscribeError},
    },
    solana_clock::Slot,
    std::{
        collections::BTreeMap,
        fmt,
        future::Future,
        pin::Pin,
        sync::{Arc, Mutex, MutexGuard},
        task::{Context, Poll, Waker},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationMask(u8);

impl NotificationMask {
    /// Slot and BlockMeta are always delivered — cannot be filtered by subscribers.
    pub const ALWAYS_ON: Self = Self(
        PluginNotification::Slot.bit() | PluginNotification::BlockMeta.bit(),
    );

    pub const fn for_notification(n: PluginNotification) -> Self {
        Self(n.bit())
    }

    pub const fn matches(self, n: PluginNotification) -> bool {
        self.0 & n.bit() != 0
    }
}

impl std::ops::BitOr for NotificationMask {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

pub struct SlotMeta {
    pub slot: Slot,
    pub status_i32: i32,
    pub confirmed: bool,
    pub finalized: bool,
}

struct PushMetrics {
    slot: Slot,
    /// 0=Processed, 1=Confirmed, 2=Rooted, 3=FirstShredReceived, 4=Completed, 5=CreatedBank, 6=Dead
    /// None means this message is not a Slot message
    slot_status: Option<i32>,
    is_dead: bool,
    is_processed: bool,
    tail: u64,
    head: u64,
    slots_len: usize,
    bytes_total: usize,
}

#[derive(Debug, Clone)]
pub struct Sender {
    shared: Arc<Shared>,
    recorder: Arc<MaybeRecorder<PrometheusRecorder>>,
}

impl Sender {
    pub fn new(config: ConfigChannel, recorder: Arc<MaybeRecorder<PrometheusRecorder>>) -> Self {
        let max_messages = config.max_messages.next_power_of_two();
        let mut buffer = Vec::with_capacity(max_messages);
        for i in 0..max_messages {
            buffer.push(Mutex::new(Item {
                pos: i as u64,
                slot: 0,
                data: None,
                closed: false,
            }));
        }

        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                head: max_messages as u64 + 1,
                tail: max_messages as u64,
                slots: BTreeMap::new(),
                bytes_total: 0,
                bytes_max: config.max_bytes,
                wakers: Vec::with_capacity(16),
            }),
            mask: (max_messages - 1) as u64,
            buffer: buffer.into_boxed_slice(),
        });

        Self { shared, recorder }
    }

    pub fn push_encoded(
        &self,
        notification: PluginNotification,
        slot_meta: SlotMeta,
        data: std::sync::Arc<Vec<u8>>,
    ) {
        let mut state = self.shared.state_lock();
        let metrics = self.push_msg_encoded(&mut state, notification, slot_meta, data);
        let mut to_wake: Vec<Waker> = Vec::new();
        state.wakers.retain(|(mask, waker)| {
            if mask.matches(notification) {
                to_wake.push(waker.clone());
                false
            } else {
                true
            }
        });
        drop(state);
        self.update_metrics(metrics);
        for waker in to_wake {
            waker.wake();
        }
    }

    fn push_msg_encoded(
        &self,
        state: &mut std::sync::MutexGuard<'_, State>,
        notification: PluginNotification,
        slot_meta: SlotMeta,
        data: std::sync::Arc<Vec<u8>>,
    ) -> PushMetrics {
        let mut removed_max_slot = None;

        // bump current tail
        state.tail = state.tail.wrapping_add(1);

        // update slots info
        let head = state.tail;
        let entry = state.slots.entry(slot_meta.slot).or_insert_with(|| SlotInfo {
            head,
            confirmed: false,
            finalized: false,
        });
        if slot_meta.confirmed {
            entry.confirmed = true;
        }
        if slot_meta.finalized {
            entry.finalized = true;
        }

        // lock and update item
        state.bytes_total += data.len();
        let idx = self.shared.get_idx(state.tail);
        let mut item = self.shared.buffer_idx(idx);
        if let Some(evicted) = item.data.take() {
            state.head = state.head.wrapping_add(1);
            state.bytes_total -= evicted.1.len();
            removed_max_slot = Some(item.slot);
        }
        item.pos = state.tail;
        item.slot = slot_meta.slot;
        item.data = Some((notification, data));
        drop(item);

        // drop extra messages by max bytes
        while state.bytes_total >= state.bytes_max && state.head < state.tail {
            let idx = self.shared.get_idx(state.head);
            let mut item = self.shared.buffer_idx(idx);
            let Some(evicted) = item.data.take() else {
                panic!("nothing to remove to keep bytes under limit")
            };
            state.head = state.head.wrapping_add(1);
            state.bytes_total -= evicted.1.len();
            removed_max_slot = Some(match removed_max_slot {
                Some(s) => item.slot.max(s),
                None => item.slot,
            });
        }

        // remove not-complete slots
        if let Some(remove_upto) = removed_max_slot {
            loop {
                match state.slots.first_key_value() {
                    Some((s, _)) if *s <= remove_upto => {
                        let s = *s;
                        state.slots.remove(&s);
                    }
                    _ => break,
                }
            }
        }

        let is_slot = matches!(notification, PluginNotification::Slot);
        let is_dead = is_slot && slot_meta.status_i32 == 6;
        let is_processed = is_slot && slot_meta.status_i32 == 0;
        PushMetrics {
            slot: slot_meta.slot,
            slot_status: if is_slot { Some(slot_meta.status_i32) } else { None },
            is_dead,
            is_processed,
            tail: state.tail,
            head: state.head,
            slots_len: state.slots.len(),
            bytes_total: state.bytes_total,
        }
    }

    fn update_metrics(&self, m: PushMetrics) {
        if let Some(status_i32) = m.slot_status {
            let status_str = match status_i32 {
                0 => "processed",
                1 => "confirmed",
                2 => "rooted",
                3 => "first_shred_received",
                4 => "completed",
                5 => "created_bank",
                _ => "dead",
            };
            if !m.is_dead {
                gauge!(&self.recorder, metrics::GEYSER_SLOT_STATUS, "status" => status_str)
                    .set(m.slot as f64);
            }
            if m.is_processed {
                debug!(
                    "new processed {} / {} messages / {} slots / {} bytes",
                    m.slot,
                    m.tail - m.head,
                    m.slots_len,
                    m.bytes_total
                );
                gauge!(&self.recorder, metrics::CHANNEL_MESSAGES_TOTAL)
                    .set((m.tail - m.head) as f64);
                gauge!(&self.recorder, metrics::CHANNEL_SLOTS_TOTAL).set(m.slots_len as f64);
                gauge!(&self.recorder, metrics::CHANNEL_BYTES_TOTAL).set(m.bytes_total as f64);
            }
        }
    }

    pub fn close(&self) {
        for idx in 0..self.shared.buffer.len() {
            self.shared.buffer_idx(idx).closed = true;
        }

        let mut state = self.shared.state_lock();
        for (_, waker) in state.wakers.drain(..) {
            waker.wake();
        }
    }
}

impl Subscribe for Sender {
    fn subscribe(
        &self,
        replay_from_slot: Option<Slot>,
        filter: Option<RichatFilter>,
    ) -> Result<RecvStream, SubscribeError> {
        let shared = Arc::clone(&self.shared);

        let state = shared.state_lock();
        let next = match replay_from_slot {
            Some(slot) => state.slots.get(&slot).map(|s| s.head).ok_or_else(|| {
                match state.slots.first_key_value() {
                    Some((key, _value)) => SubscribeError::SlotNotAvailable {
                        first_available: *key,
                    },
                    None => SubscribeError::NotInitialized,
                }
            })?,
            None => state.tail,
        };
        drop(state);

        let filter = filter.unwrap_or_default();

        Ok(Receiver {
            shared,
            next,
            finished: false,
            enable_notifications_accounts: !filter.disable_accounts,
            enable_notifications_transactions: !filter.disable_transactions,
            enable_notifications_entries: !filter.disable_entries,
        }
        .boxed())
    }
}

#[derive(Debug)]
pub struct Receiver {
    shared: Arc<Shared>,
    next: u64,
    finished: bool,
    enable_notifications_accounts: bool,
    enable_notifications_transactions: bool,
    enable_notifications_entries: bool,
}

impl Receiver {
    fn notification_mask(&self) -> NotificationMask {
        let mut mask = NotificationMask::ALWAYS_ON;
        if self.enable_notifications_accounts {
            mask = mask | NotificationMask::for_notification(PluginNotification::Account);
        }
        if self.enable_notifications_transactions {
            mask = mask | NotificationMask::for_notification(PluginNotification::Transaction);
        }
        if self.enable_notifications_entries {
            mask = mask | NotificationMask::for_notification(PluginNotification::Entry);
        }
        mask
    }

    pub async fn recv(&mut self) -> Result<RecvItem, RecvError> {
        Recv::new(self).await
    }

    pub fn recv_ref(&mut self, waker: &Waker) -> Result<Option<RecvItem>, RecvError> {
        loop {
            // read item with next value
            let idx = self.shared.get_idx(self.next);
            let mut item = self.shared.buffer_idx(idx);
            if item.closed {
                return Err(RecvError::Closed);
            }

            if item.pos != self.next {
                // release lock before attempting to acquire state
                drop(item);

                // acquire state to store waker
                let mut state = self.shared.state_lock();

                // make sure that position did not changed
                item = self.shared.buffer_idx(idx);
                if item.closed {
                    return Err(RecvError::Closed);
                }
                if item.pos != self.next {
                    return if item.pos < self.next {
                        state.wakers.push((self.notification_mask(), waker.clone()));
                        Ok(None)
                    } else {
                        Err(RecvError::Lagged)
                    };
                }
            }

            self.next = self.next.wrapping_add(1);
            let (plugin_notification, item) = item.data.clone().ok_or(RecvError::Lagged)?;
            match plugin_notification {
                PluginNotification::Account if !self.enable_notifications_accounts => continue,
                PluginNotification::Transaction if !self.enable_notifications_transactions => {
                    continue;
                }
                PluginNotification::Entry if !self.enable_notifications_entries => continue,
                _ => {}
            }
            break Ok(Some(item));
        }
    }
}

struct Recv<'a> {
    receiver: &'a mut Receiver,
}

impl<'a> Recv<'a> {
    const fn new(receiver: &'a mut Receiver) -> Self {
        Self { receiver }
    }
}

impl Future for Recv<'_> {
    type Output = Result<RecvItem, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        let receiver: &mut Receiver = me.receiver;

        match receiver.recv_ref(cx.waker()) {
            Ok(Some(value)) => Poll::Ready(Ok(value)),
            Ok(None) => Poll::Pending,
            Err(error) => Poll::Ready(Err(error)),
        }
    }
}

impl Stream for Receiver {
    type Item = Result<RecvItem, RecvError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let me = self.get_mut();
        if me.finished {
            return Poll::Ready(None);
        }

        match me.recv_ref(cx.waker()) {
            Ok(Some(value)) => Poll::Ready(Some(Ok(value))),
            Ok(None) => Poll::Pending,
            Err(error) => {
                me.finished = true;
                Poll::Ready(Some(Err(error)))
            }
        }
    }
}

struct Shared {
    state: Mutex<State>,
    mask: u64,
    buffer: Box<[Mutex<Item>]>,
}

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared").field("mask", &self.mask).finish()
    }
}

impl Shared {
    #[inline]
    const fn get_idx(&self, pos: u64) -> usize {
        (pos & self.mask) as usize
    }

    #[inline]
    fn state_lock(&self) -> MutexGuard<'_, State> {
        mutex_lock(&self.state)
    }

    #[inline]
    fn buffer_idx(&self, idx: usize) -> MutexGuard<'_, Item> {
        mutex_lock(&self.buffer[idx])
    }
}

struct State {
    head: u64,
    tail: u64,
    slots: BTreeMap<Slot, SlotInfo>,
    bytes_total: usize,
    bytes_max: usize,
    wakers: Vec<(NotificationMask, Waker)>,
}

struct SlotInfo {
    head: u64,
    confirmed: bool,
    finalized: bool,
}

struct Item {
    pos: u64,
    slot: Slot,
    data: Option<(PluginNotification, RecvItem)>,
    closed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_mask_matches_own_bit() {
        for n in [
            PluginNotification::Slot,
            PluginNotification::Account,
            PluginNotification::Transaction,
            PluginNotification::Entry,
            PluginNotification::BlockMeta,
        ] {
            let mask = NotificationMask::for_notification(n);
            assert!(mask.matches(n), "{n:?} mask should match itself");
        }
    }

    #[test]
    fn notification_mask_does_not_match_other_bits() {
        let txn_only = NotificationMask::for_notification(PluginNotification::Transaction);
        assert!(!txn_only.matches(PluginNotification::Account));
        assert!(!txn_only.matches(PluginNotification::Slot));
        assert!(!txn_only.matches(PluginNotification::Entry));
        assert!(!txn_only.matches(PluginNotification::BlockMeta));
    }

    #[test]
    fn notification_mask_combined() {
        let mask = NotificationMask::ALWAYS_ON
            | NotificationMask::for_notification(PluginNotification::Transaction);
        assert!(mask.matches(PluginNotification::Slot));
        assert!(mask.matches(PluginNotification::BlockMeta));
        assert!(mask.matches(PluginNotification::Transaction));
        assert!(!mask.matches(PluginNotification::Account));
        assert!(!mask.matches(PluginNotification::Entry));
    }
}
