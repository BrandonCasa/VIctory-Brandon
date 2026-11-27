# Transition a composed back buffer directly to its next consumer

## Problem

`crates/victory-gpu/src/queue.rs:905–1111` routes a present with compatible base and overlay textures to `Host::present_planes`. In `crates/victory-gpu/src/host.rs:9769–9824` the composer draws directly into the swap-chain back buffer, then transitions it from render target to copy destination. Capture, HUD drawing and presentation follow (9843–9898). Two-plane composition is enabled by default for a distinct supported second plane (`compose.rs:38–43`).

On a normal composed frame with no capture and no HUD draw, the back buffer goes from render target to copy destination to present. No command uses it as a copy destination between those transitions. A composed frame captured without HUD additionally transitions through copy destination on both sides of the readback.

The November 27 log reports no successful two-plane composition and gives no crop dimensions.

## Change

For an already correct rendered present, leave the back buffer in render-target state until its next actual consumer. Transition to copy source for capture and to copy destination only when the HUD performs its copy. Batch the source restorations with the final transition to present. Track the actual state on early exits, including a failed capture allocation.

## Expected result

On the composed frame with neither capture nor HUD, two output transitions become one; capture alone requires render target → copy source → present. A frame actually receiving a HUD copy still needs copy destination. Source restoration and the fullscreen composite draw cost the same.

[Related correctness finding](../present-render-and-capture-transformed-single-plane-output.md)
