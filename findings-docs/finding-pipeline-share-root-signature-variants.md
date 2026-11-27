# Share the host root signature across graphics-state variants

## Location and execution path

`Pending::make` in `crates/victory-gpu/src/pipeline/make.rs:78–104` builds a host pipeline; first use can wait for it in `ready` (398–408). `HostPipeline::new` in `pipeline/host.rs:417–597` selects translated shader stages and constructs their binding layout. `root_signature` (1066–1103) serializes the description, creates a D3D12 root signature and copies its blob. Only afterward does `pipeline/library.rs:518–557` try loading the pipeline state object (PSO) from the driver library.

## Current cost

Two pipelines with identical shaders and stage visibility but different raster, blend or render-target state rebuild the same parameter arrays, boxed ranges, static samplers and root signature. Even a successful PSO library lookup pays this setup. Ordinary draws using an already built PSO bypass it.

## Proposed redesign

Key a bounded per-device cache by the ordered identities of the interned `Shader` objects, stage visibility and compute/graphics mode. Binding mode is fixed for the process. Read the static-sampler capability once per device. On a hit, share the root signature and immutable `Stage` array and borrow the original serialized bytes for PSO naming. This avoids hashing DXIL or copying binding vectors on a hit. Build misses outside the cache lock and publish only completed entries, accepting duplicate concurrent builds so a lost worker cannot strand another. Eviction releases the cache’s references while live PSOs keep their root and stages. Keep graphics state and rectangle fallback in the individual PSO.

## Before and after

For P pipeline builds spanning U unique binding keys, layout construction and serializer/driver calls fall from P to U after warmup, apart from concurrent misses. The cache retains O(U) entries subject to an eviction or weak-reference policy. Distinct shaders with equivalent layouts may still miss with this conservative key.

## Expected impact and confidence

High for first-use stalls in variant-heavy scenes, with no recurring gain for a fully built scene. High confidence in the repeated setup; the unknown is how many distinct PSOs share stage sets and how much the driver already deduplicates internally. Measure P/U, build time on library hits and frames that wait on compilation.
