# Reuse stable texture digests and hash shared mip prefixes once

## Problem

In `crates/victory-gpu/src/texture.rs:1353–1368,1396–1408`, `Staging::refresh_timed` on the copy-through path and `refresh_unlocked` on a successful private read compute `digest` and detile changed bytes. `keep_whole` (1466–1485) passes eligible full single-slice textures to `texstore::keep` in `crates/victory-gpu/src/texstore.rs:174–184`. The store requires a folder configured through `VICTORY_TEXTURE_STORE` (88–97).

The store hashes the same full snapshot again before checking its process-wide set of seen blobs. This happens on the refresh thread even when another staging object already offered identical content and no store job will run. The worker separately derives smaller-mip keys from prefixes (197–220).

## Change

Pass the stable buffer's digest through `keep_whole`. Scope reuse to the same conversion layout and host footprints: identical tiled bytes can detile differently. Use that layout-qualified identity for both deduplication and the stored blob. Treat old entries lacking the required identity as cache misses. For live guest memory, copy the completed producer version into owned storage first, hash that snapshot, and give the same bytes to the worker. The mapping guard keeps the VA valid while another CPU writer can still change its contents between a hash and a later clone. On the worker, walk the eligible levels in reverse. For eligible mips, `Layout::bytes_from` is the maximum byte extent of the remaining mip suffix (`victory-formats/src/texture.rs:607-625`), so this already visits prefix lengths in nondecreasing order. Advance the four digest lanes over complete 64-byte blocks. At each distinct length, finalize a copy with its padded tail and length, including an all-zero tail block at an exact multiple of 64. Reuse that digest for equal lengths and emit keys in the original level order. Keep the ordinary digest for a single prefix.

## Expected result

For B eligible stable bytes, the producer removes B bytes of duplicate hashing. Given L eligible prefixes of lengths pᵢ, the worker currently hashes Σ(⌊pᵢ/64⌋ + 1) blocks. The shared pass hashes ⌊max(pᵢ)/64⌋ full blocks plus one tail block per distinct length. Visiting the existing key list takes O(L) time with constant extra hash state. Accepted jobs still copy and retain the tiled payload.
