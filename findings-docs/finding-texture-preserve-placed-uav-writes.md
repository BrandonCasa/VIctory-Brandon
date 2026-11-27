# Resolve a placed texture UAV to the allocation its next reader uses

## Location and execution path

Device slots `0x230` and `0x238` call `create_placed_srv` and `create_placed_uav` (`crates/victory-gpu/src/descriptor.rs:1567-1591`). Both save the resource, view words and placement address in `View::Placed`. In `descriptor/host.rs:4507-4535,4920-4933`, `placed_texture` accepts only `View::Placed(false,..)`. `make_twinned` emits the same “placed unordered texture view not mirrored” message for either kind if resolution fails. `null_for` drops writes through a failed UAV. Independently, `draw::readable` handles texture UAV transitions and write tracking only after resolution to `View::Unordered` (`draw.rs:4389-4469`).

## Current cost

The warning reached 4,096 materialization attempts. Every logged example has address zero, a BC1/BC7 variant, SRV component-mapping value 5768 and 15 in the SRV mip-count position. `create_at_address` rejects zero. The strongest explanation is an unbacked SRV entering the shared warning. The log proves no specific dispatched store through these descriptors. The code separately excludes every real nonzero placed texture UAV.

## Proposed redesign

1. Replace `placed_texture` with a resolver shared by `make_twinned` and `readable`. Decode SRV/UAV unions separately, including the placement VA, typed format, mip, slice, plane, sample count and dimensions. Distinguish an explicit guest null from a pending placement and an unsupported access. A zero VA alone is insufficient to prove the originally intended view or result.
2. Give the writer and all matching consumers one canonical placed allocation under `resource::texture_at`, identified by backing generation, layout and subresource offsets. Reuse a compatible object-backed target via `TARGETS_AT`/`alias_of`. Promote a read-only host resource to writable storage only after preserving existing contents. Carry masked channels, sample index, typed conversion and write/atomic semantics through shader lowering. For example, `emit/lower.rs:11570-11590` rejects mip-selected stores and some component masks. Implement the exact requested operation where encountered or propagate unsupported. A full-channel store or store to mip zero would change the result.
3. `note_buffer` and `State::heap_textures` primarily prepare sampled textures. Add writable candidates for computed heap indices and track writes at the actual draw/dispatch. Aliases simultaneously accessible within that operation must share coherent writable storage. Record dependent UAV barriers, transitions and interqueue waits. Pin the host copy, view generation and any transfer buffer through the last consumer. A later SRV must resolve to those bytes with the title's component mapping, format, mip/slice range and filtering. `texture::alias` already handles certain byte-compatible reinterpretations, including six-part format 42 to 67 copies in this log.
4. If the producer actually writes encoded BC blocks, keep the block bytes authoritative (8 bytes for BC1, 16 for BC7). Represent the block store with a buffer or unsigned-integer UAV of the correct width, map the shader's block coordinates, and preserve unaffected bytes and blocks. Copy the encoded blocks via the actual pitched footprint to the BC mip for sampling. A GPU byte copy or CPU consumer must observe the identical encoded bytes. Extend `HostList::texture_written` and `texback::{record,land}` beyond the current kind allowlist for a confirmed CPU reader. Retile with the title layout after the producing fence and before guest completion, while the backing generation still matches. A texel-writing operation may use an uncompressed view only if every observable conversion, sample and alias matches its original semantics. When BC bits themselves are observable, use the title's exact encoder or encoded output. If encoding or atomic alias semantics are unknown, expose that operation as unsupported rather than recompressing or dropping it.

## Before and after

A valid placed UAV store reaches the designated mip, slice and channels. The next ordered sample or byte copy sees the actual result, including unchanged neighboring bytes.

## Expected impact and confidence

High confidence in the missing writable path, low confidence the zero-address warnings caused this cutscene symptom. After implementation, follow one nonzero `0x238` record from the actual shader store to its first alias read and compare the written bytes. Absence of such a consumer falsifies attribution for that shot.
