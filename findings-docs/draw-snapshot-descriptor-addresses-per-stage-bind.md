# Use a coherent descriptor address throughout a stage bind

## Problem

`draw.rs::State::memory_ready` and the two user-word bindings read the descriptor independently. A rewrite between reads can prepare one page and give the shader another address. Nine compute shaders in the original shader set use fixed address pairs. `CS_RaytraceVBuffer_RayBinning_ShaderBinning_Mixed_WithFar` reads three addresses in slot 1 at dwords 8, 32 and 56.

## Change

Snapshot the complete descriptor address once per (root slot, descriptor dword) per stage bind. When `State::tables` materializes or reuses a snapshot for the same entry, take the address from the descriptor generation retained by that snapshot. Otherwise resolve the live record once at the bind. Use that value for page preparation and both low/high user words. Preserve zero addresses, lifetime checks and acceleration-structure SRVs that store the address in words 4 and 5. Discard the snapshots after the bind.

## Expected result

For two offsets in the same slot, each prepared address equals the 64-bit address reconstructed from its bound words. Replacing one descriptor changes the next bind and leaves the other offset intact.

[Deferred optimization](deferred/draw-index-stage-bind-address-snapshots-by-ordinal.md)
