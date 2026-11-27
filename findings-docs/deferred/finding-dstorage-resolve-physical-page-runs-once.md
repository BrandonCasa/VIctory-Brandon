# Share physical page resolution between DirectStorage validation and writing

## Problem

`crates/victory-hle/src/modules/dstorage.rs:1593–1675,1717–1729` handles page destinations through `into_destination`, `page_list` and `into_pages`. `crates/victory-hle/src/modules/xmem.rs:787–815,838–853,893–906` contains `runs`, `pages_known`, `write_physical` and `page_window`.

A known list is validated under the page-table lock, then looked up again to build section runs for the in-place window. A multi-run list cannot use that window, so its resolved runs are discarded; the buffered output writer resolves them again. Each traversal visits the physical page IDs and competes with mapping operations.

## Change

Cache contiguous section runs for an accepted page-list snapshot, keeping one run inline. Advance a generation on page removal and actual slice reassignments. Reuse the runs between validation, the direct-window attempt and scatter only while the mapping is still valid and protected during use. Clip the run list to the requested extent on each use. Preserve retries, the post-fill reread and error notifications.

## Expected result

For P physical pages in R runs, stable mappings need one O(P) resolution per accepted list version. Direct mapping, buffered scatter and post-write alias discovery can share O(R) descriptors for that snapshot. Each new write attempt still validates ownership freshness, and current virtual aliases must be looked up after the write. Page-list rereads and retries on mapping failure remain.

[Related correctness finding](../finding-dstorage-resolve-physical-page-runs-once.md)
