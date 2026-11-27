# Reuse unchanged compositor descriptors on fence-safe presentation slots

## Problem

`crates/victory-gpu/src/host.rs:9754–9785` chooses a completed presenter list for a two-plane present. `crates/victory-gpu/src/compose.rs:130–183` reuses its shader-visible heap but `Composer::record` creates both SRVs each time and calls `Host::rtv` (`host.rs:6430–6444`) for the back buffer.

Each composed frame performs three driver descriptor creations. The two source descriptions always have the same layout apart from resource and format. Swap-chain buffers rotate in a finite set. A stable source bound again to the same completed slot receives an identical descriptor.

The November 27 Series X log recorded no successful two-plane composition.

## Change

Carry HostTexture::serial and the view format through both present paths. In a fence-safe presenter slot, recreate only SRVs whose scalar keys changed. Keep draw parameters for crop and color separate from descriptor reuse. Use a private CPU RTV heap indexed by swap-chain buffer, lazily populate it and invalidate it on every successful resize. Retain the existing RTV ring on allocation failure.

## Expected result

For F composed frames, two SRV creations per frame become one per changed plane/slot binding; RTV creations approach the number of buffers per swap-chain generation. Two small keys per presenter slot and private RTV storage are required. Alternating sources on all slots can still require 2F SRVs.

[Related correctness finding](../finding-present-cache-compositor-descriptors.md)
