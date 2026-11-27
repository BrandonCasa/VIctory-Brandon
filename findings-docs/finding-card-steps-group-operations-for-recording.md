# Group card operations once for level recording and descriptor writes

## Location and execution path

`crates/victory-gpu/src/host/card_steps.rs:654–695,730–774,811–849` builds a chosen operation vector in `Host::take_steps`. After dependency levels, source owners and destinations are known, recording scans the whole vector for each level. Slot updates scan it again independently in each partition. `host.rs:5717–5732` holds the submit lock during this work.

## Current cost

For L levels, even an operation known to belong to just one level is tested L times. At 256 or more operations, eight workers each visit every operation to find their slot writes, including all operations with no slot destination. The latter scan also occurs in stage 1 fill-only workloads.

## Proposed redesign

Resolve holders and destinations in the original order. For a single level, record directly from `ops`. For multiple levels, group accepted operation indices by level in stable order using contiguous storage, so a long dependency chain does not allocate a list per level. Keep the first-level barrier, one read per staging place and level, and all reads before writes. Handle descriptor updates separately: keep the direct scan for one partition; for eight, append only slot writes to their `(slot_index / 4) % 8` bucket during an existing pass over the resolved ops. Preserve order inside each bucket and skip worker dispatch when there are no slot writes.

## Before and after

For T operations, L levels and P partitions, selection overhead drops from O(TL + TP) tests to O(T + L + P) plus work on emitted entries. It takes O(T) temporary indices, which can be allocated contiguously. Required copies, barriers and descriptor creations still occur.

## Expected impact and confidence

Medium within the opt-in card-step path. Eight full scans are explicit in large batches; single-level, single-partition batches offer little. Measure levels, slot fraction, total worker CPU and elapsed submit-lock time. This applies after level assignment and can be evaluated independently of its algorithm.
