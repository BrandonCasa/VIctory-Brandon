# Deliver the compacted BVH size before the title allocates its destination

## Location and execution path

The GPU writes a size to a small title buffer. One or two frames later the title reads the slot, allocates that many bytes and asks the builder to copy the structure. `docs/fork-ray-tracing.md` and `crates/victory-gpu/src/memory.rs` above `sizes_wait` document stale values in enabled runs. In light-11, 53 of 230 observed copies still had the previous slot value. A whole-submission wait in light-15 left 33 of 154 stale. The builder was off in the supplied RTX log.

## Current cost

`memory::ready` recognizes `asks_size` by a one-thread shape, plausible constants and small destination, then tags the list with `SIZES` when configured. `Memory::small`, `memory::asked_sizes`, `Host::execute_parts` and the relay operate at buffer/submission granularity. Waiting on the submit worker can occur after the title has already consumed the old CPU value. A wrong allocation size lets compaction overwrite the next structure. A bound on the copy leaves malformed geometry.

## Proposed redesign

1. Recognize a size query by its original compute recipe, parameter layout and store. Use `asks_size` only to narrow candidates. Decode the query kind (compacted size is one of several modes), source structure generation, exact destination range and actual shader store width. Replace the list Boolean with a ticket carrying allocation/mapping generation, list generation and command ordinal. Attach the real producer fence at submit. Abandon tickets for discarded recordings.
2. Directly after the dispatch, order its UAV write and copy the exact bytes to a private readback slot. Capture each write separately, even for repeated writes to the same address or identical values. Use the active GPU version of the result buffer. Keep it and the readback alive until the fence retires. Land only those bytes in `Host::relay_land`, respecting alias identity, later writes and slot reuse.
3. Move the CPU visibility boundary to the title-facing queue submission. Flush the ordered prefix through the query. When its prerequisite signals are already enqueued, submit that prefix, wait for its exact copy and land the bytes before returning to the title. Let the rest of the list proceed asynchronously. This covers persistent mapped loads. For a wait that needs a future signal, return control so the title can enqueue it. Attach the landing to the corresponding guest fence/event or frame wait before making it visible. If an already-submitted query is first consumed by a direct CPU load, make the mapping's read barrier service the ticket before resuming that load. Wait outside storage, queue and page-watch locks. Never wait on the only thread that can enqueue the producer. Preserve genuine guest races.
4. At compaction, validate the source header extent against the actual destination allocation for that query mode. A mismatch fails the generation before the write. Keep quarantine and guessed CPU size formulas off this path.

## Before and after

When a slot is reused for structures A and B, B's allocator receives B's completed result. Its compaction copy fits the allocated span without clobbering A or a neighboring object's header. Unrelated submissions do not have to wait.

## Expected impact and confidence

Moving-object shadows and reflections gain intact dynamic geometry. The assumption is that the CPU intends the preceding query for that slot. After implementation, reuse one slot for two differently sized structures and verify that the second GPU copy leaves its neighbor intact.
