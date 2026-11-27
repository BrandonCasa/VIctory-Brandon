# Query the GPU budget only when it can change a tile grant

## Location and execution path

`crates/victory-gpu/src/texture.rs:843–876` calls `OnDemand::uploaded` after claiming an upload. `crates/victory-gpu/src/resource.rs:2700–2716` eagerly calls `Host::video_memory`; `crates/victory-gpu/src/host.rs:10039–10076` queries both DXGI segments. `crates/victory-gpu/src/tiles.rs:366–383,473–485,510–538` determines which missing levels actually need tiles.

## Current cost

The budget argument is evaluated even when every named level is backed or the only missing levels were card-written and must receive tiles either way. A fixed texture budget also ignores the DXGI sample (`resource.rs:1774–1789`). The query sits on the submitting thread for claimed uploads in levels-on-demand mode.

## Proposed redesign

Check missing optional grants including the shared packed tail before sampling. Use fixed-limit accounting directly where configured. Keep the latest residency state authoritative at grant time and avoid holding the tile lock across driver calls. Preserve held/shown changes, mandatory card-write grants, failure retries and independent budget telemetry. Skipped upload-triggered samples change the timing of budget-change diagnostics.

## Before and after

For T claimed tiled uploads and M decisions that need a DXGI sample, ordinary driver calls can fall from 2T to 2M. Mask work stays bounded by mip count. Level-on-demand is opt-in; ordinary host textures never take this path.

## Expected impact and confidence

Medium within this mode, lower overall. The eager sample is explicit. Measure optional misses, driver query time and submit spikes, and check telemetry cadence when samples become less frequent.
