# Size explicit-format raw loads to the channels the instruction returns

## Location and execution path

`crates/victory-shader/src/emit/lower.rs:12236,12656–12672` routes an explicit-format buffer load through `format_load`, then optionally packs d16 results. At 7469–7505 and 7602–7610 the lowerer builds a raw load mask from the full format width, converts all components, pads missing channels and truncates to the requested count.

## Current cost

A one-channel instruction on a four-word format emits a four-word raw load mask. Building the unused field extractions and conversions also costs translator work. Downstream dead-code elimination can already remove computations unreachable from the result.

## Proposed redesign

Compute the bit extent of the requested component prefix. Request the covering dwords and generate conversions for only that prefix. Keep the original element addressing, sub-dword alignment, conversion rules, channel defaults and d16 packing. Verify resource-end and partially out-of-bounds behavior for each narrower mask before enabling it there.

## Before and after

For F format components and C requested, conversion work falls from F to min(F,C). An RGBA32 one-channel load can request one dword instead of four. Packed formats occupying one word still need that word. The mask change does not imply a proportional reduction in DRAM transactions.

## Expected impact and confidence

Medium. The initial mask and extra translation work are certain for qualifying formats. Driver scalarization, cache-line width and full-channel instructions can eliminate the runtime advantage. Measure eligible instructions and compare final ISA, translation time and GPU time.
