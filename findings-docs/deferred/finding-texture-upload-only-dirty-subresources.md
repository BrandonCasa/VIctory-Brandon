# Carry write-watch results through detiling and GPU upload

## Problem

`Staging::refresh_timed` in `crates/victory-gpu/src/texture.rs:1233–1256` receives ordered 64 KiB page numbers and change generations from `watch::writes` (`crates/victory-gpu/src/watch.rs:33–38,1246–1266`). For an already initialized, write-watched texture, one newer page makes it copy all `self.read` bytes and call `Staging::detile` for the entire footprint list (1489–1538). After refresh, `PreludeUpload::finish_refresh` claims one dirty bit and `record` emits a GPU copy for every footprint (814–876, 895–916).

A write confined to a small mip or one array layer incurs CPU reads, detiling, staging writes and GPU transfer for unrelated subresources. An unchanged watched texture already skips the work, so the opportunity is specifically a *partially changed* large texture on repeated upload.

## Change

For initialized 2D textures with verified producer coverage, intersect changed absolute pages with each mip and slice, including all users of a shared tail. Coalesce adjacent reads and detile only affected footprints. Merge pending footprint masks across refreshes and claim them for the submitting prelude. Omit a GPU copy only when the destination still contains the last upload for that footprint. Keep the full path for missing baselines, near-full coverage, guarded memory, 3D and on-demand residency. Preserve first-use barriers and prefill.

## Expected result

For B staged bytes and M footprints, a qualifying change currently processes B and issues M copies. With K affected footprints occupying b bytes of conservative storage, the target is O(b) conversion and K GPU copies plus dirty-page lookup. A change within a single large mip can still require that whole mip. Small textures, shared tails and fully dirty textures gain little. A per-staging footprint index and pending bitset cost memory proportional to layout complexity.

[Related correctness finding](../finding-texture-upload-only-dirty-subresources.md)
