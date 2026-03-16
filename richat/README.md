# richat storage notes

This document explains the current RocksDB replay storage design in `richat/`,
the first stabilization phase that keeps the single-DB layout, and the two
larger retention paths discussed for future work.

## Current workload

`richat` stores replayable messages in RocksDB:

- `slot_index` stores per-slot metadata
- `message_index` stores the replay payload keyed by a monotonically increasing
  replay index

The workload is:

- append-heavy
- replay reads are sequential
- retention deletes the oldest prefix
- disk efficiency matters because long retention requires compression

That workload is not a natural fit for a single hot LSM tree with frequent
oldest-prefix eviction.

## Root cause of the retention cliff

The old implementation trimmed replay storage by deleting one slot at a time and
issuing `DeleteRange(message_index, 0, until)` repeatedly.

That caused three problems:

1. The same historical prefix was tombstoned over and over.
2. RocksDB had to compact overlapping old SSTs to reclaim space.
3. While `db.write()` slowed down, the in-process serializer kept growing one
   `WriteBatch`, which pushed RSS and `channel_storage_write_batch_size` into
   the multi-GB range.

With `zstd`, that compaction work is even more expensive because rewritten SSTs
are recompressed.

## Phase 1: single-DB stabilization

Phase 1 keeps the existing single RocksDB instance and fixes the pathological
parts without changing the replay API or on-disk schema.

### What Phase 1 changes

1. Incremental trim
   Old replay data is now trimmed with one monotonic delta:
   `[previous_oldest_retained_head, new_oldest_retained_head)`.
   The serializer no longer re-deletes from index `0`.

2. Coalesced trim
   Finalization now trims all excess slots in one pass instead of enqueueing one
   storage delete per slot.

3. Trim hysteresis
   Storage can temporarily exceed `max_slots` by `trim_slack_slots`, then trims
   back to `max_slots`. This reduces trim frequency and write amplification.

4. Bounded ingest queue
   The enqueue path now reserves from a byte budget before handing work to the
   serializer thread. The queue no longer grows without bound.

5. Bounded write batches
   The serializer flushes foreground writes by size and time instead of
   appending forever while the RocksDB writer is blocked.

6. Separate maintenance writes
   Trim operations are coalesced and flushed as their own small write batch
   rather than being merged into large foreground write batches.

7. RocksDB retune
   The message column family now uses larger write buffers, larger level sizes,
   larger WAL budget, subcompactions, and sync pacing that better match the
   intended ingest rate.

8. Hot/cold compression split
   When `messages_compression` is `zstd`, hot levels default to `lz4` while deep
   levels and the bottommost level stay on `zstd`. This keeps long-lived data
   compressed without paying `zstd` cost on every hot rewrite.

### New metrics

Phase 1 adds:

- `channel_storage_write_ser_queue_bytes`
- `channel_storage_trim_from_index`
- `channel_storage_trim_to_index`
- `channel_storage_trim_slots`
- `channel_storage_trim_requests_total`
- `channel_storage_trim_requests_merged_total`

These metrics make it easier to prove that trim is incremental and that
in-process buffering stays bounded.

### New storage config knobs

The storage section now supports:

- `trim_slack_slots`
- `max_ingest_queue_bytes`
- `max_write_batch_bytes`
- `max_write_batch_delay`
- `messages_hot_compression`
- `rocksdb_max_total_wal_size`
- `rocksdb_message_write_buffer_size`
- `rocksdb_message_max_write_buffer_number`
- `rocksdb_message_l0_file_num_compaction_trigger`
- `rocksdb_message_target_file_size_base`
- `rocksdb_message_max_bytes_for_level_base`
- `rocksdb_message_max_subcompactions`
- `rocksdb_message_bytes_per_sync`
- `rocksdb_message_wal_bytes_per_sync`

The defaults are chosen to stabilize the current `~1000` slot replay window.

## Why not a compaction filter

Compaction filters were considered as an alternative to explicit trim.

They were rejected for Phase 1 because they are lazy:

- data is only removed when compaction touches it
- disk retention becomes approximate instead of deterministic
- reclaim timing depends on background compaction pressure

For `richat`, exact retention control is more useful than lazy cleanup.

## Future phases

Phase 1 is meant to flatten the current `~1000` slot steady state. It is not
the final answer for very long retention windows such as `48h`.

Two future directions remain on the table.

### Option A: append-only zstd log segments

Store payloads in immutable compressed segment files and keep only metadata in a
small KV/index store.

Shape:

- payloads in append-only `zstd` segments
- independent compressed chunks inside each segment
- small metadata store for `slot -> (segment, offset)` lookups
- replay by sequential scan from the first matching chunk
- eviction by deleting old segment files

Pros:

- compress once instead of recompressing during compaction
- sequential writes and sequential replay
- eviction is `unlink`, not compaction
- disk efficiency is usually the best of the options

Cons:

- bigger implementation change
- needs crash recovery logic for the active segment
- requires a small metadata/index layer beside the segment files

### Option B: sharded RocksDB instances

Keep RocksDB but rotate multiple DB instances over slot/time ranges.

Shape:

- one active shard DB for new writes
- sealed shard DBs for older immutable data
- replay walks shards in order
- eviction deletes whole old shard directories

Pros:

- much smaller change than a custom log store
- keeps the existing RocksDB key/value schema
- avoids range tombstone debt on the oldest prefix
- retention becomes `close + delete old shard`

Cons:

- still pays LSM rewrite/compression cost inside a shard
- needs a manifest/catalog and cross-shard replay logic
- retention granularity is limited by shard size

## Choosing between the future options

Use append-only compressed logs if:

- retention is measured in many hours or days
- payload size dominates metadata size
- disk efficiency matters more than preserving RocksDB semantics

Use sharded RocksDB if:

- you want to stay close to the current code
- the existing replay/index schema is valuable
- you want most of the benefit without designing a custom file format

## Validation plan

Phase 1 should be evaluated with a soak that crosses the retention boundary and
holds high ingest for at least 30-60 minutes after trim starts.

Expected behavior:

- `channel_storage_write_batch_size` stays bounded by the configured cap
- `channel_storage_write_ser_queue_bytes` stays bounded by the configured queue
  budget
- `channel_storage_trim_from_index` and `channel_storage_trim_to_index` move
  forward monotonically
- `channel_storage_trim_requests_merged_total` increases under pressure
- `channel_storage_rocksdb_estimate_pending_compaction_bytes` may spike
  transiently but should no longer ramp without bound at the retention edge

If the single-DB phase is stable for `~1000` slots but not for much larger
windows, the next step should be shard rotation or immutable log segments, not
more prefix deletes in one monolithic DB.
