# Reuse unchanged compositor descriptors on fence-safe presentation slots

## Location and execution path

`crates/victory-gpu/src/host.rs:9754–9785` chooses a completed presenter list for a two-plane present. `crates/victory-gpu/src/compose.rs:130–183` reuses its shader-visible heap but `Composer::record` creates both SRVs each time and calls `Host::rtv` (`host.rs:6430–6444`) for the back buffer.

## Current cost

Each composed frame performs three driver descriptor creations. The two source descriptions always have the same layout apart from resource and format. Swap-chain buffers rotate in a finite set. A stable source bound again to the same completed slot receives an identical descriptor.

## Proposed redesign

Carry each source’s `HostTexture::serial` and view format with the resource from both present paths. Compare these two scalar keys in the selected presenter slot after its fence completes (slot zero for synchronous presents). Recreate only changed SRVs, preserving the existing command-list lifetime holds. This avoids both pointer-reuse errors and pinning large textures in idle cache slots. On first composition allocate one private CPU RTV heap indexed by swap-chain buffer. Populate each RTV on first use and invalidate all entries on every successful resize, including size-only changes. Keep the existing RTV ring available if the private heap cannot be allocated.

## Before and after

For F composed frames, two SRV creations per frame become one per changed plane/slot binding; RTV creations approach the number of buffers per swap-chain generation. Two small keys per presenter slot and private RTV storage are required. Alternating sources on all slots can still require 2F SRVs.

## Expected impact and confidence

Medium to lower, limited to actual two-plane composition. The per-frame driver calls are explicit; the gain is CPU recording time. Count stable slot bindings, descriptor creates and present-list CPU time across resize and asynchronous presentation.
