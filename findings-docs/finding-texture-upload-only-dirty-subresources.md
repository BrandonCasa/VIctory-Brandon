# Carry write-watch results through detiling and GPU upload

## Location and execution path

`Staging::refresh_timed` in `crates/victory-gpu/src/texture.rs:1233–1256` receives ordered 64 KiB page numbers and change generations from `watch::writes` (`crates/victory-gpu/src/watch.rs:33–38,1246–1266`). For an already initialized, write-watched texture, one newer page makes it copy all `self.read` bytes and call `Staging::detile` for the entire footprint list (1489–1538). After refresh, `PreludeUpload::finish_refresh` claims one dirty bit and `record` emits a GPU copy for every footprint (814–876, 895–916).

## Current cost

A write confined to a small mip or one array layer incurs CPU reads, detiling, staging writes and GPU transfer for unrelated subresources. An unchanged watched texture already skips the work, so the opportunity is specifically a *partially changed* large texture on repeated upload.

## Proposed redesign

Start with initialized, repeatedly refreshed 2D textures and arrays on the reliable write-watch path. Intersect newer absolute pages with the actual mip/slice storage intervals, including all users of a shared tail. Coalesce adjacent source reads and detile from checked, offset-aware slices into existing staging footprints. Use the full path for scattered or near-full coverage, 3D, on-demand residency, missing baselines and guarded memory. Merge pending footprints across refreshes and claim them for the submitting prelude. Skip a GPU copy only when that host copy still holds this staging object's last upload for the omitted footprints. Another staging object or GPU writer can invalidate that assumption even with the same host serial. Otherwise retain the full GPU upload, including first-use barriers and prefill.

## Before and after

For B staged bytes and M footprints, a qualifying change currently processes B and issues M copies. With K affected footprints occupying b bytes of conservative storage, the target is O(b) conversion and K GPU copies plus dirty-page lookup. A change within a single large mip can still require that whole mip. Small textures, shared tails and fully dirty textures gain little. A per-staging footprint index and pending bitset cost memory proportional to layout complexity.

## Expected impact and confidence

High. This removes work on both sides of a submission, and the existing journal already supplies the necessary dirty-page evidence for the scoped branch. Measure dirty-to-total footprint bytes, CPU copy/detile time, GPU copy bytes and affected frames. Validate consecutive same-frame uploads, rotated video buffers, shared tails, host replacement and tile admission before attributing any FPS gain.
