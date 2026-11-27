# Reuse unchanged compositor descriptors on fence-safe presentation slots

## Location and execution path

`crates/victory-gpu/src/host.rs:9754–9785` chooses a completed presenter list for a two-plane present. `crates/victory-gpu/src/compose.rs:130–183` reuses its shader-visible heap but `Composer::record` creates both SRVs each time and calls `Host::rtv` (`host.rs:6430–6444`) for the back buffer.

## Current cost

Each composed frame performs three driver descriptor creations. The two source descriptions always have the same layout apart from resource and format. Swap-chain buffers rotate in a finite set. A stable source bound again to the same completed slot receives an identical descriptor.

## Proposed redesign

Keep the last resource identity and format for each of the slot's two SRVs, updating only changed entries after fence-safe reuse. Use lifetime-safe identity to handle resource destruction and pointer reuse. Allocate persistent RTV slots per swap-chain buffer, invalidating them and releasing resource references before resize/recreation. Preserve the transient RTV ring for other callers.

## Before and after

For F composed frames, two SRV creations per frame become one per changed plane/slot binding; RTV creations approach the number of buffers per swap-chain generation. Two small keys per presenter slot and private RTV storage are required. Alternating sources on all slots can still require 2F SRVs.

## Expected impact and confidence

Medium to lower, limited to actual two-plane composition. The per-frame driver calls are explicit; the gain is CPU recording time. Count stable slot bindings, descriptor creates and present-list CPU time across resize and asynchronous presentation.
