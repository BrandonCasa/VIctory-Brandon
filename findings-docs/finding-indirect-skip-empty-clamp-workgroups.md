# Skip group synchronization in empty indirect clamp groups

## Location and execution path

`crates/victory-gpu/src/draw.rs:3930–3968` and `crates/victory-gpu/src/indirect.rs:577` route eligible indirect executions to a batch. `crates/victory-gpu/src/indirect/batch.rs:21,58–72,109–118,160–168,264–296` records a fixed four-group dispatch and defines its 64-lane shader.

## Current cost

The count is filled through the job table. Entirely empty groups still initialize shared state, write 64 zero costs and reach two group barriers. Their eventual path has no record, budget or diagnostic update.

## Proposed redesign

Use a group-uniform return when group base is at or above the loaded job count, before touching shared state. Keep inactive lanes in the last partially populated group participating in both barriers. Retain the recorded capacity, resource barriers, active-group budget atomics and ordering.

## Before and after

For J jobs (up to 256), four groups still launch, but only ceil(J/64) groups execute the shared-memory and barrier body. There is a small uniform test per group. Full batches save no barriers.

## Expected impact and confidence

Medium, conditional on many sparse batches on a GPU-limited path. High confidence in the redundant shader work. Measure job-count distribution and GPU timestamps for the clamp dispatch, including full batches.
