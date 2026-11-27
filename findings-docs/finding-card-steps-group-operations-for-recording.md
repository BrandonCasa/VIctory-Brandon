# Group card operations once for level recording and descriptor writes

## Location and execution path

`crates/victory-gpu/src/host/card_steps.rs:654–695,730–774,811–849` builds a chosen operation vector in `Host::take_steps`. After dependency levels, source owners and destinations are known, recording scans the whole vector for each level. Slot updates scan it again independently in each partition. `host.rs:5717–5732` holds the submit lock during this work.

## Current cost

`does_of` knows the originating run and step, but keeps a list address. `fill_to` later searches the run list and takes the deferred-list lock again for each chosen fill. Recording then tests each resolved operation at every level. At 256 or more operations, eight workers scan the same operations to find their slot writes.

## Proposed redesign

Keep the run ordinal and step index when `does_of` walks the submission. For `take_steps`, use them to clone chosen fill destinations under one short list lock per contributing run, in step order. Release it before resolving shadows or recording commands. Derive the existing list-address identity where `Taken` or the split probe needs it, including duplicate list submissions. Resolve operations in original order. Record a single level directly. For multiple levels, group accepted indices stably in contiguous storage, keeping reads before writes and one read per place per level. Once the accepted count reaches the partition threshold, distribute slot writes in one pass by `(slot_index / 4) % 8`, preserving bucket order. Skip worker dispatch if there are no slot writes.

## Before and after

For F chosen fills across M submitted runs, locating destinations can cost O(FM) list comparisons today. Direct run indices remove that search and batching reduces F lock acquisitions to one per contributing run. For T emitted operations, L levels and P partitions, stable grouping reduces selection from O(TL + TP) to O(T + L + P) using O(T) temporary entries. Copies, barriers and descriptor writes remain.

## Expected impact and confidence

Medium within the opt-in card-step path. Eight full scans are explicit in large batches; single-level, single-partition batches offer little. Measure levels, slot fraction, total worker CPU and elapsed submit-lock time. This applies after level assignment and can be evaluated independently of its algorithm.
