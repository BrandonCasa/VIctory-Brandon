# Render and capture the single-plane output requested by the title

## Problem

The single-plane path copies and captures the entire source. The retail caller supplies an explicit source rectangle even for one plane (EXE RVAs `0x24822a7` and `0x24824c1`). A crop or color transform makes the source texture differ from the requested output.

## Change

Use `CopyResource` only for a compatible full-source, full-destination present with no transformation. Otherwise render using the [owned plane description](finding-present-cache-compositor-descriptors.md). In `Host::present_planes`, capture the resulting back buffer before the HUD, transition it for each consumer, restore source states, and finish in present state. On composition failure, discard the unsubmitted list and propagate the failure through the present path. The current recursive base-only retry would expose the wrong image for a transformed present.

## Expected result

With distinctive pixels outside the crop, the window and pre-HUD capture show only the requested image. The HUD appears only on the window. An eligible full-plane present still uses the copy path.

[Deferred optimization](deferred/finding-present-composed-back-buffer-state.md)
