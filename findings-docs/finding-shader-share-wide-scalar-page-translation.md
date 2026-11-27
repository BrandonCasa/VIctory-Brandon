# Translate a same-page wide scalar load once

## Location and execution path

`crates/victory-shader/src/emit/lower.rs:6699–6716` selects `memory_smem` for scalar loads classified as title memory. At lines 3906–3949 it splits x8 and x16 loads into groups of four dwords. Each group invokes `memory` (3569–3647) before emitting a payload load. This path generates code executed by the GPU.

## Current cost

Each chunk emits page-index arithmetic, a clamped directory load, a conditional missing-page report and descriptor-handle work. When all chunks use the same 64 KiB page, the directory entry and backing handle are identical. The payload loads are already vectorized.

## Proposed redesign

Prove page containment from known alignment when possible. Otherwise specialize on a uniform same-page check only if the added branch pays off. Translate the effective base once and increment its byte offset for later chunks. Use the original lowering for crossings or address wrap. Preserve register results, coherent handles, clamp and missing-page behavior; coalesce identical fault-slot writes only when equivalent under the fault-reporting protocol.

## Before and after

For a contained load with G four-dword chunks (2 or 4), directory lookups and fault tests can fall from G to 1. The G payload loads remain. Dynamic specialization adds a test and alternate code. Narrow and non-MEMORY scalar loads are unaffected.

## Expected impact and confidence

High potential in shaders that repeatedly execute wide raw scalar loads. The redundant emitted sequences are explicit. Title instruction mix, driver optimization and branch cost determine GPU savings. Inspect final ISA and time shader workloads containing these instructions.
