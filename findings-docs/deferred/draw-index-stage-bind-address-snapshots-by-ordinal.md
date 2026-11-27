# Index stage-bind descriptor snapshots with precomputed ordinals

## Problem

`crates/victory-shader/src/emit/lower.rs:3485–3497` lowers a direct buffer-view address to two host user-data words. `crates/victory-gpu/src/draw.rs:1355–1380,1390–1426,1678–1690` resolves the view for `memory_ready` and separately for each low/high value in `State::bind`.

The paired correctness change shares one address snapshot per (slot, dword) across these three consumers. Looking up a bind-local snapshot by hash would add allocation and lookup work on the bind path even though the stage already fixes the set of keys.

## Change

Precompute compact ordinals for each distinct (slot, dword) in the stage metadata. Use those indices for the bind-local addresses and both host words so each bind avoids a fresh hash table. Reuse the same snapshot for missing-view diagnostics.

## Expected result

Each eligible stage bind fills one address entry per distinct (slot, dword) using precomputed ordinals. The correctness fix already reduces three resolutions to one per pair. The incremental saving here is direct indexing with no per-bind hash table. Prepared addresses and both bound words stay identical.

[Related correctness finding](../draw-snapshot-descriptor-addresses-per-stage-bind.md)
