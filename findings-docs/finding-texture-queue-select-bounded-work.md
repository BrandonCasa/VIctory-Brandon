# Select the texture slots a pass can actually service

## Location and execution path

In `crates/victory-gpu/src/draw.rs`, the once-per-frame stride-heap pass calls `State::heap_textures` (2031–2087). `early_textures` can call it separately for early graphics draws and texture-bake shaders (2103–2162). Heap textures are enabled by default. The pass asks for up to 4,096 slots, or 65,536 for a bake, which can also request a second, future-frame window (2328–2341). Its processing timer starts only after selection (around 2412).

## Current cost

`HostHeap::texture_slots_due` in `crates/victory-gpu/src/descriptor/host.rs:1939–1945` scans the entire locked slot map twice, once for due entries and once for counters. `TextureQueue::in_order` (1113–1139) sorts both due categories, copies every retry to the return vector, then truncates. It even sorts fresh entries when retries alone fill the quota. A burst of descriptor writes therefore adds recording-thread work before the time or byte budget can stop the pass.

## Proposed redesign

Collect the census and due candidates in one locked traversal. Release the lock before ordering. Select the K newest retries first using their unique write numbers. If capacity remains, select exactly the newest and oldest fresh subsets needed to alternate from the two ends, starting with newest. Sort only those retained subsets. Use ordinary sorting when the request already covers essentially the whole input. Keep the snapshot's write tokens for stale-result checks, the full census, on-time-before-lookahead ordering and the bake's duplicate suppression.

## Before and after

For Q queued slots, D due and cap K, current selection uses two O(Q) scans, up to O(D log D) sorting and O(D) temporary space. A single scan plus linear partitioning of the snapshot and sorting the returned prefix costs O(Q + D + K log K), with O(D) snapshot space. Retry-only backlogs need no fresh sort. The advantage shrinks when K covers the backlog.

## Expected impact and confidence

High relative priority for streaming frames with a large due backlog. The source establishes that discarded ordering work precedes rendering and sits outside the pass budget. Compare selection time, queue-lock hold time, due-to-cap ratios and frame-time percentiles during streaming. Verify identical slot/write order against the current selector for normal and bake windows.
