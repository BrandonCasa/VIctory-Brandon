# Restore exact graphics PSOs at Xbox device slot 0x1c8

## Location and execution path

`crates/victory-gpu/src/device.rs` maps `f[57]` to `unknown_pipeline::<0x1c8>`. The log reaches at least 16,384 failed calls (lines 125–139, 229). The cache worker in title build 1.0.2.1 at RVA `0x24774c0` installs a root signature at node+0x40 and passes that record interior, the pipeline IID and a local output to slot 0x1c8 at `0x2477578`. Failure advances to the next record. Success inserts the returned pointer and variant data into the title cache.

At demand, `0x24799c2` calls lookup `0x2479f40`. An exact match reconstructs and AddRefs the pointer. A base hit with different state can call slot **0x1b8** at `0x247a5c2` with override records. A miss invokes the title's ordinary stream builder at `0x2479a50`, then device slot **0x2b0** at `0x2479a87`. Both paths put the PSO in the same request object's +0x38 field.

## Current cost

Every seed returns null. The same geometry can still draw through the ordinary fallback, but host creation begins on demand. The packed record contains a root and several packet/code/metadata ranges, some absent. Their addresses and lengths do not establish vertex versus mesh stage, render-target format, blend or depth state. Passing only recovered shaders to `pipeline::create_graphics` loses the host graphics state. Returning the wrong base variant can produce plausible geometry with wrong depth or color.

## Proposed redesign

1. Factor `pipeline::create_stream` into a checked recipe reader and normal PSO creation. Check each full subobject, nested array and bounded semantic string before copying it. Own the stage containers, root layout, input semantics, blend, raster, depth/stencil, topology, formats and sample settings. Use `host::GraphicsState::read` for its supported fixed-function subobjects. An unknown-size subobject currently breaks the loop. Known-size subobjects such as stream output (7) and view instancing (22) also pass through `GraphicsState::read` without being represented. Exclude recipes with unrepresented effective state from cache equivalence.
2. Replace slot 57 with `create_cached_graphics_x`. Give it an exact packed-record-to-recipe index. On this build the producer hashes node+0x18 and copies 32 bytes of variant data from node+0x20. At demand the lookup hashes its third argument, compares the key at cache-entry+0x10, and compares the 64-bit field at entry+0x20 with its second argument (the value at request object+0x28) for an exact hit. The other copied variant fields feed the override path. Capture these identities at the validated producer/lookup boundaries. Include the root-layout digest and bounded packet contents in the seed identity. Store content identities, never process addresses.
3. Populate the index using the **ordinary recipe built by the title itself**. On a cold opaque seed, return null and the proper cache-miss HRESULT. Carry the exact requested variant across the lookup and its enclosing fallback in a nestable thread-local scope. On 0x2b0 create and deliver the requested pipeline immediately, and publish its owned recipe to the matching seed. Associate only unambiguous variants. Persist the build-scoped mapping with the pipeline book and load it before boot workers. When a seed matches, return a real `Pipeline` through `hand_out`, sharing its host recipe and normal COM lifetime. This path has a correct first launch without inferred packet defaults.
4. Implement the associated 0x1b8 variant method. The caller emits tagged records at 0x14c-byte strides. Tag 2 fits the eight-target D3D12 blend description, tag 1 the raster description, and tag 0 the depth/stencil description. Bound count × stride and copy decoded fields into an owned clone of the base recipe. Preserve untouched fields and fix up owned input semantic pointers. Tag 5 adds state whose meaning is unresolved. For an unknown change, return failure and null so the same title request takes its complete ordinary stream. Never return the base object as the changed variant.
5. Use normal `known::note`, `make::early`, translated bindings and D3D12 creation for both entry points. Prioritize previously demanded PSOs and share concurrent creation by canonical recipe. The title's seed count is not a reason to block its boot worker on every driver compile.

## Before and after

Cold records can draw through the title's exact stream. Learned records return the equivalent PSO earlier, during cache population. A depth-only or blend-only variant retains its own state. [Ordered first use](finding-pipeline-retain-first-use-commands.md) keeps a valid late compile in the frame.

## Expected impact and confidence

Geometry can reach the original pass with its intended depth and blend state, including on a cold miss. The assumption is a stable cross-site identity for this build. Two same-shader variants with different depth or blend should produce the same respective pixels through cached and ordinary creation.
