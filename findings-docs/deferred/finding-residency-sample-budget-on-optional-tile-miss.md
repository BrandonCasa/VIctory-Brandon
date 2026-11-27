# Query the GPU budget only when it can change a tile grant

## Problem

`crates/victory-gpu/src/texture.rs:843–876` calls `OnDemand::uploaded` after claiming an upload. `crates/victory-gpu/src/resource.rs:2700–2716` eagerly calls `Host::video_memory`; `crates/victory-gpu/src/host.rs:10039–10076` queries both DXGI segments. `crates/victory-gpu/src/tiles.rs:366–383,473–485,510–538` determines which missing levels actually need tiles.

The budget argument is evaluated even when every named level is backed or the only missing levels were card-written and must receive tiles either way. A fixed texture budget also ignores the DXGI sample (`resource.rs:1774–1789`). The query sits on the submitting thread for claimed uploads in levels-on-demand mode.

## Change

Use the existing filled-byte accounting directly for a fixed limit. In automatic mode inspect the missing standard levels and shared packed tail under `given`. A missing tail needed by a card-written level is mandatory as a group. If no grant depends on budget, finish the held/shown update and mandatory grants under the same lock. Only for an optional miss, unlock to evaluate `room(video_memory())`, including its pool accounting. Reacquire `given` and recompute grants from current state before publishing this upload’s held mask and applying the sampled decision. Preserve retries and independent telemetry. The all-backed path takes the tile lock once.

## Expected result

For T claimed tiled uploads and M decisions that need a DXGI sample, ordinary driver calls can fall from 2T to 2M. Mask work stays bounded by mip count. Level-on-demand is opt-in; ordinary host textures never take this path.
