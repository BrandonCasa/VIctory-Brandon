# Select the texture slots a pass can actually service

## Location and execution path

In `crates/victory-gpu/src/draw.rs`, the once-per-frame stride-heap pass calls `State::heap_textures` (2031–2087). `early_textures` can call it separately for early graphics draws and texture-bake shaders (2103–2162). Heap textures are enabled by default. The pass asks for up to 4,096 slots, or 65,536 for a bake, which can also request a second, future-frame window (2328–2341). Its processing timer starts only after selection (around 2412).

## Current cost

`HostHeap::texture_slots_due` in `crates/victory-gpu/src/descriptor/host.rs:1939–1945` scans the entire locked slot map twice, once for due entries and once for counters. `TextureQueue::in_order` (1113–1139) sorts both due categories, copies every retry to the return vector, then truncates. It even sorts fresh entries when retries alone fill the quota. A burst of descriptor writes therefore adds recording-thread work before the time or byte budget can stop the pass.

## Proposed redesign

Collect the census and due `(slot, write)` pairs in one locked traversal. Use the write frame for the census only. A zero cap needs no candidates, and the bake's lookahead caller discards its census. After unlocking, partition retries in place by write number when only the newest K fit. For R remaining slots, take up to R fresh entries, half from each end with the extra entry from the newest end. Sort only the selected subsets and interleave newest-first and oldest-first. Sort a category directly when it fits. Keep both bake windows' existing caps, ordering and slot deduplication, and return each original write token for stale-result checks.

## Before and after

For Q queued slots, D due and cap K, current selection uses two O(Q) scans, up to O(D log D) sorting and O(D) temporary space. A single scan plus linear partitioning of the snapshot and sorting the returned prefix costs O(Q + D + K log K), with O(D) snapshot space. Retry-only backlogs need no fresh sort. The advantage shrinks when K covers the backlog.

## Expected impact and confidence

High relative priority for streaming frames with a large due backlog. The source establishes that discarded ordering work precedes rendering and sits outside the pass budget. Compare selection time, queue-lock hold time, due-to-cap ratios and frame-time percentiles during streaming. Verify identical slot/write order against the current selector for normal and bake windows.
