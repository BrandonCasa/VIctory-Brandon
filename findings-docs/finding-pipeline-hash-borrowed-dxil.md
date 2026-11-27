# Hash translated DXIL without copying each shader into a temporary vector

## Location and execution path

`HostPipeline::new` in `crates/victory-gpu/src/pipeline/host.rs:604–624,675–681,749–785` derives a driver-library name for compute, mesh and graphics pipelines. The translated shaders own their DXIL throughout the call. `code_bytes` (1014–1020) converts each pointer/length back into an owned vector; `library::name` in `pipeline/library.rs:507–515` immediately consumes borrowed slices.

## Current cost

Every nonempty shader passed through this helper causes an allocation and a full bytecode copy solely to feed the name hash. This happens before a library hit can return. A rejected rectangle geometry stage triggers a second name calculation for the fallback PSO. Generating or compiling the shader is separate from these copies.

## Proposed redesign

Derive both the driver bytecode descriptor and the name input from the translated shader's owned slice. Supply an empty slice for a missing stage and borrow the generated geometry shader for the duration of the hash. Retain the identical sequence of stage bytes, length prefixes, pipeline kind, root blob and state text so warm caches still hit. No pointer-derived slice with an unconstrained lifetime is needed.

## Before and after

For S nonempty stages totaling B bytecode bytes per name, both paths hash B bytes. The change eliminates S temporary allocations and B bytes copied. Peak temporary bytecode falls from O(B) to O(1) for the bounded number of stage references.

## Expected impact and confidence

Medium for PSO creation bursts, below avoiding repeated root-signature builds when layouts are shared. High confidence in the copy removal. Compare bytecode bytes copied, allocation count and time on warm-library builds, and verify identical names for compute, mesh and both rectangle paths.
