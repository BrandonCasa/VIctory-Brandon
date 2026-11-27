# Preserve byte offsets for buffer views between host elements

## Location and execution path

`buffer_srv` and `buffer_uav` in `crates/victory-gpu/src/descriptor/host.rs:3882-3947,3989-4069` convert title `FirstElement` and stride to a byte address. `view_home` tries `shadow::exact(start,len).unwrap_or(shadow)`. If the fallback shadow cannot express the requested start, the SRV path returns `None` on `offset % element != 0`.

At 06:56:11 UTC the log gives `offset=7 element=4`. By 07:00:54 it reports 59,212 view-relative copies and zero counted host-refused views. The early return precedes that refusal counter. `shadow::exact` rejects a non-dword-aligned absolute address or more than 64 MiB and can fail allocation (`shadow.rs:5663-5704`). The logged offset is relative to a host copy, so it does not identify which rejection occurred.

## Current cost

Even a fully backed SRV can become null. With writable aliases, independent copies can also disagree about the same title byte before CPU copy-back.

## Proposed redesign

1. Resolve object and placed views once into the canonical backing/version, checked `base + FirstElement * unit`, element count, stride, format, component mapping, counter if present and access type. Pass this same result to descriptor creation, table and indexed consumers, `buffer_copy`, `Shadow::refresh` and write tracking. Preserve typed conversions, structured indexing, raw byte addressing and title VA metadata. Validate the original view separately from what D3D12 permits.
2. For a read-only, stable interval, use the existing legal descriptor or an aligned exact copy of the accessible bytes with `FirstElement=0`. Permit byte-granular source addresses. A large legal view can use dedicated storage. Preserve guest-defined bounds and component swizzle in the descriptor or shader, including loads at the edges. A copied snapshot is valid only until a write ordered before its reader. Bytes of host padding are never guest data. A short host read is a pending dependency, not permission to substitute a null or zeros.
3. For views that may write or overlap an active writer, resolve all aliases onto **one canonical writable GPU allocation**. Choose direct typed descriptors where representable or a shared aligned raw descriptor with per-view byte bias. Translate loads/stores at the exact effective offset, honoring typed conversion, component/write masks, counter identity and width. Preserve native memory barriers, coherence scope, atomics' returned old value and compare/exchange semantics. A masked read-modify-write of a containing dword can clobber a concurrent neighboring store. Use native operations with equivalent guarantees, or one coordinated lowering covering *all* conflicting accesses and proven equivalent for their allowed interleavings. An independent staging copy, two stores for one atomic or a cross-dword atomic without that proof cannot implement the operation. Report that access as unsupported if the backend lacks an exact form.
4. For an ordered, nonconcurrent read needing a different presentation of those bytes, copy **after** the preceding writer and before the read, with resource barriers or a cross-queue fence. Preserve byte identity for a subsequent GPU copy or title CPU read at its guest fence. Pin source, result and descriptor versions until their readers finish. Do not treat the set of possibly written views as evidence every byte changed.

## Before and after

A four-byte element starting seven bytes into its host mirror reads bytes 7 through 10 as element zero. A supported write through another view of that interval becomes visible on the same backing in the required order.

## Expected impact and confidence

High confidence in the early-null path. After implementation, compare the logged view's first and last accessed elements and an overlapping writer against the original checked title range. An unused or guest-invalid descriptor would rule it out for that shot.
