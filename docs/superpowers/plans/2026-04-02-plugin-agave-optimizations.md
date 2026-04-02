# plugin-agave Performance Optimizations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move protobuf encoding off the validator thread, remove the raw encoder, simplify the channel's state lock footprint, and add selective waker wakeup.

**Architecture:** Validator callbacks clone borrowed Geyser data into owned prost `UpdateOneof` structs and send them via a `tokio::sync::mpsc::unbounded_channel` to a single background Tokio encoder task. The encoder calls `SubscribeUpdate::encode_to_vec()` (single-pass) and pushes the result into the ring buffer. The raw encoder and its entire `protobuf/` module are deleted. Wakers are tagged with a `NotificationMask` bitmask so only subscribers interested in a given message type are woken on each push.

**Tech Stack:** Rust, prost (`encode_to_vec`), `tokio::sync::mpsc`, `richat_proto::geyser` prost types, `richat_proto::convert_to`

**Spec:** `docs/superpowers/specs/2026-04-02-plugin-agave-optimizations-design.md`

---

## File Map

| File | Action | What changes |
|------|--------|-------------|
| `plugin-agave/src/channel.rs` | Modify | Remove missed slot check, `SlotInfo.parent_slot`, add `NotificationMask`, change wakers vec, defer metrics outside lock, change `Sender::push` signature |
| `plugin-agave/src/plugin.rs` | Modify | Add `OwnedUpdate`, `encoder_task`, update `PluginInner`, rewrite all 5 geyser callbacks |
| `plugin-agave/src/config.rs` | Modify | Remove `ConfigChannel.encoder` field and `deserialize_encoder` |
| `plugin-agave/src/metrics.rs` | Modify | Remove `GEYSER_MISSED_SLOT_STATUS` constant and its `describe_counter!` call |
| `plugin-agave/src/lib.rs` | Modify | Remove `pub mod protobuf` |
| `plugin-agave/src/protobuf/` | Delete | Entire module (message.rs, mod.rs, encoding/) |
| `plugin-agave/fuzz/` | Delete | All fuzz targets |
| `plugin-agave/Cargo.toml` | Modify | Add `sync` to tokio features; remove unused deps after deletion |

---

## Task 1: Remove missed slot check and simplify `push()`

**Files:**
- Modify: `plugin-agave/src/channel.rs`
- Modify: `plugin-agave/Cargo.toml`

- [ ] **Step 1: Delete the missed slot logic from `Sender::push`**

Replace the entire body of `Sender::push` (lines 66–126 in `channel.rs`) with:

```rust
pub fn push(&self, message: ProtobufMessage, encoder: ProtobufEncoder) {
    // encode message
    let data = message.encode(encoder);

    // acquire state lock
    let mut state = self.shared.state_lock();

    self.push_msg(&mut state, message, data);

    // notify receivers
    for waker in state.wakers.drain(..) {
        waker.wake();
    }
}
```

(We keep the old `ProtobufMessage` / `ProtobufEncoder` signature for now — those get replaced in Task 4.)

- [ ] **Step 2: Remove `slot_status` variable and `SlotInfo.parent_slot` field**

In `push_msg` (`channel.rs:128`), remove the block that sets `entry.parent_slot`:

```rust
// DELETE this block inside push_msg:
if let ProtobufMessage::Slot { parent, status, .. } = &message {
    if let Some(parent) = parent {
        entry.parent_slot = Some(*parent);   // DELETE
    }
    ...
}
```

Remove `parent_slot` from the `SlotInfo` struct:

```rust
struct SlotInfo {
    head: u64,
    // parent_slot: Option<Slot>,   ← DELETE this line
    confirmed: bool,
    finalized: bool,
}
```

And remove the `or_insert_with` initialiser's `parent_slot` field:

```rust
let entry = state.slots.entry(slot).or_insert_with(|| SlotInfo {
    head,
    // parent_slot: None,   ← DELETE
    confirmed: false,
    finalized: false,
});
```

- [ ] **Step 3: Remove `smallvec` from imports in `channel.rs`**

Delete the `smallvec::SmallVec` import line from the `use { ... }` block at the top of `channel.rs`.

- [ ] **Step 4: Remove `smallvec` from `Cargo.toml`**

In `plugin-agave/Cargo.toml`, delete:

```toml
smallvec = { workspace = true }
```

- [ ] **Step 5: Verify compilation and tests pass**

```bash
cargo test -p richat-plugin-agave
```

Expected: `test result: ok. 5 passed`

- [ ] **Step 6: Commit**

```bash
git add plugin-agave/src/channel.rs plugin-agave/Cargo.toml
git commit -m "refactor(plugin-agave): remove missed slot check and parent chain traversal"
```

---

## Task 2: Defer metrics writes outside the state lock

**Files:**
- Modify: `plugin-agave/src/channel.rs`

- [ ] **Step 1: Add `PushMetrics` struct**

Add this struct near the top of `channel.rs` (after the imports):

```rust
struct PushMetrics {
    slot: Slot,
    status: Option<SlotStatus>,  // None for non-Slot messages; SlotStatus is Copy via &SlotStatus deref
    tail: u64,
    head: u64,
    slots_len: usize,
    bytes_total: usize,
}
```

