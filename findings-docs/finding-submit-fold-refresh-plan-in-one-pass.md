# Keep refresh planning ordered around builder landing writes

## Problem

`Batch::refresh_front`, its parallel `ListFront` summary and `refresh_ahead` can plan through the host callback used by `Memory::land`. An address already marked refreshed can then receive new directory or arena bytes before the GPU reads it.

## Change

Carry the [landing producer](finding-memory-defer-new-numbered-shadows-in-bulk.md) and its position through all three planners. Keep dependent refreshes after the producer. Where exact ranges become available only at submit, exclude the potentially affected directory and arena shadows from speculative coverage across landing. Apply the actual ranges when it executes. Repeat the dependency at every replayed occurrence and preserve list-part order.

## Expected result

With a late arena fill between two list parts, front, parallel-front and ahead planning deliver the same directory entries and GPU bytes as ordered execution, including on replay.

[Deferred optimization](deferred/finding-submit-fold-refresh-plan-in-one-pass.md)
