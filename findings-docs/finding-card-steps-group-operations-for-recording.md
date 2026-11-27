# Group card operations once for level recording and descriptor writes

## Location and execution path

`crates/victory-gpu/src/host/card_steps.rs:654–695,730–774,811–849` builds a chosen operation vector in `Host::take_steps`. After dependency levels, source owners and destinations are known, recording scans the whole vector for each level. Slot updates scan it again independently in each partition. `host.rs:5717–5732` holds the submit lock during this work.

## Current cost

For L levels, even an operation known to belong to just one level is tested L times. At 256 or more operations, eight workers each visit every operation to find their slot writes, including all operations with no slot destination. The latter scan also occurs in stage 1 fill-only workloads.

## Proposed redesign

After resolving operations in their existing order, build stable per-level index lists and append each slot operation to its existing `(slot_index / 4) % partitions` bucket. Use those buckets for recording and worker dispatch. Retain first-level barriers, all reads before writes within each level, one read per staging place/level, and ordering for repeated writes to a slot. Keep the tiny-plan single-pass variant where cheaper.

## Before and after

For T operations, L levels and P partitions, selection overhead drops from O(TL + TP) tests to O(T + L + P) plus work on emitted entries. It takes O(T) temporary indices, which can be allocated contiguously. Required copies, barriers and descriptor creations still occur.

## Expected impact and confidence

Medium within the opt-in card-step path. Eight full scans are explicit in large batches; single-level, single-partition batches offer little. Measure levels, slot fraction, total worker CPU and elapsed submit-lock time. This applies after level assignment and can be evaluated independently of its algorithm.
