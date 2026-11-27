# Carry write-watch results through detiling and GPU upload

## Problem

`Staging::refresh_timed` in `crates/victory-gpu/src/texture.rs:1233–1256` receives ordered 64 KiB page numbers and change generations from `watch::writes` (`crates/victory-gpu/src/watch.rs:33–38,1246–1266`). For an already initialized, write-watched texture, one newer page makes it copy all `self.read` bytes and call `Staging::detile` for the entire footprint list (1489–1538). After refresh, `PreludeUpload::finish_refresh` claims one dirty bit and `record` emits a GPU copy for every footprint (814–876, 895–916).

A write confined to a small mip or one array layer incurs CPU reads, detiling, staging writes and GPU transfer for unrelated subresources. An unchanged watched texture already skips the work, so the opportunity is specifically a *partially changed* large texture on repeated upload.

## Change

For initialized 2D textures with verified producer coverage, intersect changed absolute pages with each mip and slice, including all users of a shared tail. Read the full tiled spans needed by each affected footprint and coalesce overlaps. Preserve layout-relative offsets: `Layout::walk` indexes from the texture origin and checks `bytes_from(mip)`. A compact slice requires translating those offsets. Merge pending footprint masks across refreshes and retain immutable staged bytes for each in-flight copy. Clear only the version successfully queued for that footprint so a failed prelude or newer write remains pending. Omit a GPU copy only when the destination still contains the last upload for that footprint. Keep the full path for missing baselines, near-full coverage, guarded memory, 3D and on-demand residency. Preserve first-use barriers. Apply prefill only to finer levels whose bytes are established for this allocation. Matching smaller mips alone cannot identify those finer texels.

## Expected result

For B staged bytes and M footprints, a qualifying change currently processes B and issues M copies. With K affected footprints occupying b bytes of conservative storage, the target is O(b) conversion and K GPU copies plus dirty-page lookup. A change within a single large mip can still require that whole mip. Small textures, shared tails and fully dirty textures gain little. A per-staging footprint index and pending bitset cost memory proportional to layout complexity.

[Related correctness finding](../finding-texture-upload-only-dirty-subresources.md)
