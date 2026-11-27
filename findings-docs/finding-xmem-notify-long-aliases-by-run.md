# Resolve long virtual aliases once per physical run

## Location and execution path

`crates/victory-hle/src/modules/dstorage.rs:1651–1675` notifies `xmem::pages_written` after writing physical destinations. `crates/victory-hle/src/modules/xmem.rs:540–600,916–966` implements `Shown`, `shown_at` and both GPU-notification branches.

## Current cost

`shown_at` clones a section reference for each destination page and calls `Shown::at` per page. Each call searches all long views, even those of unrelated sections. It materializes page-sized alias pieces, sorts them and joins neighbors. One contiguous write through one long mapping can produce just one final notification.

## Proposed redesign

Let a DirectStorage fill pass its resolved section runs to `pages_written`, clipped to the notification's byte range. Under the page-table lock, validate the same generation used by the writer. Both page removal and a slice reassignment in `arrange` invalidate the plan. On a miss, resolve the current runs, skipping unknown pages while preserving their positions in the input. Drop that lock, then consult the current `SHOWN` aliases under their own read lock even on a generation hit. Intersect long views once per run and collect short aliases from their page buckets. Release the lock before sorting or notifying. Where alias spans overlap, split them at the original page boundaries before applying the existing sort and adjacency rule.

## Before and after

For P pages across R runs and L long mappings, long-view probes fall from P × L to R × L. A current write plan can also avoid P more physical-page lookups and per-page section reference clones. Short aliases still require page-bucket probes, and overlap can still require sorting page-sized pieces. The mapping writer needs no new long-view index.

## Expected impact and confidence

High during page-backed streaming with long mappings. The nested loop is explicit for either notification setting. Measure runs, long views tested, pieces before joining, SHOWN read-lock time and end-to-end completion. Small writes or few aliases limit the gain.
