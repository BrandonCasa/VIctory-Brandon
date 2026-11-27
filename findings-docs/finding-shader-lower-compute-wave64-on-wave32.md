# Lower the three refused compute shaders for a 32-lane host

## Location and execution path

The log identifies an RTX 4080 with minimum and maximum wave width 32 (lines 65–66). Three shaders translate successfully, then D3D12 refuses their compute PSOs with `0x80070057` (5063–5065, 8372–8373, 10200–10201). Matching the original container hashes, recomputing the fork's shader-cache keys and inspecting the resulting cached DXIL gives the following.

| Title shader hash | Pass | Threads/group | Required DXIL wave | Shared dwords | User-data words read / static samplers |
| --- | --- | ---: | ---: | ---: | ---: |
| `02384a60fda6f2d0` | VFX triangle-clip heap relocation | 128 | 64 | 4,224 | 1 / 0 |
| `369cbe906ea72764` | GPU particle visibility | 64 | 64 | 128 | 1 / 0 |
| `2da4beb386509b77` | Alpha-transmission mip prefilter | 256 | 64 | 128 | 2 / 2 |

In `victory-gpu/src/pipeline.rs`, `Shader::translated` and `translate` obtain the host shader and binding plan. In `pipeline/host.rs::HostPipeline::new`, the three programs read user-data indices 5, 4 and 4/5 respectively. Each gets eight root constants at b0 in the reserved space. All three have empty additional root CBV/SRV/UAV lists. The prefilter has two static samplers. `root_signature` succeeds before `pipeline/library.rs::pipeline` attempts a library load and, on a miss, fresh host creation. No dispatch-time descriptor change can make these bytecodes' required width available.

## Current cost

In `crates/victory-shader/src/emit/mod.rs::emit`, retries can still fall through to `original.map(|(shader, _)| shader)`. This returns a 64-lane artifact even for a 32-lane adapter. The relocation and prefilter programs use lane exchanges. The visibility kernel shares LDS across 64 title threads despite having no explicit wave intrinsic in its cached DXIL. Removing the width annotation alone gives independently scheduled halves different synchronization behavior. The refused PSOs leave their original dispatches without output.

## Proposed redesign

Keep native wave64 on capable devices and proved-exact fast wave32 lowering. For a wave64 compute shader otherwise requiring 64 lanes, add an exact logical-wave lowering in `emit/lower.rs`, ahead of half-wave approximations. Preserve the original group dimensions and bindings.

1. Map flattened group index to `virtual_wave = index / 64` and `guest_lane = index % 64`. Give each virtual wave its guest PC, scalar state, EXEC/VCC as two 32-bit words, and barrier generation. Keep VGPRs thread-private. Execute scalar instructions once, including with empty EXEC. Predicate vector reads, writes and atomics by guest lane.
2. Emit a group-uniform loop selecting runnable virtual waves fairly. Compile lane-private straight-line arithmetic as one step. At a ballot, shuffle or read-first operation, stage operands by guest lane, synchronize the entire host group, read/reduce across both halves, and synchronize before reusing scratch. Preserve order at dependent LDS instructions and the memory scope of guest fences. Keep host barriers outside every divergent branch. Finished waves and inactive lanes still participate in host synchronization.
3. At a guest group barrier, park that virtual wave and schedule its peers until all required waves reach the same generation. For the 64-thread visibility kernel there is only one logical wave. No peer scheduler is needed. Reuse scratch across instructions and check total shared storage against host limits. The largest original footprint is 16.5 KiB. An unrepresentable program returns an explicit translation failure.
4. Emit matching entry-point and PSV metadata with a supported width. On fresh and cached shader returns, reject any required width outside the adapter range. Bump `TRANSLATOR_VERSION` and the compatible warm/refusal fingerprint. The three cache key prefixes for the observed build were `ff66b86b47720552`, `abd315b028f6f932` and `886567e838680907`.

## Before and after

D3D12 receives shaders it can execute while the title still sees 64-lane results, including lane 31 reading lane 32. The first dispatch writes relocation offsets, the second writes particle visibility, and the third stores prefiltered transmission. General lowering may cost GPU time. Proved-exact fast paths retain their current cost.

## Expected impact and confidence

All three artifacts have a concrete capability conflict. Fixing it should restore these effects where their dispatches run. A second driver error could still surface. After implementation, compare all three outputs against native wave64 or an instruction-level reference with different data and active masks on either side of lane 32.
