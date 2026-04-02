# plugin-agave: performance optimizations

## What changed

Four independent improvements to the Geyser plugin, shipped together because they compound and because the most impactful one (encoding off the validator thread) made the raw encoder obsolete, which in turn simplified everything else.

### 1. Encoding moved off the validator thread

**Before:** Every geyser callback (`notify_transaction`, `update_account`, etc.) called `Sender::push()` synchronously, which serialized the message to protobuf bytes before returning. For a complex transaction with inner instructions, token balances and address lookups this is a non-trivial amount of CPU work happening on the validator's own thread, blocking it from processing the next item.

**After:** Each callback clones the relevant data into an owned struct (`OwnedUpdate`) and sends it via a `tokio::sync::mpsc::unbounded_channel`. The callback returns immediately. A single background Tokio task drains the channel, calls `SubscribeUpdate::encode_to_vec()`, and pushes the encoded bytes into the ring buffer.

The clone is unavoidable — `ProtobufMessage<'a>` borrowed validator-owned memory, so to hand off to another thread you must own the data. The cost of the clone is roughly the same as the cost of encoding (both walk the full transaction), so total CPU work is similar; the difference is it no longer happens on the validator thread.

### 2. Raw encoder removed

**Before:** There were two encoding paths selectable via `config.channel.encoder`: a custom zero-copy raw encoder (`"raw"`, the default) and standard prost (`"prost"`). The raw encoder's advantage was that it encoded directly from borrowed Geyser data without allocating intermediate structs — two passes (size calculation + encode) but no intermediate allocation.

**After:** Only prost. `SubscribeUpdate::encode_to_vec()` does a single pass with dynamic allocation.

**Why:** The raw encoder's zero-copy advantage only existed when encoding happened on the validator thread directly from borrowed data. Once encoding moves to a background task, you must clone the data first regardless. That clone already allocates the owned prost structs (`OwnedUpdate` carries `UpdateOneof` which is prost-generated). At that point `encode_to_vec()` is a single-pass write into a growing `Vec<u8>` — simpler and equally fast. The raw encoder's double-traversal (size pass + encode pass) was strictly worse.

The encoding tests (`test_encode_*`) verify byte-for-byte that the new prost path produces identical output to what the raw encoder produced. No subscriber-visible change in wire format.

### 3. Missed slot status check removed

**Before:** `Sender::push()` maintained a `parent_slot` chain in `SlotInfo` and traversed it on every slot status update to retroactively emit `Confirmed`/`Rooted` status for any parent slots that had been skipped. This was added in March 2023 after Triton One observed that Agave sometimes failed to emit `Confirmed` for certain slots.

**After:** Removed entirely. `SlotInfo.parent_slot` is gone. `Sender::push()` is a straight-line path.

**Why:** The issue has not been observed since 2023. The code comment at the time said "I'm not sure that this still a case but for safety I added this hack." The cost is non-trivial: a `SmallVec` allocation and a `BTreeMap` traversal on every slot push on the validator thread. If the issue resurfaces, subscribers will miss a `Confirmed` or `Rooted` status for an occasional slot — observable and non-fatal.

### 4. Two channel micro-optimisations

**Metrics deferred outside the state lock:** `push_msg` previously wrote prometheus gauges while holding the state `Mutex`. Metrics writes now happen after `drop(state)` via a `PushMetrics` snapshot. Reduces lock hold time, reducing contention for subscriber tasks calling `recv_ref`.

**Selective waker wakeup:** Previously every push woke every subscriber regardless of what they were subscribed to. A transaction-only subscriber was woken by every account update and entry, only to go back to sleep after filtering in `recv_ref`. Wakers are now tagged with a `NotificationMask` bitmask. Each push only wakes subscribers whose filter intersects the message type. Slot and BlockMeta messages always wake all subscribers (they cannot be filtered).

---

## Breaking changes

### Config: `channel.encoder` field removed

`ConfigChannel` uses `#[serde(deny_unknown_fields)]`. Any config file with `"encoder": "raw"` or `"encoder": "prost"` will now **fail to load** with an unknown field error.

**Action required:** Remove the `"encoder"` line from your `config.json`.

### CLI: `--no-verify` flag removed from `stream-richat`

The `--no-verify` flag suppressed the cross-encoder verification (raw vs prost byte comparison) in the CLI's `stream-richat` subcommand. Since there is no longer a second encoder to compare against, the flag and the verification code were removed. Scripts passing `--no-verify` will get an unknown argument error.

### Public API: `richat_plugin_agave::protobuf` module deleted

The `protobuf` module (`ProtobufMessage`, `ProtobufEncoder`, and all encoding helpers) was a public module of the `richat-plugin-agave` crate. It is now gone. The only known internal consumer was the CLI's `stream-richat` command (updated in this PR). External crates importing from this module will need to be updated.

---

## What we gain

- **Validator thread unblocked.** The plugin's per-callback cost is now: a few `to_vec()` calls for pubkeys/data, one prost struct construction, one channel send. No lock acquisition, no protobuf serialisation.
- **Single-pass encoding.** `encode_to_vec()` replaces the raw encoder's two-pass size-then-encode loop.
- **Shorter lock hold time.** State mutex is held only for ring buffer and slot map operations. Metrics writes happen outside.
- **Less spurious wakeups.** Transaction-only subscribers are no longer woken by account and entry pushes.
- **Less code.** Deleted: `plugin-agave/src/protobuf/` (entire module), `plugin-agave/fuzz/` (all fuzz targets), ~200 lines from `channel.rs`, `--no-verify` CLI path. Net: significant reduction in total lines.

## What we lose

- **The raw encoder.** If for some reason prost's `encode_to_vec()` has a measurable overhead vs the old raw path in a synchronous on-thread scenario, that option is gone. Given the move to background encoding this is not a real trade-off.
- **Missed slot status recovery.** The 2023 parent-chain hack is gone. An occasional missed `Confirmed`/`Rooted` slot status is now possible if Agave regresses on that front.
- **Cross-encoder verification in the CLI.** The `stream-richat --no-verify` debugging tool that compared raw vs prost output is gone. It was only useful when both encoders existed.
