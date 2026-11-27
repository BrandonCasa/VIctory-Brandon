# Share the host root signature across graphics-state variants

## Location and execution path

`Pending::make` in `crates/victory-gpu/src/pipeline/make.rs:78–104` builds a host pipeline; first use can wait for it in `ready` (398–408). `HostPipeline::new` in `pipeline/host.rs:417–597` selects translated shader stages and constructs their binding layout. `root_signature` (1066–1103) serializes the description, creates a D3D12 root signature and copies its blob. Only afterward does `pipeline/library.rs:518–557` try loading the pipeline state object (PSO) from the driver library.

## Current cost

Two pipelines with identical shaders and stage visibility but different raster, blend or render-target state rebuild the same parameter arrays, boxed ranges, static samplers and root signature. Even a successful PSO library lookup pays this setup. Ordinary draws using an already built PSO bypass it.

## Proposed redesign

Cache the successful root signature, serialized blob and immutable binding layout per host device, keyed before serialization by the ordered, lifetime-safe identities of translated stages, visibility, compute/graphics flags and binding options. Use the cached parameter numbering when constructing each pipeline's stage bindings. Keep graphics state, PSO creation, rectangle geometry fallback and per-shader metadata distinct. Preserve device-specific sampler capability and exact blob bytes for library naming. Publish successful entries without holding a global cache lock across driver calls or leaving waiters stranded on worker failure.

## Before and after

For P pipeline builds spanning U unique binding keys, layout construction and serializer/driver calls fall from P to U after warmup, apart from concurrent misses. The cache retains O(U) entries subject to an eviction or weak-reference policy. Distinct shaders with equivalent layouts may still miss with this conservative key.

## Expected impact and confidence

High for first-use stalls in variant-heavy scenes, with no recurring gain for a fully built scene. High confidence in the repeated setup; the unknown is how many distinct PSOs share stage sets and how much the driver already deduplicates internally. Measure P/U, build time on library hits and frames that wait on compilation.
