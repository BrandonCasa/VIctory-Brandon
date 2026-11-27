# Create the compute pipeline requested by Xbox device slot 0x218

## Location and execution path

`crates/victory-gpu/src/device.rs` maps `f[67]` to `unknown_pipeline::<0x218>`, which clears the output and returns `E_NOTIMPL`. The launch log reaches at least 2,048 calls (118–124, 190–194, 225). In title build 1.0.2.1 the wrapper at RVA `0x247fd30` obtains ordinary compute bytecode via `0x86f8e0`. A cache lookup at `0x2480050` installs the same root in a packed record and invokes 0x218 at `0x24800d5`. On null, the caller immediately invokes **0x1f0** at `0x247fe66` with the ordinary description and zero extensions. Both outputs go to the title object's +0x120 field. The fork already routes 0x1f0 through `pipeline::create_compute_x` and `create_compute`.

## Current cost

Every packed request takes the fallback. The descriptor begins with the root followed by four pointer/byte-length pairs, ending at offset +0x40. The first two sampled lengths are 0x1c and 0x14, followed by code and resource metadata. This fits the split compute entries in `victory-shader/src/native.rs::fspx_program`. Casting it to `ComputeHead` would treat the 28-byte register packet as shader bytecode. A fake success would suppress the working ordinary fallback.

## Proposed redesign

1. Install a typed `create_cached_compute_x` at slot 67. On the identified title build, scope the ordinary compute description to the cache lookup at RVA `0x247fe25`. At that call site the wrapper has written the shader pointer/length at `rsp+0x48`, the root at `rsp+0x40`, and holds the lookup key in `rdx`. Keep this description borrowed only through the lookup on its originating thread. Restore the previous scope on nested returns.
2. At slot 67, clear a valid output, validate the IID and match the root and current lookup to the scoped request. Pass the ordinary description to `create_compute`, which copies or interns the original shader before returning. This retains the shader role, bindings and builder classification. A call outside that validated wrapper, or one with an incomplete description, returns a cache miss. The existing fallback supplies its own ordinary recipe.
3. Share the canonical `Pending` across equivalent packed and ordinary requests in `hand_out`. Each returned title object retains its normal interface and refcount. Preserve used-key tracking, early scheduling and actual host compilation failures.

## Before and after

A lookup with a matching ordinary description returns the requested compute PSO directly. Its first dispatch uses the same group dimensions, root parameters and D3D12 shader as ordinary creation. The warning count measures failed imports. Only the subsequent fallback and dispatch establish whether a pass ran.

## Expected impact and confidence

The title can reuse its cached compute pipeline without changing the bytes its shader writes. The four-entry layout is inferred. The wrapper supplies the exact ordinary description before it attempts the packed lookup, so this build needs no guessed packet interpretation. After implementation, a matched packed and ordinary request should write identical bytes for identical inputs.
