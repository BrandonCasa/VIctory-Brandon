# Retain the title's plane descriptions across queued presents

## Problem

The retail PresentX caller writes one integer source rectangle `(0, 0, width, height)` per plane and points every plane at the same normalized float destination `(0, 0, 1, 1)` (EXE RVAs `0x2481eb2`, `0x248227e`, `0x24822a7–0x2482356`, constant at `0x6159cd0`). Each 72-byte plane names one resource and carries its own color-space word at `+0x28`. Both queue paths drop a second plane that names the same resource as the first. The compositor samples whole textures even when the supplied source extent covers only part of one.

## Change

Decode the 72-byte records at the retail entry (`+0x100`) and the short prefix at the older entry (`+0xf0`). Copy the pointed-to rectangles and retain the resources before returning. Carry each plane’s own values through both `queue::behind::Job::Present` and synchronous present. Convert source texels and normalized destination coordinates separately, clamp sampling to the requested texel centers, and apply the supported color treatment per plane. When both planes sample the same host texture, preserve their blend order and individual crops while transitioning that resource only once.

## Expected result

Presenting the same resources with different crops displays the pixels requested by each call, even if the queue consumes both after the caller reuses its stack. Different-sized planes align in their requested destination.

[Deferred optimization](deferred/present-reuse-plane-descriptors-on-retired-slots.md)