Because `SlotStatus` isn't `Copy`, store only what's needed for the two metrics calls. Looking at the existing metrics code (lines 198–216), the slot-status gauge needs the `SlotStatus` value and the processed-slot gauge needs `tail - head`, `slots_len`, `bytes_total`. Capture them as integers:

```rust
struct PushMetrics {
    slot: Slot,
    /// 0=Processed,1=Confirmed,2=Rooted,3=FirstShredReceived,4=Completed,5=CreatedBank,6=Dead
    /// None means this message is not a Slot message
    slot_status: Option<i32>,
    is_dead: bool,
    is_processed: bool,
    tail: u64,
    head: u64,
    slots_len: usize,
    bytes_total: usize,
}
```

- [ ] **Step 2: Modify `push_msg` to return `PushMetrics` instead of writing metrics**

Change the signature of `push_msg`:

```rust
fn push_msg(
    &self,
    state: &mut MutexGuard<'_, State>,
    message: ProtobufMessage,
    data: Vec<u8>,
) -> PushMetrics {
```

Replace the metrics block at the end of `push_msg` (lines 198–216) with constructing and returning a `PushMetrics`:

```rust
// replace the entire `if let ProtobufMessage::Slot { status, .. } = message { ... }` block with:
let (slot_status, is_dead, is_processed) = if let ProtobufMessage::Slot { status, .. } = &message {
    let status_i32 = match **status {
        SlotStatus::Processed => 0,
        SlotStatus::Confirmed => 1,
        SlotStatus::Rooted => 2,
        SlotStatus::FirstShredReceived => 3,
        SlotStatus::Completed => 4,
        SlotStatus::CreatedBank => 5,
        SlotStatus::Dead(_) => 6,
    };
    let is_dead = matches!(**status, SlotStatus::Dead(_));
    let is_processed = matches!(**status, SlotStatus::Processed);
    (Some(status_i32), is_dead, is_processed)
} else {
    (None, false, false)
};

PushMetrics {
    slot,
    slot_status,
    is_dead,
    is_processed,
    tail: state.tail,
    head: state.head,
    slots_len: state.slots.len(),
    bytes_total: state.bytes_total,
}
```

- [ ] **Step 3: Add `update_metrics` helper on `Sender`**

```rust
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
```

- [ ] **Step 4: Update `Sender::push` to call `push_msg` then `update_metrics` after dropping the lock**

```rust
pub fn push(&self, message: ProtobufMessage, encoder: ProtobufEncoder) {
    let data = message.encode(encoder);
    let mut state = self.shared.state_lock();
    let metrics = self.push_msg(&mut state, message, data);
    // notify receivers
    for waker in state.wakers.drain(..) {
        waker.wake();
    }
    drop(state);
    self.update_metrics(metrics);
}
```

- [ ] **Step 5: Add `SlotStatus` import to `channel.rs` if not already present**

The `SlotStatus` is already imported at the top of `channel.rs`:
```rust
agave_geyser_plugin_interface::geyser_plugin_interface::SlotStatus,
```
Verify it's there; add it if missing.

- [ ] **Step 6: Verify compilation and tests pass**

```bash
cargo test -p richat-plugin-agave
```

Expected: `test result: ok. 5 passed`

- [ ] **Step 7: Commit**

```bash
git add plugin-agave/src/channel.rs
git commit -m "perf(plugin-agave): defer metrics writes outside the state lock"
```

---

## Task 3: Add `NotificationMask` and selective waker wakeup

**Files:**
- Modify: `plugin-agave/src/plugin.rs`
- Modify: `plugin-agave/src/channel.rs`

- [ ] **Step 1: Write failing tests for `NotificationMask`**

Add to the bottom of `plugin-agave/src/channel.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p richat-plugin-agave notification_mask
```

Expected: compile error — `NotificationMask` does not exist yet.

- [ ] **Step 3: Add `bit()` to `PluginNotification` in `plugin.rs`**

In `plugin.rs`, extend the `PluginNotification` impl:

```rust
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
```

- [ ] **Step 4: Add `NotificationMask` to `channel.rs`**

Add after the imports in `channel.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationMask(u8);

impl NotificationMask {
    /// Slot and BlockMeta are always delivered; they cannot be filtered by subscribers.
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
```

- [ ] **Step 5: Change `State.wakers` to `Vec<(NotificationMask, Waker)>`**

In the `State` struct in `channel.rs`:

```rust
struct State {
    head: u64,
    tail: u64,
    slots: BTreeMap<Slot, SlotInfo>,
    bytes_total: usize,
    bytes_max: usize,
    wakers: Vec<(NotificationMask, Waker)>,  // was: Vec<Waker>
}
```

Update the initial capacity in `Sender::new`:

```rust
wakers: Vec::with_capacity(16),
```

(No type annotation needed — the field type drives inference.)

- [ ] **Step 6: Update waker registration in `Receiver::recv_ref`**

In `recv_ref`, the call that pushes the waker (currently `state.wakers.push(waker.clone())`) needs to include a mask computed from the receiver's filters.

