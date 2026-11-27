# Reuse texture readback buffers across retired list recordings

## Location and execution path

`crates/victory-gpu/src/draw.rs:4458–4465` marks texture UAV writes. `crates/victory-gpu/src/host.rs:6874–6899` selects each eligible owner once per recording and calls `crates/victory-gpu/src/texback.rs:149–195` at close. `host.rs:9101–9129` creates a committed D3D12 readback resource. `texback.rs:201–244` maps it at landing.

## Current cost

Every qualifying recording allocates footprint and row vectors, asks the device for the layout, and creates a fresh committed resource, even for an identical texture shape and pitched size. Repeated draws inside a single list are already deduplicated. The default filter admits four shape/format combinations (`texback.rs:30–45`).

## Proposed redesign

Share immutable host footprints, row counts and the padded extent in a bounded per-device cache keyed by the complete host description. On the guest side, `title_texture` owns the freshly computed `Texture`. Move its sole plane's layout into the target after the existing single-plane check, avoiding a deep clone just before the temporary dies. Keep the allocation address and invalidation ticket specific to each target. Lease one exclusive readback buffer of the required size to each recording until reset or drop after all flights and landings. A list can replay after landing. Cap idle bytes, allocate on a miss without waiting for a busy lease, and perform driver allocation and release outside the pool lock.

## Before and after

For N recordings of repeated descriptions, layout queries approach the number of distinct descriptions. Committed allocations approach peak simultaneously retained buffers per size class, subject to eviction. GPU copy volume and CPU retiling stay the same. Reuse trades idle resident bytes for fewer driver allocations.

## Expected impact and confidence

High for titles repeatedly rewriting eligible textures across recordings. The committed allocation is certain on each successful path. Measure recording frequency, driver allocation time, replay lifetime, peak pooled bytes and list-close stalls.
