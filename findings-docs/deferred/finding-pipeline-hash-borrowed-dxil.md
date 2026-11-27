# Build pipeline library keys on demand from borrowed DXIL

## Problem

`HostPipeline::new` in `crates/victory-gpu/src/pipeline/host.rs:604–624,675–681,749–785` derives a driver-library name for compute, mesh and graphics pipelines. The translated shaders own their DXIL throughout the call. `code_bytes` (1014–1020) converts each pointer/length back into an owned vector; `library::name` in `pipeline/library.rs:507–515` immediately consumes borrowed slices.

Every nonempty shader passed through this helper causes an allocation and a full bytecode copy solely to feed the name hash. This happens before a library hit can return. A rejected rectangle geometry stage triggers a second name calculation for the fallback PSO. Generating or compiling the shader is separate from these copies.

## Change

Give `library::pipeline` a lazy name builder and pass it through the compute, graphics and stream helpers. Resolve `library()` before building or widening the name. Return the serializer's owning blob from `root_signature` and borrow its bytes for creation and naming, eliminating `bytes.to_vec()`. Hash slices borrowed from the translated shaders and optional rectangle geometry shader. Preserve the kind, root bytes, stage order, empty stages, length prefixes and state text. Build `g.describe()` on first key use and reuse it if the driver rejects the rectangle variant. The fallback still gets its distinct graphics key.

## Expected result

Without a library, creation needs no key hash, key-specific state formatting or UTF-16 name. With a library, S nonempty stages totaling B bytes still contribute B bytes to each hash, but the S temporary vectors and B copied bytes disappear. Driver creation and cache identity follow the same paths.