In the `Receiver` struct, add a helper:

```rust
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
}
```

Then in `recv_ref`, replace:

```rust
state.wakers.push(waker.clone());
```

with:

```rust
state.wakers.push((self.notification_mask(), waker.clone()));
```

- [ ] **Step 7: Update waker drain in `Sender::push` to only wake matching**

In `Sender::push`, replace:

```rust
for waker in state.wakers.drain(..) {
    waker.wake();
}
```

with (drain matching wakers inside lock, wake outside lock):

```rust
let notification = PluginNotification::from(&message);
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
return; // early return since we already dropped state above
```

Full updated `Sender::push`:

```rust
pub fn push(&self, message: ProtobufMessage, encoder: ProtobufEncoder) {
    let data = message.encode(encoder);
    let notification = PluginNotification::from(&message);
    let mut state = self.shared.state_lock();
    let metrics = self.push_msg(&mut state, message, data);
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
```

Also update `Sender::close` which also drains wakers — wake all on close:

```rust
pub fn close(&self) {
    for idx in 0..self.shared.buffer.len() {
        self.shared.buffer_idx(idx).closed = true;
    }
    let mut state = self.shared.state_lock();
    for (_, waker) in state.wakers.drain(..) {
        waker.wake();
    }
}
```

- [ ] **Step 8: Run the failing tests — they should now pass**

```bash
cargo test -p richat-plugin-agave notification_mask
```

Expected: `test result: ok. 3 passed`

- [ ] **Step 9: Run all tests**

```bash
cargo test -p richat-plugin-agave
```

Expected: `test result: ok. 8 passed` (5 encoding tests + 3 new mask tests)

- [ ] **Step 10: Commit**

```bash
git add plugin-agave/src/plugin.rs plugin-agave/src/channel.rs
git commit -m "perf(plugin-agave): add NotificationMask and selective waker wakeup"
```

---

## Task 4: Add `OwnedUpdate`, `encoder_task`, update `PluginInner`, and rewrite callbacks

This is the largest task — all changes land together so the crate compiles at the end.

**Files:**
- Modify: `plugin-agave/src/plugin.rs`
- Modify: `plugin-agave/src/channel.rs`
- Modify: `plugin-agave/Cargo.toml`

- [ ] **Step 1: Add `sync` to tokio features in `Cargo.toml`**

In `plugin-agave/Cargo.toml`, change:

```toml
tokio = { workspace = true, features = ["rt-multi-thread", "macros"] }
```

to:

```toml
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "sync"] }
```

- [ ] **Step 2: Add `OwnedUpdate` struct to `plugin.rs`**

Add after the `PluginNotification` definition in `plugin.rs`:

```rust
use richat_proto::geyser::subscribe_update::UpdateOneof;
use std::time::SystemTime;

pub struct OwnedUpdate {
    pub notification: PluginNotification,
    pub created_at: SystemTime,
    pub payload: UpdateOneof,
}
```

- [ ] **Step 3: Add `encoder_task` function to `plugin.rs`**

Add this function before `PluginInner`:

```rust
async fn encoder_task(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<OwnedUpdate>,
    sender: Sender,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use {prost::Message, prost_types::Timestamp, richat_proto::geyser::SubscribeUpdate};

    loop {
        tokio::select! {
            biased;
            Some(owned) = rx.recv() => {
                let bytes = SubscribeUpdate {
                    filters: vec![],
                    update_oneof: Some(owned.payload),
                    created_at: Some(Timestamp::from(owned.created_at)),
                }
                .encode_to_vec();
                sender.push_encoded(owned.notification, std::sync::Arc::new(bytes));
            }
            _ = shutdown.cancelled() => break,
        }
    }
}
```

- [ ] **Step 4: Add `Sender::push_encoded` to `channel.rs`**

Add a new `push_encoded` method alongside the existing `push` on `Sender`:

```rust
pub fn push_encoded(&self, notification: PluginNotification, data: std::sync::Arc<Vec<u8>>) {
    let slot = 0u64; // placeholder — extracted below from the item position
    // We need a slot for the slot map. Since we're post-encoding we don't have a
    // ProtobufMessage anymore. Pass notification + data; slot is 0 for non-slot messages.
    // ...
}
```

Wait — `push_msg` needs a `slot` value to maintain the `state.slots` BTreeMap. With the new design, we no longer have a `ProtobufMessage` when calling `push_encoded`. We need to pass the slot too.

Update the signature:

```rust
pub fn push_encoded(&self, notification: PluginNotification, slot: Slot, data: Arc<Vec<u8>>) {
    let mut state = self.shared.state_lock();
    let metrics = self.push_msg_encoded(&mut state, notification, slot, data);
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
```

And add `push_msg_encoded` — a variant of `push_msg` that takes the already-encoded `Arc<Vec<u8>>` instead of raw `Vec<u8>` wrapped in `Arc` only after:

Actually, let's keep this simple. Rename the internal logic. The key insight: `push_msg` today takes `(state, message: ProtobufMessage, data: Vec<u8>)` where `message` is used for:
1. Getting the slot (`message.get_slot()`)
2. Updating `SlotInfo` confirmed/finalized fields
3. Building `PushMetrics`

