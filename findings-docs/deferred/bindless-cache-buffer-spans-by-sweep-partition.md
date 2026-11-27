# Maintain bindless buffer spans as descriptor slots change

## Problem

`State::heap_buffers` in `crates/victory-gpu/src/draw.rs:2031–2087` starts a buffer sweep on the eligible bindless path, normally once per frame. `BufferSweep` retains each slot's shadow and range (599–648). After the initial pass, `heap_buffer_pass` updates changed slots and normally rechecks one of 16 partitions (859–882). It nevertheless traverses the *entire* retained slot vector and rebuilds a per-shadow hash map of minimum and maximum addresses (940–991).

The dense vector extends to the highest retained slot index, including holes. A heap with many aliases of the same buffers rehashes those unchanged contributions every pass to produce the same small list of refresh spans. This work occupies the per-heap sweep lock. Off-thread sweeps can shift where the cost is paid, but submission still needs the results.

## Change

For large alias-heavy heaps, cache one summary per existing sweep partition. Rebuild the scheduled partition from its resolved slots, including slots that disappeared. In the other partitions compare each write with its retained shadow and endpoints; mark a partition dirty only on an actual change and rebuild it once after all writes. Merge the 16 summaries into reusable output storage, retaining live `Arc`s and emitting every refresh due under the unwatched-frame policy. Keep the direct fold for small heaps. Preserve full-pass rebuilding and the existing UAV reports. If sparse refreshes are supported, retain their runs within each partition; taking extrema here would discard the holes before submission.

## Expected result

After the initial full fold, a stable large heap rebuilds the scheduled sweep partition and merges 16 retained summaries. Additional rebuilds are limited to partitions whose slot contributions changed. Clearing a slot, replacing its shadow or shrinking an endpoint produces the same refresh spans as a full rebuild.
