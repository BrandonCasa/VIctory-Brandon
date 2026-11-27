# Resolve long virtual aliases once per physical run

## Location and execution path

`crates/victory-hle/src/modules/dstorage.rs:1651–1675` notifies `xmem::pages_written` after writing physical destinations. `crates/victory-hle/src/modules/xmem.rs:540–600,916–966` implements `Shown`, `shown_at` and both GPU-notification branches.

## Current cost

`shown_at` clones a section reference for each destination page and calls `Shown::at` per page. Each call searches all long views, even those of unrelated sections. It materializes page-sized alias pieces, sorts them and joins neighbors. One contiguous write through one long mapping can produce just one final notification.

## Proposed redesign

Resolve consecutive indices of the same section into runs at notification time. Index long views by section and physical index range in `Shown::note` and `forget`, then intersect each run once. For short aliases, append matches from existing buckets. Preserve exact leading/trailing byte offsets, missing pages, aliases and repeated page IDs, output sort and adjacency semantics. Retain separate lock acquisition and resolve current mappings after writing; a decode-time snapshot can be stale.

## Before and after

For P pages and L long mappings the existing long-view probe count is P × L. With R physical runs, an interval index can approach O(R log L + matches) for long views, in addition to O(P) page resolution and short-alias work. A single long intersection yields one piece before sorting. Index updates cost more when mappings change.

## Expected impact and confidence

High during page-backed streaming with long mappings. The nested loop is explicit for either notification setting. Measure runs, long views tested, pieces before joining, SHOWN read-lock time and end-to-end completion. Small writes or few aliases limit the gain.
