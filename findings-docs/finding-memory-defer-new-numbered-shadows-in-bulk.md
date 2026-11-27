# Publish builder landing writes before the GPU reads numbered shadows

## Problem

A generic `Step::Host` hides `Memory::land` from refresh deduplication. A refresh can stage old data before landing, then suppress the upload of a newly filled arena range. The title batches up to 16 structures at EXE RVA `0xea6c60`, and `XboxBvhBuilder.dll` records work per entry at `0x14700`. One frame-wide refresh cannot establish freshness for all entries.

## Change

Represent landing as an ordered producer in each recording. Have `place`, `take` and `gone` report affected directory and arena ranges. Invalidate earlier coverage for those ranges, then refresh the new bytes before the consuming GPU prelude. Honor the full resend requested by `take`, and exclude pending GPU writes. Pair this with the [front and ahead planning change](finding-submit-fold-refresh-plan-in-one-pass.md).

## Expected result

When a second structure fills an already numbered arena after recording, the first submit and replay both stage its new bytes and matching directory entry before the builder reads them.

[Deferred optimization](deferred/finding-memory-defer-new-numbered-shadows-in-bulk.md)
