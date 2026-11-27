# Share physical page resolution between DirectStorage validation and writing

## Location and execution path

`crates/victory-hle/src/modules/dstorage.rs:1593–1675,1717–1729` handles page destinations through `into_destination`, `page_list` and `into_pages`. `crates/victory-hle/src/modules/xmem.rs:787–815,838–853,893–906` contains `runs`, `pages_known`, `write_physical` and `page_window`.

## Current cost

A known list is validated under the page-table lock, then looked up again to build section runs for the in-place window. A multi-run list cannot use that window, so its resolved runs are discarded; the buffered output writer resolves them again. Each traversal visits the physical page IDs and competes with mapping operations.

## Proposed redesign

Return owned section runs and a mapping generation from the successful validation attempt. Use them to choose the single-window path or write buffered output over the same runs. If mappings changed during decode, resolve again at the point the existing writer would. Bound the scatter to the actual filled prefix. Preserve page-list rereads and up to three changing destinations, unknown-page retries and notification using current visible aliases (`xmem.rs:957–966`).

## Before and after

For P pages in R runs, the stable contiguous case can use one O(P) resolution in place of two. A stable fragmented write can avoid a third traversal. O(R) retained run descriptors replace transient copies. Payload bytes and the required final mutable-list check remain.

## Expected impact and confidence

Medium to high during large page-backed streaming loads. The duplicate lookups are explicit, but mapped aliases, copy/decode time and concurrent remaps may dominate. Measure locked page lookups per request, generation misses, lock waits and streaming completion latency.
