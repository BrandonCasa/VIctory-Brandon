# Read a stale arena page once before merging its pieces

## Location and execution path

`crates/victory-gpu/src/memory.rs:82–86,499–542,633–655,980–984` defines 64 KiB pages and the 4 KiB comparisons in `Memory::bring` and `Memory::take`. Shader address preparation can call `bring`; deferred builder landing can call `take`. `crates/victory-gpu/src/host.rs:8846–8916` implements the common mapped-copy fast path and hole handling.

## Current cost

After freshness checks select a page, a full update calls `copy_mapped` sixteen times. Pieces are needed for comparison, watch notification or merge semantics, but the function also repeats the successful read setup for every piece under the shared memory lock. A call is not necessarily a kernel transition.

## Proposed redesign

Give the locked memory worker reusable page-sized scratch. Read the selected title span once via `copy_mapped`, then retain the existing per-piece merge, notification and whole-resend logic over the scratch. Use the helper's existing zero-fill and sparse fallback for inaccessible source bytes. Check behavior on remaps and keep per-frame/watch gating.

## Before and after

For P full nonfaulting stale pages the top-level read calls fall from 16P to P. Bytes read and comparison work stay O(P × 64 KiB). Partial spans can still use one bulk read. Scratch grows from 4 KiB to at most 64 KiB per active owner. Faulting ranges can incur additional fallback work.

## Expected impact and confidence

Medium on raw-address workloads with many stale pages. The multiplier is exact on full pages, while clean pages already skip it. Measure successful read calls, sparse fallbacks, stale-page bytes and memory-lock duration.
