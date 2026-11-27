# Translate the full title view index into a resident host descriptor

## Location and execution path

`HostHeap::new` reserves the last host slot for null and 262,144 slots for dynamic tables (`crates/victory-gpu/src/descriptor/host.rs:1557-1595,1629-1776`). The log reports 1,245,184 title slots, a host limit of 1,000,000 and no raw twins. The direct mirror ends at index 737,854. Physical slots 737,855 through 999,998 are scratch. `HostHeap::mirror` refuses a logical slot past the fitted count, and `descriptor::sync_heaps` clips its raw-write scan to that count.

`Lower::heap_index` computes a title byte offset and shifts by five. `Shader::heap_handle_at` clamps to physical slot 999,999. A valid title index of 800,000 can read a scratch descriptor. Index 1,100,000 goes to null. Explicit tables sometimes escape this via `State::tables`, which snapshots them. A dynamically chosen per-lane index does not. Separately, `Lower::heap_view_address` reads **two title VA dwords** from the root SRV at `VIEW_ADDRESSES + 8 * slot`, and the stride lookup reads at `4 * slot`. Both clamp the logical index to `SLOT_STRIDES_COUNT-1`. Mirroring never populates a skipped high slot's metadata.

## Current cost

A valid index can resolve to unrelated draw data or a null handle. The log's 79 recording summaries report zero dynamic-table refusals, peaking at 11,008 table requests per frame. They establish neither high-index shader use nor single-operation capacity exhaustion.

## Proposed redesign

1. Split CPU logical records from physical slots. In `Heap::host`, `mirror`, `note_buffer`, `sync_heaps` and `make_waiting`, retain the full title range, including slot rewrites, raw record copies, resource release and host replacement. Per logical generation keep the view type, format, swizzle, raw/structured mode, stride, allocation serial, subresource range, both VA words and readiness. Share physically equivalent descriptors. Reserve scratch and permanent, correctly typed guest-null descriptors separately.
2. Bind a versioned map through the existing host-owned `SLOT_STRIDES` root SRV, including for indexed texture-only stages. Extend DXIL declarations, root signatures and `State::bind` together. Use the same checked logical origin for handle lookup, stride and the two-word pointer. Keep title VA metadata meaningful even for a valid AS record that supplies only an address. Snapshotting must publish both words of one generation atomically from the GPU reader's perspective.

   ```text
   logical = title_byte_offset >> 5
   entry = lookup_checked(logical, origin, logical_extent)
   if entry is a valid binding:
       handle = createHandleFromHeap(entry.slot_for(access_form), false, nonuniform)
   ```

   Carry the trusted origin with its computed index through select/phi and per-lane arithmetic in `Lower::handle`, or translate the incoming arms before merging them. Two tables may use the same relative index for different descriptors. Replace `raw_twin`'s `umin(index, first-1) | first` with the matching physical variant when that mode is active. Preserve DXIL annotation and lane divergence. Keep host-created memory-page and root scratch indices physical. Fresh table arguments currently use `first * 32`. Give their roots distinct snapshot origins and bounds, with relative indices mapping to the reserved physical run and matching title metadata. Validate the origin separately from guest-controlled offsets. Sampler heaps have their own indexing. Version the shader translation ABI, root variants and persisted/warm PSOs.
3. Honor the original descriptor observation point. Preserve recorded snapshot tables and capture execution-visible logical writes before the dependent commands, including delayed texture flips and CBV producers. Share unchanged map pages between immutable execution versions. A slot rewrite copies only the affected page and mapping path. Pin physical slots and bindings until fences complete on each queue, allocating a new physical slot when an older reader still uses the old one. Bind a new map root at an intervening producer/consumer boundary. On resubmission retire the previous use or record against fresh storage. After a physical heap switch rebind affected root tables.
4. A known guest-null access gets its defined class/dimension-compatible null result. An unresolved valid slot is pending until publication. An invalid or uninitialized guest access whose outcome is unknown must not be accepted as faithfully reading zero or writing nothing. Keep physical bounds safe while propagating the unsupported access if it executes. For pressure, deduplicate identical views and retire older leases, or roll heaps between operations and restore bindings. A high logical index costs one physical slot per distinct representation. A conservative candidate set over capacity calls for a tighter reachability proof. Only a proved simultaneous set exceeding the clean host heap including helper slots is a hardware-limit case. Such an operation requires an equivalent descriptor-emulation path or an explicit unsupported result. Replaying parts of a side-effecting draw can duplicate writes.

## Before and after

A rewritten high slot selects its own current descriptor and pointer metadata. An older in-flight draw still sees the generation it was entitled to observe.

## Expected impact and confidence

High confidence in the address-space defect. After implementation, trace one high slot selected by a cutscene shader across a rewrite and compare the sampled value or written destination and both VA words. The cap alone cannot attribute a visual defect to that slot.
