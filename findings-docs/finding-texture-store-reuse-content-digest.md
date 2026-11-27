# Pass the existing digest of a stable texture snapshot to the texture store

## Location and execution path

In `crates/victory-gpu/src/texture.rs:1353–1368,1396–1408`, `Staging::refresh_timed` on the copy-through path and `refresh_unlocked` on a successful private read compute `digest` and detile changed bytes. `keep_whole` (1466–1485) passes eligible full single-slice textures to `texstore::keep` in `crates/victory-gpu/src/texstore.rs:174–184`. The store requires a folder configured through `VICTORY_TEXTURE_STORE` (88–97).

## Current cost

The store hashes the same full snapshot again before checking its process-wide set of seen blobs. This happens on the refresh thread even when another staging object already offered identical content and no store job will run. The worker separately derives smaller-mip keys from prefixes (197–220).

## Proposed redesign

Carry the computed digest alongside the stable read buffer through `keep_whole`. Use exactly its current blob component for the seen check and job filename. Keep the current digest path for live guest memory in `refresh_timed` (1371–1384), where a remapping guard alone does not establish byte-for-byte stability across the two reads. Preserve the prefix-key hashing, eligibility checks and owned job payload.

## Before and after

For B total eligible bytes on the stable-copy paths, this removes B bytes of hash input, with O(1) additional state. Both versions still detile changed bytes and copy accepted jobs. Unchanged staging content already exits before the store and gains nothing.

## Expected impact and confidence

Medium for streaming with the optional store and stable reads, zero when the store is disabled. High confidence that the input and algorithm match on these paths. Measure eligible bytes, refresh-thread hashing time and stored-blob identity under copy-through and remapping workloads.
