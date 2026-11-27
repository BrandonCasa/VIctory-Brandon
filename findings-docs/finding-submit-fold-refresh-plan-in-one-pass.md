# Fold refresh-planning events as the submitting thread visits command steps

## Location and execution path

`crates/victory-gpu/src/host.rs:5870–5876` chooses `Batch::refresh_front` or `refresh_ahead` before the ordered list execution. On the light, wide-refresh path, `refresh_ahead` (8624–8649) gathers steps up to the first qualifying title-write boundary, then calls `written_spans` (8274–8295). The sequential front branch likewise builds a vector for `front_spans` (8047–8089,8518–8551).

## Current cost

The ahead planner writes one `Touch` for every visited step, including `Nothing` for work irrelevant to this calculation. It immediately reads the vector to reduce it by shadow. The sequential front variant stores meaningful events and part boundaries only, but still builds and consumes a whole intermediate. Neither vector is needed by the eventual shadow refresh.

## Proposed redesign

Expose incremental reducers that accept each event as the recorder walks a list. For ahead planning, preserve its stop at untaken copies, runs and placed operations; preserve exactly how a write before the first refresh is treated. For front planning, handle each part boundary, order of first touches and texture-due collection. Hold only one list mutex at a time, stage the resulting ownership references, then release locks before updating shadows. The separate parallel `list_front` already streams its per-list events (8109–8131).

## Before and after

For T visited steps and S distinct shadows, the ahead path eliminates O(T) temporary event storage and the second T-element traversal. Both versions still visit T command steps and retain O(S) state. Front-mode savings scale with relevant events only. The separate large-list parallel planner has no corresponding temporary event vector.

## Expected impact and confidence

Medium to high for long submissions on wide, light-refresh configurations. `ahead_of_writes` is enabled by default when those prerequisites hold (7943–7958,8017–8019). Verify output, stop boundaries and texture ordering, then measure planner allocation bytes and submit CPU versus steps visited.
