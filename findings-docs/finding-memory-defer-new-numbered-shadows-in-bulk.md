# Register each numbered shadow once per list refresh interval

## Location and execution path

`crates/victory-gpu/src/memory.rs:300–315,744–752,962–987,1052–1055` appends numbered shadows and calls `Memory::bring_up` after qualifying builder addresses or a new number of copies on a list. `crates/victory-gpu/src/shadow.rs:3178–3181` forwards each full-span refresh to `HostList::defer_refresh` (`crates/victory-gpu/src/host.rs:7222–7235`).

## Current cost

Under the global memory mutex, `bring_up` requests a refresh of the directory and every numbered shadow. Each request separately locks the list and hashes its identity and range. A second address can therefore repeat thousands of checks while adding no refresh steps. Existing exact-range deduplication already suppresses duplicate transfers.

## Proposed redesign

Add a bulk registration operation to the list. Under one deferred lock, track whether the directory has been handled and how far the append-only numbered vector has been registered for this memory instance. Route new entries through existing exact-range deduplication. Clear this marker with `Deferred::title_written` and list reset (2483,6594,7177,7584). Keep caller timing, host-only landing callback order and per-list ownership. A host-only callback at 7590 intentionally leaves current deduplication intact.

## Before and after

For A `bring_up` calls in a stable interval with C already-numbered shadows, lock acquisitions and range probes can fall from O(AC) to O(A + C); only newly registered shadows need probes after the first call. Store constant-size progress per participating list and reset it at the existing invalidation boundaries. Required refresh bytes remain unchanged.

## Expected impact and confidence

High conditional on repeated by-address preparation in long recordings with many numbered shadows, lower for ordinary draws. The named BVH builder is opt-in (`pipeline.rs:339–344`); other by-address uses and copy-count changes can also trigger registration. Measure attempted vs. inserted refreshes, numbered copies and time under the memory mutex.