After the refactor, we need to pass these separately. Add `OwnedUpdate` to carry the slot as well:

Update `OwnedUpdate` to include `slot`:

```rust
pub struct OwnedUpdate {
    pub notification: PluginNotification,
    pub created_at: SystemTime,
    pub slot: Slot,
    pub payload: UpdateOneof,
    /// Only Some for Slot messages
    pub slot_confirmed: bool,
    pub slot_finalized: bool,
}
```

This lets `push_msg_encoded` update the SlotInfo correctly.

Then `push_msg_encoded` signature:

```rust
fn push_msg_encoded(
    &self,
    state: &mut MutexGuard<'_, State>,
    notification: PluginNotification,
    slot: Slot,
    slot_confirmed: bool,
    slot_finalized: bool,
    data: Arc<Vec<u8>>,
) -> PushMetrics
```

And `push_encoded`:

```rust
pub fn push_encoded(&self, owned: &OwnedUpdate, data: Arc<Vec<u8>>) {
    let mut state = self.shared.state_lock();
    let metrics = self.push_msg_encoded(
        &mut state,
        owned.notification,
        owned.slot,
        owned.slot_confirmed,
        owned.slot_finalized,
        data,
    );
    let mut to_wake: Vec<Waker> = Vec::new();
    state.wakers.retain(|(mask, waker)| {
        if mask.matches(owned.notification) {
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
```

Now add `push_msg_encoded` to `Sender` in `channel.rs`. It mirrors `push_msg` but takes `Arc<Vec<u8>>` and the extracted slot metadata instead of `ProtobufMessage`:

```rust
fn push_msg_encoded(
    &self,
    state: &mut MutexGuard<'_, State>,
    notification: PluginNotification,
    slot: Slot,
    slot_confirmed: bool,
    slot_finalized: bool,
    data: Arc<Vec<u8>>,
) -> PushMetrics {
    let mut removed_max_slot = None;

    // bump current tail
    state.tail = state.tail.wrapping_add(1);

    // update slots info
    let head = state.tail;
    let entry = state.slots.entry(slot).or_insert_with(|| SlotInfo {
        head,
        confirmed: false,
        finalized: false,
    });
    if slot_confirmed {
        entry.confirmed = true;
    }
    if slot_finalized {
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
    item.slot = slot;
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
    PushMetrics {
        slot,
        slot_status: if is_slot {
            // We encode status in OwnedUpdate; for metrics we reuse the slot_confirmed/finalized flags
            // Map back to the integer used by update_metrics:
            // This is fine for display purposes — exact value set by caller
            Some(if slot_finalized { 2 } else if slot_confirmed { 1 } else { 0 })
        } else {
            None
        },
        is_dead: false,
        is_processed: is_slot && !slot_confirmed && !slot_finalized,
        tail: state.tail,
        head: state.head,
        slots_len: state.slots.len(),
        bytes_total: state.bytes_total,
    }
}
```

**Wait** — there's a problem. `PushMetrics` for slot status needs the actual slot status integer (Processed/Confirmed/Rooted/Dead/etc.), not just confirmed/finalized flags. The `GEYSER_SLOT_STATUS` gauge uses the status string (processed/confirmed/rooted/dead/...).

We need to pass the full status through `OwnedUpdate`. Let's store it as an `i32`:

```rust
pub struct OwnedUpdate {
    pub notification: PluginNotification,
    pub created_at: SystemTime,
    pub slot: Slot,
    pub payload: UpdateOneof,
    // Only meaningful for Slot notifications:
    pub slot_status_i32: i32,   // 0=Processed,1=Confirmed,2=Rooted,3=FirstShredReceived,4=Completed,5=CreatedBank,6=Dead
    pub slot_confirmed: bool,
    pub slot_finalized: bool,
}
```

For non-Slot messages, `slot_status_i32 = 0`, `slot_confirmed = false`, `slot_finalized = false`.

And `PushMetrics` uses `owned.slot_status_i32` when `is_slot`.

With this, the full `OwnedUpdate` is defined and `push_msg_encoded` has all it needs.

Let's finalize the full `OwnedUpdate`:

```rust
pub struct OwnedUpdate {
    pub notification: PluginNotification,
    pub created_at: SystemTime,
    pub slot: Slot,
    pub payload: UpdateOneof,
    /// Only meaningful for PluginNotification::Slot:
    pub slot_status_i32: i32,
    pub slot_confirmed: bool,
    pub slot_finalized: bool,
}
```

- [ ] **Step 5: Implement all the above in `channel.rs` and `plugin.rs`**

Here is the complete updated `OwnedUpdate` definition in `plugin.rs`:

```rust
use {
    richat_proto::geyser::subscribe_update::UpdateOneof,
    solana_clock::Slot,
    std::time::SystemTime,
};

pub struct OwnedUpdate {
    pub notification: PluginNotification,
    pub created_at: SystemTime,
    pub slot: Slot,
    pub payload: UpdateOneof,
    /// Only meaningful when notification == Slot
    pub slot_status_i32: i32,
    pub slot_confirmed: bool,
    pub slot_finalized: bool,
}
```

