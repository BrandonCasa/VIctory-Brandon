# Publish complete scene structures before the title traces lighting rays

## Location and execution path

The cutscene log attributes 594.5 and 2,582.5 dropped commands per frame to the ray-tracing structure builder being off (lines 7425, 18020). One memory report shows two read dispatches per frame, zero builds and zero packed structures present (11183). The screen-radiance estimate was active at strength 0.8 (11103–11108).

The game rehydrates streamed static bottom-level structures once. It builds/refits dynamic geometry, queries its compacted size, copies it and references it from top-level instances. Probe rays, reflections and lamp/sun shadow rays traverse these structures in ordinary compute shaders using 40-bit title addresses. The fork emulates the ray/node instruction in `victory-shader/src/emit/lower.rs`. Earlier enabled runs on an AMD adapter produced interior indirect light using 52 builder programs and 53 PSOs (`docs/fork-ray-tracing.md`).

## Current cost

In `crates/victory-gpu/src/pipeline.rs`, `of_builder` recognizes a single compute shader with spilled parameters. `hand_out` adds it to a global list. `builder_runs` is off by default. When enabled, it calls `make::now` on all registered PSOs under the builder mutex. A successful check says nothing about whether a particular structure was built. A direct bind failure calls `builder_rests` for this and the next frame. Indirect execution also has independent argument, signature and bind failure exits. Resting later cannot recover a skipped one-time rehydration or undo a partial refit already written.

## Proposed redesign

1. Make the title's normal build/trace path active. Replace the global all-PSOs gate with readiness per output allocation and geometry generation. Intern builder shader identities from canonical programs in `hand_out`. Request their PSOs through the ordinary worker outside the builder mutex. At `State::dispatch` and `execute_indirect`, record stage, source/destination spans, parameter-spill bindings, dimensions or indirect arguments, list ordinal and queue. Preserve flag 0x20 thread-count dispatch, group rounding and `USER_DISPATCH_THREADS_X` in the final group.
2. Group preceding producer writes by their actual output and the consumer that first observes it. Carry static rehydration across frames when the title does. Track source generation, scratch, in-place aliases and referenced bottom levels. Bind all producer stages to a private copy-on-write version until their output is ready to be observed. Initialize preserved bytes from the preceding version. Validate PSO, bindings, [page coverage](finding-lighting-reside-bvh-inputs-before-traversal.md) and indirect inputs before each stage. Respect GPU-generated arguments and zero dispatch counts. A pending stage parks. A terminal failure discards the unpublished generation.
3. Publish at each guest-visible dependency. Order UAV writes before a subsequent GPU reader and wait on the actual producer fence across queues. A [size query](finding-lighting-publish-compacted-sizes-before-allocation.md) can publish its CPU result before the later compaction version. Final traversal takes a directory snapshot covering its top level and reachable bottom levels. Keep snapshots/pages through the last consumer fence. Keep a static bottom-level structure across camera cuts while its backing and bytes remain valid. Start a new version on an overlapping CPU/GPU write or mapping replacement. Instance-transform updates affect the top-level structure that records them.
4. Release the title's original trace and resolve once their inputs are available. Preserve real empty structures and title-issued recovery. Terminal host failure cannot become a successful ray miss. Quarantine suppresses CPU size feedback and is unsuitable for this path. The [probe resolve](finding-lighting-preserve-title-probe-resolve.md) should retain the title's diffuse output.

## Before and after

A room can currently have visible light sources but no geometry for a probe ray to hit. With the change, the probe or shadow ray sees the static room and the current moving occluder in the same shot. The older note about five shadow-mask shaders predates the existing `Lower::heap_view_address`. Use that two-word VA path with the [logical heap mapping](finding-bindless-map-full-title-heap.md). This log does not identify those five as active.

## Expected impact and confidence

Surfaces outside direct sun can receive the title's indirect light, and moving objects can affect shadows and reflections where the shot issues those passes. This assumes exact translation of the shot's trace shader. After implementation, verify a ray against a newly refitted moving occluder changes the title's shadow or probe result.
