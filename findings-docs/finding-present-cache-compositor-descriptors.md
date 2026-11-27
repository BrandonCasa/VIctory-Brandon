# Retain the title's plane descriptions across queued presents

## Problem

The title builds a separate source rectangle and color-space word for each 72-byte retail plane, then passes stack-backed descriptions to the queue (EXE RVAs `0x24822a7–0x24824c1`). `queue::present` retains only the resources and the compositor samples whole textures with identical UVs. Reusing a texture can therefore display the previous or wrong crop.

## Change

At the known retail caller ABI, copy each plane's rectangle, destination and color space before returning to the title. Carry them with the owned resource through `queue::behind::Job::Present` and synchronous present. Use the short caller's own layout at its entry point. Derive UVs from each plane's texture and crop, clamp sampling to the requested texel centers, and apply its placement and supported color treatment.

## Expected result

Presenting the same resources with different crops displays the pixels requested by each call, even if the queue consumes both after the caller reuses its stack. Different-sized planes align in their requested destination.

[Deferred optimization](deferred/finding-present-cache-compositor-descriptors.md)
