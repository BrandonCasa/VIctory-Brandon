# Translate a same-page wide scalar load once

## Location and execution path

`crates/victory-shader/src/emit/lower.rs:6699–6716` selects `memory_smem` for scalar loads classified as title memory. At lines 3906–3949 it splits x8 and x16 loads into groups of four dwords. Each group invokes `memory` (3569–3647) before emitting a payload load. This path generates code executed by the GPU.

## Current cost

Each chunk emits page-index arithmetic, a clamped directory load, a conditional missing-page report and descriptor-handle work. When all chunks use the same 64 KiB page, the directory entry and backing handle are identical. The payload loads are already vectorized.

## Proposed redesign

Reuse the first chunk’s lookup for x8/x16 only when the effective address, including both offsets and carry, is statically known to keep the 32/64-byte span in one 64 KiB page. Sufficient alignment of the final address proves this even when the page number is unknown. Emit the original vector payload loads and advance only the translated byte offset. Keep the clamped page number, null/coherent handle and missing-page reporting semantics. The host reads and clears hashed fault slots asynchronously, so retain the original fault stores if coalescing them would change reporting. Leave unresolved addresses on the current lowering, avoiding a runtime branch, duplicate shader paths and result merges.

## Before and after

For a contained load with G four-dword chunks (2 or 4), directory lookups and fault tests can fall from G to 1. The G payload loads remain. Dynamic specialization adds a test and alternate code. Narrow and non-MEMORY scalar loads are unaffected.

## Expected impact and confidence

High potential in shaders that repeatedly execute wide raw scalar loads. The redundant emitted sequences are explicit. Title instruction mix, driver optimization and branch cost determine GPU savings. Inspect final ISA and time shader workloads containing these instructions.
