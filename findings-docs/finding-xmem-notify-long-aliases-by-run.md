# Resolve long virtual aliases once per physical run

## Location and execution path

`crates/victory-hle/src/modules/dstorage.rs:1651–1675` notifies `xmem::pages_written` after writing physical destinations. `crates/victory-hle/src/modules/xmem.rs:540–600,916–966` implements `Shown`, `shown_at` and both GPU-notification branches.

## Current cost

`shown_at` clones a section reference for each destination page and calls `Shown::at` per page. Each call searches all long views, even those of unrelated sections. It materializes page-sized alias pieces, sorts them and joins neighbors. One contiguous write through one long mapping can produce just one final notification.

## Proposed redesign

In `shown_at`, resolve consecutive slices of one section as a run, retaining input offsets across missing pages. Under the separate `SHOWN` read lock, intersect each run once with the existing long views and collect short aliases from their page buckets. Clip spans to the written bytes. Release the lock, sort by address and join adjacent spans. If any spans overlap, first split the unmerged spans at their original page boundaries and use the existing sort-and-join rule to preserve the output for repeated pages. Long-view probes then fall from P × L to R × L without adding work on remaps. Keep alias resolution after the physical write.

## Before and after

For P pages and L long mappings the existing long-view probe count is P × L. With R physical runs, an interval index can approach O(R log L + matches) for long views, in addition to O(P) page resolution and short-alias work. A single long intersection yields one piece before sorting. Index updates cost more when mappings change.

## Expected impact and confidence

High during page-backed streaming with long mappings. The nested loop is explicit for either notification setting. Measure runs, long views tested, pieces before joining, SHOWN read-lock time and end-to-end completion. Small writes or few aliases limit the gain.
