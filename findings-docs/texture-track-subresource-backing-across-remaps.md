# Track which streamed texture subresources have valid backing

## Location and execution path

`State::heap_textures` reduces the requested level range to `mapped_bytes(address,len)` (`crates/victory-gpu/src/draw.rs:2483-2507`). A partial texture with `mapped <= host.texture_slot_mapped(index)` goes back to sleep. A delivered `TextureQueue::changed` resets the remembered count, but the periodic comparison alone misses equal-sized substitutions. `texture::mapped_bytes` uses physical section views where known. Its fallback can report zero when both ends are unreadable but a middle range is present. `Staging::refresh_timed` can detile a zero-filled partial snapshot or skip an all-zero change. `Staging::fill` can return the requested size after reading nothing.

## Current cost

At 07:00:54 UTC, 11,001 slots waited for memory and nine were partial. The runtime had made 84 partial uploads and nulled 976 previously made slots. These counters do not establish a null Xbox descriptor or the behavior of an Xbox sparse tile. A changed physical page at the same VA can leave a shader reading earlier bytes.

## Proposed redesign

1. Replace `Waiting.mapped` as a correctness key with versioned backing intervals. Under `watch::title_memory_reading`, retain the section, offset and generation for each interval from the mapping index. `inplace::shown_runs` merges adjacent VAs even across distinct backings, so use its merged runs only for coverage. For spans outside that index, determine the actual committed/readable regions throughout the range. A failed read during remap is pending. Intersect actual covered blocks with `Layout`, `Staging::part` and `resource::read_from`, including packed mip tails, planes, array slices and volume slices. Track substitutions and losses as well as gains. A complete all-zero mip is valid data.
2. Preserve the title resource shape and SRV description across residency changes. A shader's relative mip zero denotes the title view's first mip. Preserve width, height, mip count, layer/plane selection, explicit and implicit LOD, query results and filter footprint. `Lower` emits `getDimensions`, `sampleLevel` and `textureLoad` (`emit/lower.rs:11666-11686,12034-12045,12133-12149`). `tiles::show_from`, `from_base` or prefilled finer mips must never silently substitute another level. Prefer a full logical host resource. Shortened physical storage is usable only if **every** observable operation still addresses the same original level, layers and texels.
3. Keep distinct states for ready bytes, a known pending producer, an explicitly mapped/unmapped guest sparse tile and unsupported access. For ready ranges, detile the exact source bytes and copy to their original subresources. For a pending range, obtain its real bytes from xmem, the streaming completion, an alias or an earlier GPU writer, then order the consumer behind it. Native sparse storage is usable only when its per-access read/write/filter behavior matches the established guest tile semantics. A typed null is appropriate only for a known guest-null case. Where a required range has no source or established guest sparse behavior, expose the unresolved operation. Sampling a coarser mip, zero-filling holes or indefinitely waiting for an unknown producer cannot reproduce its title-visible result.
4. Keep `Staging::fill`'s byte-work counter separate from valid intervals and their source generation. Publish a footprint as ready only after its read and detile both succeed. Today `detile` marks staging dirty before its fallible walk and `claim_upload` claims even an uninitialized first upload. A failed footprint stays pending through `PreludeUpload::finish_refresh`. A known table or opaque indexed consumer needs its complete reachable coverage or equivalent sparse behavior. Commit a `texture_slot_show_after`/`show_ahead` flip only for the matching host serial, slot record and backing generation after the upload is ordered. Fence other queues and retire old versions after their readers. Same-mapping content changes use [write tracking](texture-publish-writes-across-backing-aliases.md).

## Before and after

If equally sized mip B replaces mip A, B acquires the new bytes at B's logical index. A shader selecting A still addresses A. A separate title view beginning at B can sample B as its own relative mip zero.

## Expected impact and confidence

High confidence in the periodic equal-count blind spot. After implementation, inspect a real equal-size replacement at the first consuming view and compare the guest physical runs, relative mip/layer, dimensions and returned texel. A delivered remap that already produces those values rules out this defect for that read.
