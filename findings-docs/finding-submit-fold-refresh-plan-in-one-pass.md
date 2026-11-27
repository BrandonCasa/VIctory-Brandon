# Fold refresh-planning events as the submitting thread visits command steps

## Location and execution path

`crates/victory-gpu/src/host.rs:5870–5876` chooses `Batch::refresh_front` or `refresh_ahead` before the ordered list execution. On the light, wide-refresh path, `refresh_ahead` (8624–8649) gathers steps up to the first qualifying title-write boundary, then calls `written_spans` (8274–8295). The sequential front branch likewise builds a vector for `front_spans` (8047–8089,8518–8551).

## Current cost

The ahead planner writes one `Touch` for every visited step, including `Nothing` for work irrelevant to this calculation. It immediately reads the vector to reduce it by shadow. The sequential front variant stores meaningful events and part boundaries only, but still builds and consumes a whole intermediate. Neither vector is needed by the eventual shadow refresh.

## Proposed redesign

Fold steps during submission. In `refresh_ahead`, keep one map entry per refreshed shadow with its owning reference, span and written flag. Ignore writes before its first refresh, freeze the span on the next write, and stop the entire walk at the first untaken copy, run or placed operation. Move the flagged entries into the work list. In sequential `refresh_front`, feed the existing first-touch order, part-generation and last-shadow logic directly from the loop, retaining texture-due collection in place. Keep one list lock at a time and do shadow updates after releasing it. The parallel `list_front` path already folds its events.

## Before and after

For T visited steps and S distinct shadows, the ahead path eliminates O(T) temporary event storage and the second T-element traversal. Both versions still visit T command steps and retain O(S) state. Front-mode savings scale with relevant events only. The separate large-list parallel planner has no corresponding temporary event vector.

## Expected impact and confidence

Medium to high for long submissions on wide, light-refresh configurations. `ahead_of_writes` is enabled by default when those prerequisites hold (7943–7958,8017–8019). Verify output, stop boundaries and texture ordering, then measure planner allocation bytes and submit CPU versus steps visited.