Updated `encoder_task` in `plugin.rs`:

```rust
async fn encoder_task(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<OwnedUpdate>,
    sender: Sender,
    shutdown: tokio_util::sync::CancellationToken,
) {
    use {
        prost::Message,
        prost_types::Timestamp,
        richat_proto::geyser::SubscribeUpdate,
        std::sync::Arc,
    };

    loop {
        tokio::select! {
            biased;
            Some(owned) = rx.recv() => {
                let bytes = SubscribeUpdate {
                    filters: vec![],
                    update_oneof: Some(owned.payload),
                    created_at: Some(Timestamp::from(owned.created_at)),
                }
                .encode_to_vec();
                sender.push_encoded(&owned, Arc::new(bytes));
            }
            _ = shutdown.cancelled() => break,
        }
    }
}
```

Note: `owned.payload` is moved into `SubscribeUpdate`, but we still need `owned.slot`, `owned.notification`, etc. in `push_encoded`. We must pass those before consuming `payload`. Use a split:

```rust
Some(owned) = rx.recv() => {
    let bytes = SubscribeUpdate {
        filters: vec![],
        update_oneof: Some(owned.payload),
        created_at: Some(Timestamp::from(owned.created_at)),
    }
    .encode_to_vec();
    // owned.payload is now moved; all other fields are still valid
    sender.push_encoded(
        owned.notification,
        owned.slot,
        owned.slot_status_i32,
        owned.slot_confirmed,
        owned.slot_finalized,
        Arc::new(bytes),
    );
}
```

Update `Sender::push_encoded` signature accordingly:

```rust
pub fn push_encoded(
    &self,
    notification: PluginNotification,
    slot: Slot,
    slot_status_i32: i32,
    slot_confirmed: bool,
    slot_finalized: bool,
    data: Arc<Vec<u8>>,
) {
    let mut state = self.shared.state_lock();
    let metrics = self.push_msg_encoded(
        &mut state,
        notification,
        slot,
        slot_status_i32,
        slot_confirmed,
        slot_finalized,
        data,
    );
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
```

And update `push_msg_encoded` and `PushMetrics`:

```rust
fn push_msg_encoded(
    &self,
    state: &mut MutexGuard<'_, State>,
    notification: PluginNotification,
    slot: Slot,
    slot_status_i32: i32,
    slot_confirmed: bool,
    slot_finalized: bool,
    data: Arc<Vec<u8>>,
) -> PushMetrics {
    let mut removed_max_slot = None;

    state.tail = state.tail.wrapping_add(1);

    let head = state.tail;
    let entry = state.slots.entry(slot).or_insert_with(|| SlotInfo {
        head,
        confirmed: false,
        finalized: false,
    });
    if slot_confirmed {
        entry.confirmed = true;
    }
    if slot_finalized {
        entry.finalized = true;
    }

    state.bytes_total += data.len();
    let idx = self.shared.get_idx(state.tail);
    let mut item = self.shared.buffer_idx(idx);
    if let Some(evicted) = item.data.take() {
        state.head = state.head.wrapping_add(1);
        state.bytes_total -= evicted.1.len();
        removed_max_slot = Some(item.slot);
    }
    item.pos = state.tail;
    item.slot = slot;
    item.data = Some((notification, data));
    drop(item);

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
    let is_dead = is_slot && slot_status_i32 == 6;
    let is_processed = is_slot && slot_status_i32 == 0;
    PushMetrics {
        slot,
        slot_status: if is_slot { Some(slot_status_i32) } else { None },
        is_dead,
        is_processed,
        tail: state.tail,
        head: state.head,
        slots_len: state.slots.len(),
        bytes_total: state.bytes_total,
    }
}
```

Update `update_metrics` to use the integer directly:

```rust
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
```

- [ ] **Step 6: Update `PluginInner` struct**

Replace in `plugin.rs`:

```rust
pub struct PluginInner {
    runtime: Runtime,
    messages: Sender,
    encoder: ProtobufEncoder,
    shutdown: CancellationToken,
    tasks: Vec<(&'static str, PluginTask)>,
}
```

with:

```rust
pub struct PluginInner {
    runtime: Runtime,
    messages: Sender,
    tx: tokio::sync::mpsc::UnboundedSender<OwnedUpdate>,
    shutdown: CancellationToken,
    tasks: Vec<(&'static str, PluginTask)>,
}
```

- [ ] **Step 7: Update `PluginInner::new()` to spawn the encoder task**

Inside the `runtime.block_on(async move { ... })` closure in `PluginInner::new()`, after `let shutdown = CancellationToken::new();`:

```rust
let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<OwnedUpdate>();

// Spawn encoder task
let encoder_sender = messages.clone();
tasks.push((
    "Encoder Task",
    PluginTask(Box::pin(
        tokio::spawn(encoder_task(rx, encoder_sender, shutdown.clone()))
    )),
));
```

Then in the `Ok(...)` at the end of the async block, change:

```rust
Ok::<_, anyhow::Error>((messages, shutdown, tasks))
```

to:

