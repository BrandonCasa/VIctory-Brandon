# Reuse tiled column runs across array slices and repeated uploads

## Location and execution path

`Staging::detile` in `crates/victory-gpu/src/texture.rs:1489–1540` walks D3D footprints and calls the format converter once per selected (mip, array slice). `Staging::part` supplies that indexing at 1201–1205. Texture readback takes the reverse path in `crates/victory-gpu/src/texback.rs:227–240`. Both reach `Layout::walk` in `crates/victory-formats/src/texture.rs:660–763`.

## Current cost

On each non-linear invocation, the converter walks every x coordinate, discovers contiguous runs, grows a vector and checks whether the runs allow fixed-size copies. The resulting column offsets depend on the mip, layout and element size. Slice, row address, source/destination pitch and transfer direction do not enter that calculation (`column`, 589–595). Every array slice repeats it. The loop already shares the result across the rows and depth of one invocation.

## Proposed redesign

For a non-linear array conversion, build each selected mip's runs and common chunk size on its first slice and reuse them across subsequent slices in footprint order. Keep plans scoped to the batch and borrowed from its exact immutable layout. Retain plans across refreshes only for staging objects that actually repeat the conversion, with a byte limit on retained runs. Single-use, single-slice conversions and linear layouts need no cache. Keep per-call pitch and extent checks, per-row alignment, the last partial run and mip-tail offsets in the shared walker.

## Before and after

For S array slices and mip widths Wₘ, setup falls from O(S × ΣWₘ) to O(ΣWₘ) per prepared layout, amortizable across staging refreshes. Retained storage is proportional to the runs in prepared mips. Bytes copied are identical. A one-shot, single-slice conversion offers no setup saving, and a 3D subresource already shares setup across its depth.

## Expected impact and confidence

Medium for repeated array or cubemap conversions, below full texture passes. High confidence in the slice independence of the run calculation. Measure setup separately from memory-copy time by shape, and compare output byte-for-byte at odd pitches and mip tails.
