# Share one descriptor resolution between page preparation and both address words

## Location and execution path

`crates/victory-shader/src/emit/lower.rs:3485–3497` lowers a direct buffer-view address to two host user-data words. `crates/victory-gpu/src/draw.rs:1355–1380,1390–1426,1678–1690` resolves the view for `memory_ready` and separately for each low/high value in `State::bind`.

## Current cost

One address can trigger three lookups of the same table entry during a stage bind. `view_address` resolves the descriptor and derives its title address each time. The two user-data words are just halves of that same 64-bit result.

## Proposed redesign

Pair the host words by `(slot, dword)` when building stage metadata. At bind, resolve each used address once from the current compute/graphics roots, pass it to memory preparation, and split it for the root constants. Keep zero-address handling, lifetime checks and missing-view diagnostics. Scope the snapshot to this bind so later descriptor or root changes are visible.

## Before and after

For A distinct view addresses with both halves and a readiness lookup, descriptor resolutions fall from 3A to A. The GPU still receives both words and page preparation still runs. Use compact per-stage ordinals so a lookup cache does not allocate a hash table for every draw.

## Expected impact and confidence

Medium in titles that execute many by-address stages. The duplicate resolutions are explicit. Measure eligible binds, resolutions and CPU time. Stages using ordinary root constants do not benefit.