```rust
Ok::<_, anyhow::Error>((messages, tx, shutdown, tasks))
```

And in the destructuring after `block_on`:

```rust
let (messages, tx, shutdown, tasks) = runtime.block_on(...)?;
```

Then in the final `Ok(Self { ... })`:

```rust
Ok(Self {
    runtime,
    messages,
    tx,
    // encoder: ...   ← remove this line
    shutdown,
    tasks,
})
```

- [ ] **Step 8: Rewrite all five geyser callbacks in `plugin.rs`**

**`update_account`:**

```rust
fn update_account(
    &self,
    account: ReplicaAccountInfoVersions,
    slot: u64,
    is_startup: bool,
) -> PluginResult<()> {
    if !is_startup {
        let account = match account {
            ReplicaAccountInfoVersions::V0_0_1(_) => unreachable!(),
            ReplicaAccountInfoVersions::V0_0_2(_) => unreachable!(),
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
        }).ok();
    }
    Ok(())
}
```

**`update_slot_status`:**

```rust
fn update_slot_status(
    &self,
    slot: Slot,
    parent: Option<u64>,
    status: &SlotStatus,
) -> PluginResult<()> {
    use richat_proto::geyser::SlotStatus as ProtoSlotStatus;

    let (status_i32, confirmed, finalized) = match status {
        SlotStatus::Processed       => (0i32, false, false),
        SlotStatus::Confirmed       => (1,    true,  false),
        SlotStatus::Rooted          => (2,    true,  true),
        SlotStatus::FirstShredReceived => (3, false, false),
        SlotStatus::Completed       => (4,    false, false),
        SlotStatus::CreatedBank     => (5,    false, false),
        SlotStatus::Dead(_)         => (6,    false, false),
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
                    SlotStatus::Processed          => ProtoSlotStatus::SlotProcessed,
                    SlotStatus::Confirmed          => ProtoSlotStatus::SlotConfirmed,
                    SlotStatus::Rooted             => ProtoSlotStatus::SlotFinalized,
                    SlotStatus::FirstShredReceived => ProtoSlotStatus::SlotFirstShredReceived,
                    SlotStatus::Completed          => ProtoSlotStatus::SlotCompleted,
                    SlotStatus::CreatedBank        => ProtoSlotStatus::SlotCreatedBank,
                    SlotStatus::Dead(_)            => ProtoSlotStatus::SlotDead,
                } as i32,
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
    }).ok();
    Ok(())
}
```

**`notify_transaction`:**

```rust
fn notify_transaction(
    &self,
    transaction: ReplicaTransactionInfoVersions<'_>,
    slot: u64,
) -> PluginResult<()> {
    use richat_proto::convert_to;

    let transaction = match transaction {
        ReplicaTransactionInfoVersions::V0_0_1(_) => unreachable!(),
        ReplicaTransactionInfoVersions::V0_0_2(_) => unreachable!(),
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
    }).ok();
    Ok(())
}
```

**`notify_entry`:**

```rust
fn notify_entry(&self, entry: ReplicaEntryInfoVersions) -> PluginResult<()> {
    let entry = match entry {
        ReplicaEntryInfoVersions::V0_0_1(_) => unreachable!(),
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
```

**`notify_block_metadata`:**

```rust
fn notify_block_metadata(&self, blockinfo: ReplicaBlockInfoVersions<'_>) -> PluginResult<()> {
    use richat_proto::convert_to;

    let blockinfo = match blockinfo {
        ReplicaBlockInfoVersions::V0_0_1(_) => unreachable!(),
        ReplicaBlockInfoVersions::V0_0_2(_) => unreachable!(),
        ReplicaBlockInfoVersions::V0_0_3(_) => unreachable!(),
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
                rewards: Some(convert_to::create_rewards_obj(
                    &blockinfo.rewards.rewards,
                    blockinfo.rewards.num_partitions,
                )),
                block_time: blockinfo.block_time.map(convert_to::create_timestamp),
                block_height: blockinfo
                    .block_height
                    .map(convert_to::create_block_height),
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
```

- [ ] **Step 9: Remove the old imports from `plugin.rs` that referenced `protobuf::`**

Remove from the `use { ... }` block in `plugin.rs`:

```rust
// DELETE these two lines:
crate::protobuf::{ProtobufEncoder, ProtobufMessage},
```

Remove from `channel.rs` imports:

```rust
// DELETE these two from channel.rs use block:
crate::protobuf::{ProtobufEncoder, ProtobufMessage},
```

- [ ] **Step 10: Remove old `Sender::push` and `push_msg` methods from `channel.rs`**

Delete the entire `pub fn push(&self, message: ProtobufMessage, encoder: ProtobufEncoder)` method — it is now replaced by `push_encoded`.

Delete the `fn push_msg(&self, state: &mut MutexGuard<'_, State>, message: ProtobufMessage, data: Vec<u8>) -> PushMetrics` method — it is now replaced by `push_msg_encoded`. After deleting `push`, nothing calls `push_msg` anymore.

- [ ] **Step 11: Verify compilation**

```bash
cargo check -p richat-plugin-agave
```

