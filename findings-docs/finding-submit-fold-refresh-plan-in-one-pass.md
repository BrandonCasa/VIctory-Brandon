# Reuse closed-list refresh summaries and fold live lookahead in place

## Location and execution path

`crates/victory-gpu/src/host.rs:5870–5876` chooses `Batch::refresh_front` or `refresh_ahead` before the ordered list execution. On the light, wide-refresh path, `refresh_ahead` (8624–8649) gathers steps up to the first qualifying title-write boundary, then calls `written_spans` (8274–8295). The sequential front branch likewise builds a vector for `front_spans` (8047–8089,8518–8551).

## Current cost

The ahead planner writes one `Touch` for every visited step, including `Nothing` for work irrelevant to this calculation. It immediately reads the vector to reduce it by shadow. The sequential front variant stores meaningful events and part boundaries only, but still builds and consumes a whole intermediate. Neither vector is needed by the eventual shadow refresh.

## Proposed redesign

Fold the sequential front scan directly, preserving first-touch order and part boundaries. On the parallel path that already builds `ListFront`, share one summary when the submission names the same closed recording more than once. Build it after `take_coming` has incorporated pending steps and join it at every original occurrence. Preserve texture encounter order and evaluate `texture_due` against the current batch. Keep these summaries local to front planning. On the detached path, `list::detach` replaces the submitted host list and `list::recycle` resets or drops the recording after landing.

In `refresh_ahead`, fold the live prefix into one entry per refreshed shadow. Ignore writes before its first refresh, freeze the span on its next write and emit only entries that reached that write. Stop at the first untaken copy, run or placed operation across the submission.

## Before and after

The sequential planners remove their event vectors and second traversal even for recordings submitted once. On the parallel front path, sharing saves a full step scan for each repeat of the same recording. Each occurrence still joins its shadow entries and visits its textures. Distinct recordings give no sharing benefit and the summaries can be released before GPU execution.

## Expected impact and confidence

Medium to high for long submissions on wide, light-refresh configurations. `ahead_of_writes` is enabled by default when those prerequisites hold (7943–7958,8017–8019). Verify output, stop boundaries and texture ordering, then measure planner allocation bytes and submit CPU versus steps visited.
