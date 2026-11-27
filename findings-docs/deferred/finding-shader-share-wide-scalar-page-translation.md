# Keep wide scalar-load lowering for the captured shader set

## Problem

`crates/victory-shader/src/emit/lower.rs:6699–6716` selects `memory_smem` for scalar loads classified as title memory. At lines 3906–3949 it splits x8 and x16 loads into groups of four dwords. Each group invokes `memory` (3569–3647) before emitting a payload load. This path generates code executed by the GPU.

Each chunk emits page-index arithmetic, a clamped directory load, a conditional missing-page report and descriptor-handle work. When all chunks use the same 64 KiB page, the directory entry and backing handle are identical. The payload loads are already vectorized.

The November 27 launch log names 1,119 distinct successfully translated container hashes. Matching each BLAKE3 prefix to an original container in the EXE or `shaders.rpf`, decoding its selected GFX10 program and applying `spill::build` and `resource::track` gives 30,069 reachable x8/x16 scalar-load sites. Of these, 23,357 trace to a descriptor table and 6,712 to the resource heap. **Zero reach `memory_smem`.** For example, `CS_CopyLatestWindFieldForRenderingWithSmooth` (archive record `0xf8ee011`, hash `00167d58cc5a78d7`) loads eight dwords at native PC `0x2c` from slot 1's table at byte offset `0xa0`. The translator handles table and heap sources in `descriptor_data`.

The original `XboxBvhBuilder.dll` (SHA-256 prefix `99f803ac22c01ef3`) supplies a separate check. Its 52 embedded compute programs contain 26 such sites. The spilled-user-data rewrite eliminates 24. Both survivors read code constants. Ghidra shows the function at RVA `0x1c160` referencing the containers at RVAs `0x73b20` and `0x80ed0`. Their surviving x8 instructions are at shader PCs `0x44b0` and `0x15b0`, respectively.

## Change

Keep the existing lowering for this title. Its reachable wide loads use descriptor tables, heap views or code constants. A same-page fast path in `memory_smem` would change none of them.

## Expected result

The 1,119 captured title shaders and 52 embedded builder shaders contain no eligible wide directory reads. Their generated code stays identical.
