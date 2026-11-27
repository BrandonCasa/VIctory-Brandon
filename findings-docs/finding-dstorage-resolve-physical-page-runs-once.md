# Share physical page resolution between DirectStorage validation and writing

## Location and execution path

`crates/victory-hle/src/modules/dstorage.rs:1593–1675,1717–1729` handles page destinations through `into_destination`, `page_list` and `into_pages`. `crates/victory-hle/src/modules/xmem.rs:787–815,838–853,893–906` contains `runs`, `pages_known`, `write_physical` and `page_window`.

## Current cost

A known list is validated under the page-table lock, then looked up again to build section runs for the in-place window. A multi-run list cannot use that window, so its resolved runs are discarded; the buffered output writer resolves them again. Each traversal visits the physical page IDs and competes with mapping operations.

## Proposed redesign

Resolve page IDs to section runs under the page-table read lock, keeping a single run inline. Record a generation for the page-ID-to-slice mapping. Advance it under the write lock on an actual removal and whenever `arrange` reassigns slice indices (`xmem.rs:1112-1183`). The latter can change a run while every page ID is still owned. Fresh allocations use new IDs.

Before reusing a plan for the direct window, buffered scatter, retry or notification, check that generation under the read lock. Re-resolve stale or unknown pages for the extent that operation needs. Keep the section references alive and release the lock before mapping or copying. For a short inflate, clip both the mapped pages and copied bytes to the filled prefix on scatter. Keep page-list polling and the post-fill reread that can trigger a fresh fill. Direct writes still notify the requested range even on error. Buffered writes notify the filled prefix after the whole scatter succeeds.

## Before and after

For P physical pages in R runs, stable mappings need one O(P) resolution per accepted list version. Direct mapping, buffered scatter and post-write alias discovery can share O(R) descriptors. Each use still validates ownership freshness, and current virtual aliases must be looked up after the write. Page-list rereads and retries on mapping failure remain.

## Expected impact and confidence

Measure locked page lookups per request, generation misses, lock waits and streaming completion latency. Check a slice swap between validation and use, and a short inflate whose unused suffix includes an unknown page. The expected saving is in repeated lookups during page-backed loads.
