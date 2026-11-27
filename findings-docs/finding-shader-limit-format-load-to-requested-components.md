# Size explicit-format raw loads to the channels the instruction returns

## Location and execution path

`crates/victory-shader/src/emit/lower.rs:12236,12656–12672` routes an explicit-format buffer load through `format_load`, then optionally packs d16 results. At 7469–7505 and 7602–7610 the lowerer builds a raw load mask from the full format width, converts all components, pads missing channels and truncates to the requested count.

## Current cost

A one-channel instruction on a four-word format emits a four-word raw load mask. Building the unused field extractions and conversions also costs translator work. Downstream dead-code elimination can already remove computations unreachable from the result.

## Proposed redesign

Decode and convert only the first `min(channels, bits.len())` fields, then apply the existing zero/one defaults and d16 packing. Choose the byte/half alignment path from the full element width, even when only an eight- or sixteen-bit prefix is wanted. For a dword-aligned element, the required mask covers the prefix bit count rounded up to whole dwords. Narrow it only where the raw view returns the same requested values at the resource boundary. Else retain the full mask and extract only the needed words. Make this decision while lowering so it adds no per-lane bounds test.

## Before and after

For F format components and C requested, conversion work falls from F to min(F,C). An RGBA32 one-channel load can request one dword instead of four. Packed formats occupying one word still need that word. The mask change does not imply a proportional reduction in DRAM transactions.

## Expected impact and confidence

Medium. The initial mask and extra translation work are certain for qualifying formats. Driver scalarization, cache-line width and full-channel instructions can eliminate the runtime advantage. Measure eligible instructions and compare final ISA, translation time and GPU time.
