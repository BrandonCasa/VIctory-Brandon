# Reuse texture readback buffers across retired list recordings

## Location and execution path

`crates/victory-gpu/src/draw.rs:4458–4465` marks texture UAV writes. `crates/victory-gpu/src/host.rs:6874–6899` selects each eligible owner once per recording and calls `crates/victory-gpu/src/texback.rs:149–195` at close. `host.rs:9101–9129` creates a committed D3D12 readback resource. `texback.rs:201–244` maps it at landing.

## Current cost

Every qualifying recording allocates footprint and row vectors, asks the device for the layout, and creates a fresh committed resource, even for an identical texture shape and pitched size. Repeated draws inside a single list are already deduplicated. The default filter admits four shape/format combinations (`texback.rs:30–45`).

## Proposed redesign

Share immutable footprints, row counts and padded size in a bounded, per-device cache keyed by the complete host description. Have each recorded `Back` own one exclusive buffer lease until its deferred recording is reset or dropped after all flights and landings. Landing alone cannot release it because the list may replay. Reuse idle buffers of the required size, cap retained idle bytes, and allocate on a miss without waiting for a busy lease. Perform driver allocation and release outside the pool lock. Keep tickets and title layouts per target.

## Before and after

For N recordings of repeated descriptions, layout queries approach the number of distinct descriptions. Committed allocations approach peak simultaneously retained buffers per size class, subject to eviction. GPU copy volume and CPU retiling stay the same. Reuse trades idle resident bytes for fewer driver allocations.

## Expected impact and confidence

High for titles repeatedly rewriting eligible textures across recordings. The committed allocation is certain on each successful path. Measure recording frequency, driver allocation time, replay lifetime, peak pooled bytes and list-close stalls.
