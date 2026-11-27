# Reuse closed-list refresh summaries and fold live lookahead in place

## Problem

`crates/victory-gpu/src/host.rs:5870–5876` chooses `Batch::refresh_front` or `refresh_ahead` before the ordered list execution. On the light, wide-refresh path, `refresh_ahead` (8624–8649) gathers steps up to the first qualifying title-write boundary, then calls `written_spans` (8274–8295). The sequential front branch likewise builds a vector for `front_spans` (8047–8089,8518–8551).

The ahead planner writes one `Touch` for every visited step, including `Nothing` for work irrelevant to this calculation. It immediately reads the vector to reduce it by shadow. The sequential front variant stores meaningful events and part boundaries only, but still builds and consumes a whole intermediate. Neither vector is needed by the eventual shadow refresh.

`ahead_of_writes` defaults on for eligible wide, light-refresh configurations.

## Change

Fold the sequential front scan in place, preserving first-touch order and part boundaries. On the parallel path, build one summary per distinct closed recording after `take_coming`, then join it at every occurrence. Store builder landing positions in that summary and resolve their affected ranges anew on each execution. Keep the refresh spans for affected shadows separate across those positions. Preserve texture order and evaluate `texture_due` in the current batch. Fold the live ahead prefix to one entry per refreshed shadow, freezing it on the next write. End lookahead at the first untaken copy, run, placed operation or builder landing. Keep the closed-list summaries local to front planning.

## Expected result

The sequential planners remove their event vectors and second traversal even for recordings submitted once. On the parallel front path, sharing saves a full step scan for each repeat of the same recording. Each occurrence still joins its shadow entries and visits its textures. Distinct recordings give no sharing benefit and the summaries can be released before GPU execution.

[Related correctness finding](../submit-order-refreshes-around-bvh-landing-writes.md)
