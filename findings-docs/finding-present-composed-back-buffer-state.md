# Transition a composed back buffer directly to its next consumer

## Location and execution path

`crates/victory-gpu/src/queue.rs:905–1111` routes a present with compatible base and overlay textures to `Host::present_planes`. In `crates/victory-gpu/src/host.rs:9769–9824` the composer draws directly into the swap-chain back buffer, then transitions it from render target to copy destination. Capture, HUD drawing and presentation follow (9843–9898). Two-plane composition is enabled by default for a distinct supported second plane (`compose.rs:38–43`).

## Current cost

On a normal composed frame with no capture and no HUD draw, the back buffer goes from render target to copy destination to present. No command uses it as a copy destination between those transitions. A composed frame captured without HUD additionally transitions through copy destination on both sides of the readback.

## Proposed redesign

Track the back buffer's current state through this short sequence. After composition, restore the two source textures as today. Transition the output to copy source only for actual capture, then to copy destination only when the HUD is going to copy into it. Have `Panel::draw` request that transition just before its back-buffer copy, after the early returns in `host/panel.rs:867–889`. End in present state. Preserve capture-before-HUD ordering, fallback to the base plane on composition failure and the single-plane copy-destination state.

## Before and after

On the composed frame with neither capture nor HUD, two output transitions become one; capture alone requires render target → copy source → present. A frame actually receiving a HUD copy still needs copy destination. Source restoration and the fullscreen composite draw cost the same.

## Expected impact and confidence

Medium to lower relative impact. This removes a redundant GPU resource-state transition at each qualifying frame boundary. The actual saving depends on driver barrier handling and the frequency of two-plane output. Inspect the recorded transitions with HUD and capture independently enabled, then measure present-list GPU time.
