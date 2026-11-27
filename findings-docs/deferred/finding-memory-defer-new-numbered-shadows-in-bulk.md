# Register each numbered shadow once per list refresh interval

## Problem

`crates/victory-gpu/src/memory.rs:300–315,744–752,962–987,1052–1055` appends numbered shadows and calls `Memory::bring_up` after qualifying builder addresses or a new number of copies on a list. `crates/victory-gpu/src/shadow.rs:3178–3181` forwards each full-span refresh to `HostList::defer_refresh` (`crates/victory-gpu/src/host.rs:7222–7235`).

Under the global memory mutex, `bring_up` requests a refresh of the directory and every numbered shadow. Each request separately locks the list and hashes its identity and range. A second address can therefore repeat thousands of checks while adding no refresh steps. Existing exact-range deduplication already suppresses duplicate transfers.

The final November 27 sample reported three numbered copies, zero build dispatches per frame and zero packed structures.

## Change

Register the directory and the append-only numbered vector under one deferred-list lock. Track its registered prefix per recording. Route newly numbered shadows through the existing exact-range deduplication and clear the cursor on title writes and reset. Use this cursor only for registrations. Explicit submission-time landing still invalidates affected data in already registered shadows.

## Expected result

For A `bring_up` calls in a stable interval with C already-numbered shadows, lock acquisitions and range probes can fall from O(AC) to O(A + C); only newly registered shadows need probes after the first call. Store constant-size progress per participating list and reset it at the existing invalidation boundaries. Required refresh bytes remain unchanged.

[Related correctness finding](../finding-memory-defer-new-numbered-shadows-in-bulk.md)