Fix any remaining unused import warnings or type mismatches. The old 5 protobuf encoding tests will still compile because the `protobuf` module still exists at this point.

- [ ] **Step 12: Run all tests**

```bash
cargo test -p richat-plugin-agave
```

Expected: all tests pass (5 encoding tests + 3 mask tests = 8 total)

- [ ] **Step 13: Commit**

```bash
git add plugin-agave/src/plugin.rs plugin-agave/src/channel.rs plugin-agave/Cargo.toml
git commit -m "feat(plugin-agave): decouple encoding from validator thread via background encoder task"
```

---

## Task 5: Remove encoder from `config.rs`

**Files:**
- Modify: `plugin-agave/src/config.rs`

- [ ] **Step 1: Remove `encoder` field from `ConfigChannel`**

In `config.rs`, remove from `ConfigChannel`:

```rust
// DELETE:
#[serde(deserialize_with = "ConfigChannel::deserialize_encoder")]
pub encoder: ProtobufEncoder,
```

Remove from `ConfigChannel::default()`:

```rust
// DELETE:
encoder: ProtobufEncoder::Raw,
```

Remove the `deserialize_encoder` function entirely:

```rust
// DELETE the entire function:
pub fn deserialize_encoder<'de, D>(deserializer: D) -> Result<ProtobufEncoder, D::Error>
where
    D: Deserializer<'de>,
{ ... }
```

Remove the imports that are now unused:

```rust
// DELETE from the use block in config.rs:
crate::protobuf::ProtobufEncoder,
// and if only used for encoder:
serde::de::{self, Deserializer},
```

Keep `serde::Deserialize` — it's still used for the struct derives.

- [ ] **Step 2: Verify compilation**

```bash
cargo check -p richat-plugin-agave
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p richat-plugin-agave
```

Expected: all 8 tests pass.

- [ ] **Step 4: Commit**

```bash
git add plugin-agave/src/config.rs
git commit -m "refactor(plugin-agave): remove encoder config field"
```

---

## Task 6: Delete the `protobuf` module and clean up `lib.rs` and `Cargo.toml`

**Files:**
- Modify: `plugin-agave/src/lib.rs`
- Delete: `plugin-agave/src/protobuf/` (entire directory)
- Modify: `plugin-agave/Cargo.toml`

- [ ] **Step 1: Remove `pub mod protobuf` from `lib.rs`**

In `plugin-agave/src/lib.rs`, delete:

```rust
pub mod protobuf;
```

- [ ] **Step 2: Delete the entire protobuf module directory**

```bash
rm -rf plugin-agave/src/protobuf
```

- [ ] **Step 3: Check which Cargo.toml dependencies are now unused**

```bash
cargo check -p richat-plugin-agave 2>&1 | grep "unused\|error\[E"
```

The following crates were only used by the raw encoder and should be removed:

```toml
# DELETE from plugin-agave/Cargo.toml:
solana-account-decoder = { workspace = true }
solana-message = { workspace = true }
solana-pubkey = { workspace = true }
solana-signature = { workspace = true }
solana-transaction = { workspace = true }
solana-transaction-context = { workspace = true }
solana-transaction-error = { workspace = true }
solana-transaction-status = { workspace = true }
```

Keep: `prost` (needed for `prost::Message` trait to call `encode_to_vec`), `prost-types` (needed for `Timestamp`), `solana-clock` (used in `channel.rs` for `Slot`).

Remove each entry and run `cargo check` to confirm each is truly unused. If any still causes a compile error, add it back.

- [ ] **Step 4: Verify compilation**

```bash
cargo check -p richat-plugin-agave
```

- [ ] **Step 5: Run all tests**

```bash
cargo test -p richat-plugin-agave
```

Expected: 5 encoding tests are now gone (module deleted), 3 mask tests pass. Total: `test result: ok. 3 passed`.

- [ ] **Step 6: Commit**

```bash
git add plugin-agave/src/lib.rs plugin-agave/Cargo.toml
git rm -r plugin-agave/src/protobuf
git commit -m "refactor(plugin-agave): delete raw encoder protobuf module"
```

---

## Task 7: Remove missed slot metric and delete fuzz targets

**Files:**
- Modify: `plugin-agave/src/metrics.rs`
- Delete: `plugin-agave/fuzz/`

- [ ] **Step 1: Remove `GEYSER_MISSED_SLOT_STATUS` from `metrics.rs`**

In `metrics.rs`, delete the constant:

```rust
// DELETE:
pub const GEYSER_MISSED_SLOT_STATUS: &str = "geyser_missed_slot_status_total"; // status
```

Delete the `describe_counter!` call for it in `setup()`:

```rust
// DELETE:
describe_counter!(recorder, GEYSER_MISSED_SLOT_STATUS, "Number of missed slot status updates");
```

- [ ] **Step 2: Delete the fuzz directory**

```bash
rm -rf plugin-agave/fuzz
```

- [ ] **Step 3: Verify compilation and tests**

```bash
cargo test -p richat-plugin-agave
```

Expected: `test result: ok. 3 passed`

- [ ] **Step 4: Commit**

