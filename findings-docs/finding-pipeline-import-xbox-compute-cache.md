# Create the compute pipeline requested by Xbox device slot 0x218

## Location and execution path

`crates/victory-gpu/src/device.rs` maps `f[67]` to `unknown_pipeline::<0x218>`, which clears the output and returns `E_NOTIMPL`. The launch log reaches at least 2,048 calls (118–124, 190–194, 225). In title build 1.0.2.1 the wrapper at RVA `0x247fd30` obtains ordinary compute bytecode via `0x86f8e0`. A cache lookup at `0x2480050` installs the same root in a packed record and invokes 0x218 at `0x24800d5`. On null, the caller immediately invokes **0x1f0** at `0x247fe66` with the ordinary description and zero extensions. Both outputs go to the title object's +0x120 field. The fork already routes 0x1f0 through `pipeline::create_compute_x` and `create_compute`.

## Current cost

Every packed request takes the fallback. The descriptor begins with the root followed by four pointer/byte-length pairs, ending at offset +0x40. The first two sampled lengths are 0x1c and 0x14, followed by code and resource metadata. This fits the split compute entries in `victory-shader/src/native.rs::fspx_program`. Casting it to `ComputeHead` would treat the 28-byte register packet as shader bytecode. A fake success would suppress the working ordinary fallback.

## Proposed redesign

1. Install a typed `create_cached_compute_x` at slot 67. Clear a valid out pointer, check the IID and root object, and read the four pairs within mapped bounds. Factor `fspx_program` into a shared reader for the register packet, code and resource metadata. Handle the short secondary packet as its own entry. Validate metadata code dwords, flags, group dimensions, resource/root types and record boundaries. Exclude trailing storage padding only when the identified format defines it. Copy or intern all data before the title call returns.
2. Build a content index of original compute containers from `victory_game::sgd5::records` and `victory_shader::native_programs`. Match the native instruction bytes and verify wave mode, dimensions, root layout and used-resource declarations. Import the original container for a unique equivalent match. This retains the binding plan, shader role and builder classification used by `pipeline::translate` and `of_builder`. A complete standalone split program can instead enter the same normalized compute importer. An ambiguous or incomplete record returns a true miss, allowing the title to supply the exact container through 0x1f0.
3. Send the canonical shader to the ordinary compute creation helper and `hand_out`. Attach the normal PSO interface, refcount, used-key tracking, early scheduling and translated host bindings. Equivalent packed and ordinary requests share the pending host recipe. Host PSO failures retain their actual failure state. Workers never retain title cache-node pointers.

## Before and after

A supported seed returns the requested compute PSO directly. Its first dispatch uses the same group dimensions, root parameters and D3D12 shader as ordinary creation. The warning count measures failed imports, not lost dispatches.

## Expected impact and confidence

The title can reuse its cached compute pipeline without changing the bytes its shader writes. The four-entry layout is inferred. The caller's immediate ordinary fallback establishes the result independently. After implementation, a matched packed and ordinary request should write identical bytes for identical inputs.
