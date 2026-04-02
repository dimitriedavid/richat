# plugin-agave Performance Optimizations

**Date:** 2026-04-02  
**Branch:** dd/plugin-improve-v1  
**Scope:** `plugin-agave/src/`

---

## Problem Summary

Under high transaction throughput (20k+ tx/sec), the plugin-agave Geyser plugin has four compounding performance issues:

1. **Encoding on the validator thread** — every geyser callback (account, slot, transaction, entry, block_meta) synchronously serializes to protobuf before returning. For complex transactions this is substantial CPU work blocking the validator.
2. **Double traversal in the Raw encoder** — `encode_raw` walks the entire message twice: once for size calculation, once for actual encoding.
3. **State Mutex held too long** — `push_msg` holds the state lock through eviction, slot map housekeeping, and metrics writes. Subscribers contend on this lock unnecessarily.
4. **All wakers woken on every push** — a transaction-only subscriber is woken by every account update and entry, burning CPU on spurious wakeups.

Additionally, a 2023-era missed-slot-status hack (parent chain traversal in `push()`) is dead code: it has not triggered in years, adds complexity, and encodes inside the state lock.

---

## Approach: All-in-one architectural refactor

Move encoding entirely off the validator thread. As a consequence, the Raw encoder loses its only advantage (zero-copy from borrowed data) and is removed. All four issues are resolved in one coherent pass.

---

## Architecture

### Current flow

```
Validator thread
  → notify_*(borrowed Geyser data)
  → Sender::push(ProtobufMessage<'borrowed>, encoder)
      → message.encode(encoder)          [BLOCKS — serializes to Vec<u8>]
      → state_lock()
      → push_msg()                       [BLOCKS — eviction + slot map + metrics]
      → wake ALL wakers
```

### New flow

```
Validator thread
  → notify_*(borrowed Geyser data)
  → convert_to::* → OwnedUpdate         [clone into owned prost types]
  → tx.send(owned)                       [O(1), non-blocking, validator done]

Background Tokio encoder task
  → rx.recv().await
  → SubscribeUpdate { .. }.encode_to_vec()   [single-pass, no double traversal]
  → Arc::new(bytes)
  → state_lock()                         [ring buffer + slot map only]
  → push_msg() → PushMetrics
  → drop(state_lock)
  → update_metrics(push_metrics)         [outside lock]
  → drain matching wakers                [outside lock]
  → wake matching wakers only
```

---

## Section 1: OwnedUpdate

New type in `plugin.rs` (or a new `owned.rs` module):

```rust
pub struct OwnedUpdate {
    pub notification: PluginNotification,  // for waker filtering
    pub created_at: SystemTime,            // captured on validator thread
    pub payload: UpdateOneof,              // owned prost enum
}
```

`UpdateOneof` is the existing prost-generated enum from `richat_proto::geyser`. No new types invented.

`PluginNotification` is stored explicitly to avoid a match at waker-drain time.

`created_at` is captured on the validator thread — semantically correct since that is when the event was received.

---

## Section 2: PluginInner and Geyser Callbacks

### PluginInner

```rust
pub struct PluginInner {
    runtime: Runtime,
    messages: Sender,                            // kept — servers subscribe via this
    tx: mpsc::UnboundedSender<OwnedUpdate>,      // added — validator thread pushes here
    shutdown: CancellationToken,
    tasks: Vec<(&'static str, PluginTask)>,
    // encoder: ProtobufEncoder  ← deleted
}
```

### Callbacks

Each callback becomes: unwrap version → build `OwnedUpdate` → `tx.send(owned).ok()`.

`.ok()` is correct: `send` fails only when the receiver is dropped (encoder task exited = shutdown in progress).

Example — `notify_transaction`:

```rust
inner.tx.send(OwnedUpdate {
    notification: PluginNotification::Transaction,
    created_at: SystemTime::now(),
    payload: UpdateOneof::Transaction(SubscribeUpdateTransaction {
        transaction: Some(SubscribeUpdateTransactionInfo {
            signature: transaction.signature.as_ref().to_vec(),
            is_vote: transaction.is_vote,
            transaction: Some(convert_to::create_transaction(transaction.transaction)),
            meta: Some(convert_to::create_transaction_meta(transaction.transaction_status_meta)),
            index: transaction.index as u64,
        }),
        slot,
    }),
}).ok();
```

All five callbacks follow the same pattern. The `SlotStatus` integer mapping (previously inside `encode_prost`) moves inline into `update_slot_status`.

---

## Section 3: Background Encoder Task

Spawned once in `PluginInner::new()` alongside gRPC/QUIC tasks:

```rust
async fn encoder_task(
    mut rx: mpsc::UnboundedReceiver<OwnedUpdate>,
    sender: Sender,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            Some(owned) = rx.recv() => {
                let bytes = SubscribeUpdate {
                    filters: vec![],
                    update_oneof: Some(owned.payload),
                    created_at: Some(owned.created_at.into()),
                }
                .encode_to_vec();
                sender.push(owned.notification, Arc::new(bytes));
            }
            _ = shutdown.cancelled() => break,
        }
    }
}
```

