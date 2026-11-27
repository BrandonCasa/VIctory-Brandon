# Keep refresh planning ordered around builder landing writes

## Problem

`Batch::refresh_front`, its parallel `ListFront` summary and `refresh_ahead` can plan through the host callback used by `Memory::land`. An address already marked refreshed can then receive new directory or arena bytes before the GPU reads it.

## Change

Carry the [landing producer](finding-memory-defer-new-numbered-shadows-in-bulk.md) and its position through all three planners. Where exact ranges become available only at submit, exclude the potentially affected directory and arena shadows from speculative coverage across landing. Apply the actual ranges after the producer at every replayed occurrence.

`Host::submit` runs every list's deferred CPU work before `card_before` uploads the accumulated changes. Disjoint landing writes can share that upload. If a later landing replaces bytes an earlier builder reads, capture each version in flight-owned staging and insert its GPU copy between the last reader of the old version and the first reader of the new one. Split the host command list at the builder boundary when both consumers occur in one recording. Keep the staging alive through the flight fence.

## Expected result

With a late arena fill between two list parts, front, parallel-front and ahead planning deliver the same directory entries and GPU bytes as ordered execution, including on replay.

[Deferred optimization](deferred/finding-submit-fold-refresh-plan-in-one-pass.md)