```bash
git add plugin-agave/src/metrics.rs
git rm -r plugin-agave/fuzz
git commit -m "refactor(plugin-agave): remove missed slot metric and fuzz targets for deleted raw encoder"
```

---

## Task 8: Add encoding correctness tests

**Files:**
- Modify: `plugin-agave/src/plugin.rs` or a new `plugin-agave/src/tests.rs`

These tests verify that the new `OwnedUpdate` → `SubscribeUpdate::encode_to_vec()` path produces the same bytes as the reference prost path (which is exactly what `encode_prost` did before). Since we are now the reference, we verify against `SubscribeUpdate` constructed directly from the fixture data.

- [ ] **Step 1: Write tests for each message type**

Add a `#[cfg(test)]` module to `plugin-agave/src/plugin.rs`:

```rust
#[cfg(test)]
mod tests {
    use {
        super::{OwnedUpdate, PluginNotification},
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

    fn encode_owned(owned: OwnedUpdate, created_at: SystemTime) -> Vec<u8> {
        SubscribeUpdate {
            filters: vec![],
            update_oneof: Some(owned.payload),
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
            let got = encode_owned(owned, created_at);
            let expected = SubscribeUpdate {
                filters: vec![],
                update_oneof: Some(UpdateOneof::Account(item.to_prost())),
                created_at: Some(Timestamp::from(created_at)),
            }
            .encode_to_vec();
            assert_eq!(got, expected, "account: {item:?}");
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
            let got = encode_owned(owned, created_at);
            let expected = SubscribeUpdate {
                filters: vec![],
                update_oneof: Some(UpdateOneof::Transaction(item.to_prost())),
                created_at: Some(Timestamp::from(created_at)),
            }
            .encode_to_vec();
            assert_eq!(got, expected, "transaction: {item:?}");
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
            let got = encode_owned(owned, created_at);
            let expected = SubscribeUpdate {
                filters: vec![],
                update_oneof: Some(UpdateOneof::Entry(item.to_prost())),
                created_at: Some(Timestamp::from(created_at)),
            }
            .encode_to_vec();
            assert_eq!(got, expected, "entry: {item:?}");
        }
    }

    #[test]
    fn test_encode_slot() {
        let created_at = SystemTime::now();
        for item in generate_slots() {
            let (slot, parent, status) = item.to_replica();
            use agave_geyser_plugin_interface::geyser_plugin_interface::SlotStatus;
            let (status_i32, confirmed, finalized) = match status {
                SlotStatus::Processed          => (0i32, false, false),
                SlotStatus::Confirmed          => (1, true,  false),
                SlotStatus::Rooted             => (2, true,  true),
                SlotStatus::FirstShredReceived => (3, false, false),
                SlotStatus::Completed          => (4, false, false),
                SlotStatus::CreatedBank        => (5, false, false),
                SlotStatus::Dead(_)            => (6, false, false),
            };
            let owned = OwnedUpdate {
                notification: PluginNotification::Slot,
                created_at,
                slot,
                payload: UpdateOneof::Slot(SubscribeUpdateSlot {
                    slot,
                    parent,
                    status: match status {
                        SlotStatus::Processed          => ProtoSlotStatus::SlotProcessed,
                        SlotStatus::Confirmed          => ProtoSlotStatus::SlotConfirmed,
                        SlotStatus::Rooted             => ProtoSlotStatus::SlotFinalized,
                        SlotStatus::FirstShredReceived => ProtoSlotStatus::SlotFirstShredReceived,
                        SlotStatus::Completed          => ProtoSlotStatus::SlotCompleted,
                        SlotStatus::CreatedBank        => ProtoSlotStatus::SlotCreatedBank,
                        SlotStatus::Dead(_)            => ProtoSlotStatus::SlotDead,
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
            let got = encode_owned(owned, created_at);
            let expected = SubscribeUpdate {
                filters: vec![],
                update_oneof: Some(UpdateOneof::Slot(item.to_prost())),
                created_at: Some(Timestamp::from(created_at)),
            }
            .encode_to_vec();
            assert_eq!(got, expected, "slot: {item:?}");
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
            let got = encode_owned(owned, created_at);
            let expected = SubscribeUpdate {
                filters: vec![],
                update_oneof: Some(UpdateOneof::BlockMeta(item.to_prost())),
                created_at: Some(Timestamp::from(created_at)),
            }
            .encode_to_vec();
            assert_eq!(got, expected, "block_meta: {item:?}");
        }
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p richat-plugin-agave
```

Expected: `test result: ok. 8 passed` (5 encoding tests + 3 mask tests)

- [ ] **Step 3: Commit**

```bash
git add plugin-agave/src/plugin.rs
git commit -m "test(plugin-agave): add encoding correctness tests for OwnedUpdate path"
```

---

## Final verification

- [ ] **Run the full test suite one more time**

```bash
cargo test -p richat-plugin-agave
```

Expected: `test result: ok. 8 passed; 0 failed`

- [ ] **Run clippy**

```bash
cargo clippy -p richat-plugin-agave -- -D warnings
```

Fix any warnings.

- [ ] **Build the plugin in release mode**

```bash
cargo build -p richat-plugin-agave --release
```

Expected: builds without errors.
