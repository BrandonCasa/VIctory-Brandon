# Publish builder landing writes before the GPU reads numbered shadows

## Problem

A generic `Step::Host` hides `Memory::land` from refresh deduplication. A refresh can stage old data before landing, then suppress the upload of a newly filled arena range. The title batches up to 16 structures at EXE RVA `0xea6c60`, and `XboxBvhBuilder.dll` records work per entry at `0x14700`. One frame-wide refresh cannot establish freshness for all entries.

## Change

Represent landing as both a guest-memory reader and a producer of directory and arena writes. The existing `defer_host` records an empty read range and `blocked_by` skips it. Before reading the eight-byte header, make earlier ordered writes to that header visible. Use its decoded size to resolve dependencies on the payload. Wait outside the memory mutex and revalidate the header and mapping after a wait. Keep a command boundary when an earlier part of the same submission produces the source.

An unchanged header establishes size. Reuse a previous landing only when the source mapping and payload are also known unchanged. Otherwise take the packed interval again, including on replay. Have `place`, `take` and `gone` report the affected directory and arena ranges. Invalidate coverage and bind the version produced for the consuming builder, honoring the full resend requested by `take`. The [submission finding](finding-submit-fold-refresh-plan-in-one-pass.md) describes how to preserve both versions when a later landing overlaps an earlier reader.

## Expected result

When a second structure fills an already numbered arena after recording, the first submit and replay both stage its new bytes and matching directory entry before the builder reads them.

[Deferred optimization](deferred/finding-memory-defer-new-numbered-shadows-in-bulk.md)
