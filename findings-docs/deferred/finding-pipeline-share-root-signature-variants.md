# Share host root signatures across shaders with the same binding layout

## Problem

`Pending::make` in `crates/victory-gpu/src/pipeline/make.rs:78–104` builds a host pipeline; first use can wait for it in `ready` (398–408). `HostPipeline::new` in `pipeline/host.rs:417–597` selects translated shader stages and constructs their binding layout. `root_signature` (1066–1103) serializes the description, creates a D3D12 root signature and copies its blob. Only afterward does `pipeline/library.rs:518–557` try loading the pipeline state object (PSO) from the driver library.

Two pipelines with identical shaders and stage visibility but different raster, blend or render-target state rebuild the same parameter arrays, boxed ranges, static samplers and root signature. Even a successful PSO library lookup pays this setup. Ordinary draws using an already built PSO bypass it.

The compute path shows why shader identity is too narrow a cache key. In Ghidra, `game_scarlett_master.exe` (SHA-256 prefix `e95020b2fa4a721d`) calls the title's lookup helper at RVA `0x247fe25`. If its device-slot `0x218` attempt fails, the helper at RVA `0x2480050` returns null. The caller then requests a compute PSO at RVA `0x247fe66` through slot `0x1f0`. In the fork, `create_compute_x` reaches `hand_out` and eventually the same host layout builder as the graphics stream path. A failed lookup alone cannot identify a missing compute pass.

The November 27 launch log contains 543 successful compute PSO builds with 543 distinct logged stage hashes. Separately, decoding the local `VSH2` cache from translator build `6d891c16cf6f` yields 546 distinct compute DXIL payloads but only 88 tuples of rounded user-data width, ordered CBV/SRV/UAV slots and bit-exact static sampler descriptions. The most common tuple covers 154 payloads with ten different mappings from title roots to user-data words. This is a cache census. Its population can span launches.

## Change

Intern each translated stage's layout tuple once. Key a bounded per-device root cache by the ordered tuple IDs, stage visibility and compute/graphics root flags. Compare all tuple fields on hash collisions. The bounded-root setting is process-wide. Sample the static-sampler capability per device and apply its filter conversion on construction. Cache the root, its serialized bytes and the immutable parameter-index maps. Pair those maps with the PSO's actual `Arc<Shader>` on every hit. `draw.rs::State::bind` consults that shader's user-data mapping, table plan and write declarations when recording commands. Reusing an entire `Stage` from a different shader would bind the wrong values even where D3D12 accepts the root.

Build misses outside the lock and publish completed entries. Live PSOs retain the shared root and maps across eviction. Use the actual stage bytecode and graphics state for the PSO itself, including the rectangle fallback.

## Expected result

For P builds spanning U ordered layout keys, root serialization and creation can approach U calls when the entries stay resident, plus concurrent misses. Stage classification runs once per translation. Comparing the translated sampler fields before host filter conversion gives a conservative key and can leave some equivalent roots separate.
