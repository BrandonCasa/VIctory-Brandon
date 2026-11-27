# Publish texture writes across every mapping of their backing

## Location and execution path

`draw::State::heap_textures` unstages an uploaded bindless texture, then compares `texture::fingerprint` after intervals of 4, 12, 40, 120 and 400 title frames (`crates/victory-gpu/src/draw.rs:2432-2483,2728-2767`). For large textures the fingerprint samples three 16 KiB windows. The final unchanged sample drops the queue entry. The sampled bytes at recording can also differ from the upload worker's eventual snapshot.

## Current cost

The log recorded 73 changed fingerprints. Slot 42193 had changed at its 52-frame check and had three blocks visible through other addresses. Slot 74142 changed after 41 frames with nine such blocks. Some records had no known aliases. The DirectStorage physical-write counter ended at zero. Those comparisons show that the sampled CPU bytes changed. They do not identify the writer or the bytes the GPU consumed. A change outside the fingerprint windows can survive indefinitely.

## Proposed redesign

1. Own content subscriptions in the allocation, independent of `Staging` and individual heap slots. Record backing identity, mapping generation, subresource spans, host serial and subscribing table/indexed record generations. Use the section/offset relationships in `inplace::{title_memory_view,views_of_section,shown_elsewhere}` for aliases, and allocation generation plus VA for private memory. Track availability separately via [mapped coverage](texture-track-subresource-backing-across-remaps.md).
2. Extend the existing `watch` journal and xmem/DirectStorage completion hooks to publish dirty backing intervals from watched stores, writes through aliases and completed runtime writes. The guard fault precedes the CPU store. An opened page stays potentially dirty. A stable snapshot establishes the bytes for that version only. Before a later consumer can reuse it, rearm tracking on every writable alias or synchronize with its producer again. Publish by backing identity even for high-fanout pages that `views_of_section` omits. Reattach on alias remap. Coalesce by page generation without taking renderer locks inside the fault/mapper. Each heap consumes changes with its own cursor. Today `texture_slots_remapped` drains process-wide ranges and `page_written` enqueuing is gated. Use the [physical-write path](xmem-notify-aliases-of-written-physical-byte-ranges.md) for completed DirectStorage page writes. On journal overflow invalidate affected subscriptions and refresh them on demand.
3. Before the consuming GPU operation, compare the version required by the title's writes, list order and fences to the installed version. Include all reachable bindings for opaque indices unless the shader proves a narrower set. For memory the journal cannot cover, read the complete relevant mapped span into stable storage and compare its bytes to the previous snapshot. The digest can accelerate comparisons, but equality is authoritative only when the journal or actual byte comparison establishes it. A failed read, remap in progress or outstanding ordered writer keeps the consumer pending. If dependency ownership cannot be resolved, surface the unsupported access. Reusing an older view or zero-filling failed reads changes the result.
4. Pass the exact source snapshot, version and footprint selection to `upload_from`, `PreludeUpload` or the ahead worker. Preserve title layout, view range, swizzle and typed interpretation. A copy staged on another queue signals its fence before dependent execution. Check host serial, mapping and descriptor generations before publishing. Acknowledge only the uploaded version. A newer write remains dirty. Retain staging and prior descriptor/resource versions for earlier readers. Where a GPU writer owns the bytes, transfer from its version after its barrier. A CPU snapshot may substitute only if title-visible contents and order agree. For a decoder-backed plane, use the completed frame-slot/plane snapshot established by the [decoder-write finding](texture-upload-completed-bink-frame-planes.md). The bindless subscription must outlive its staging object so that a later fill still reaches the right logical slot.

## Before and after

A material whose bytes change through a second VA now has a dependency on the new bytes even when its descriptor and original VA stay identical. Its first ordered reader sees the new texels through the original view.

## Expected impact and confidence

High confidence in the finite-sampling gap. After implementation, follow one changed block outside a fingerprint window through the first consuming shader. Matching uploaded bytes but a wrong sample would shift the diagnosis to view selection or ordering.