`biased` — drains pending messages before checking shutdown, preventing message loss on shutdown races.

**Why single task, not a pool:** serializes all ring buffer writes (no lock contention), preserves arrival order for slot tracking and replay. `encode_to_vec()` at ~20k tx/sec is well within single-task capacity.

**Shutdown:** on `on_unload`, `shutdown.cancel()` triggers task exit. Remaining queued messages are dropped — acceptable since subscribers are also being torn down.

**Channel:** `tokio::sync::mpsc::unbounded_channel`. No backpressure needed — the encoder task is faster than production rate. Bounded would risk blocking the validator thread, which defeats the purpose.

`Sender::push()` signature changes to accept pre-encoded data:
```rust
pub fn push(&self, notification: PluginNotification, data: Arc<Vec<u8>>)
```

---

## Section 4: Channel Simplification (channel.rs)

### 4a — Remove missed slot check

Delete entirely:
- Lines 76–115: `slot_status` extraction, `SmallVec<[(ProtobufMessage, Vec<u8>); 2]>`, parent chain traversal loop, `message.encode(encoder)` inside lock, missed slot counter metric
- `SlotInfo.parent_slot: Option<Slot>` field
- `push()` becomes a direct single call to `push_msg()`

### 4b — Defer metrics outside the lock

`push_msg` returns a `PushMetrics` struct (plain integers — slot, head, tail, slots_len, bytes_total, status) instead of writing metrics itself. Metrics writes happen after `drop(state)`:

```rust
let metrics = self.push_msg(&mut state, notification, slot, data);
drop(state);
self.update_metrics(metrics);  // no lock held
```

`PushMetrics` is stack-allocated, zero heap cost.

### 4c — Selective waker wakeup

`State.wakers` changes from `Vec<Waker>` to `Vec<(NotificationMask, Waker)>`:

```rust
#[derive(Clone, Copy)]
struct NotificationMask(u8);

impl NotificationMask {
    fn matches(self, n: PluginNotification) -> bool {
        self.0 & n.bit() != 0
    }
}
```

`PluginNotification` gains a `bit(self) -> u8` method:
```rust
fn bit(self) -> u8 {
    match self {
        Self::Slot        => 1 << 0,
        Self::Account     => 1 << 1,
        Self::Transaction => 1 << 2,
        Self::Entry       => 1 << 3,
        Self::BlockMeta   => 1 << 4,
    }
}
```

When a `Receiver` registers its waker in `recv_ref`, it passes a `NotificationMask` computed from its filter fields. Slot and BlockMeta bits are always set (they cannot be filtered).

After `push_msg`, wakers are drained inside the lock (for consistent snapshot) but `.wake()` is called outside:

```rust
let to_wake: Vec<Waker> = {
    let mut state = self.shared.state_lock();
    let metrics = self.push_msg(&mut state, notification, slot, data);
    let mut to_wake = Vec::new();
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
    to_wake
};
for waker in to_wake { waker.wake(); }
```

---

## Deletions

| What | Where |
|------|-------|
| `plugin-agave/src/protobuf/` | Entire module (message.rs, encoding/, mod.rs) |
| `plugin-agave/fuzz/` | All fuzz targets (tested raw encoder) |
| `ProtobufEncoder` enum | message.rs → gone with module |
| `ConfigChannel.encoder` field | config.rs |
| `encoder` field on `PluginInner` | plugin.rs |
| Missed slot check + parent chain | channel.rs:76–115 |
| `SlotInfo.parent_slot` | channel.rs |
| Missed slot counter metric | metrics.rs |

---

## Testing

Existing tests in `plugin-agave/src/protobuf/mod.rs` verified Raw == Prost output. These are deleted with the module.

Replacement: integration-level tests that build an `OwnedUpdate` for each message type, encode via `SubscribeUpdate::encode_to_vec()`, and assert the output matches the expected prost encoding. These can live in `plugin-agave/src/` or use the existing `richat_benches::fixtures`.

---

## Risk Notes

- **Missed slot check removal:** The original 2023 issue (confirmed slot status sometimes not received) has not recurred. The code comment itself expresses doubt about its continued necessity. Risk is low; if the issue resurfaces it manifests as subscribers not seeing a confirmed/rooted status for a slot, which is observable and fixable.
- **Unbounded channel:** Memory grows if the encoder task falls critically behind, but this implies the machine is already under fatal load. The ring buffer's `max_bytes` limit bounds downstream memory regardless.
- **Single encoder task ordering:** Messages arrive at the ring buffer in validator-thread order. If Solana's plugin interface ever calls callbacks concurrently from multiple threads, this assumption breaks. Currently callbacks are single-threaded.
