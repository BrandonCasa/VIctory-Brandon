# Share physical page resolution between DirectStorage validation and writing

## Problem

`crates/victory-hle/src/modules/dstorage.rs:1593–1675,1717–1729` handles page destinations through `into_destination`, `page_list` and `into_pages`. `crates/victory-hle/src/modules/xmem.rs:787–815,838–853,893–906` contains `runs`, `pages_known`, `write_physical` and `page_window`.

A known list is validated under the page-table lock, then looked up again to build section runs for the in-place window. A multi-run list cannot use that window, so its resolved runs are discarded; the buffered output writer resolves them again. Each traversal visits the physical page IDs and competes with mapping operations.

## Change

Cache contiguous section runs for the submitted snapshot, keeping one run inline. Reuse them between validation, the direct-window attempt and scatter while their assignments are reserved. If releasing a reservation for a retry, revalidate before the next write. Clip buffered writes to the produced prefix and carry the actual backing through error or success notification.

## Expected result

For P physical pages in R runs, a stable submitted list needs one O(P) resolution. Direct mapping, buffered scatter and post-write alias discovery can share O(R) descriptors. Current virtual aliases are looked up after the write. Unknown pages and failed host mappings can still incur retries before publication.

[Related correctness finding](../finding-dstorage-resolve-physical-page-runs-once.md)
