# Maintain bindless buffer spans as descriptor slots change

## Location and execution path

`State::heap_buffers` in `crates/victory-gpu/src/draw.rs:2031–2087` starts a buffer sweep on the eligible bindless path, normally once per frame. `BufferSweep` retains each slot's shadow and range (599–648). After the initial pass, `heap_buffer_pass` updates changed slots and normally rechecks one of 16 partitions (859–882). It nevertheless traverses the *entire* retained slot vector and rebuilds a per-shadow hash map of minimum and maximum addresses (940–991).

## Current cost

The dense vector extends to the highest retained slot index, including holes. A heap with many aliases of the same buffers rehashes those unchanged contributions every pass to produce the same small list of refresh spans. This work occupies the per-heap sweep lock. Off-thread sweeps can shift where the cost is paid, but submission still needs the results.

## Proposed redesign

For large alias-heavy heaps, cache one summary per existing sweep partition. Rebuild the scheduled partition from its resolved slots, including slots that disappeared. In the other partitions compare each write with its retained shadow and endpoints; mark a partition dirty only on an actual change and rebuild it once after all writes. Merge the 16 summaries into reusable output storage, retaining live `Arc`s and emitting every refresh due under the unwatched-frame policy. Keep the direct fold for small heaps. Counted endpoints are worthwhile only if scattered changes routinely force most partitions to rebuild. Preserve full-pass rebuilding and the existing UAV reports. If sparse refreshes are supported, retain their runs within each partition; taking extrema here would discard the holes before submission.

## Before and after

Let H be the highest retained slot plus one, V the live views and S their distinct shadows. Current aggregation adds O(H + V) work every pass before O(S) output. With counted endpoints, C changed or revalidated contributions cost O(C log V) and enumeration costs O(S), at the price of O(V) additional metadata. Partition summaries offer a different tradeoff when C is large. Neither scheme claims to eliminate required shadow refresh bytes.

## Expected impact and confidence

High for large, slowly changing bindless heaps, especially with sparse slot numbers or heavy aliasing. High confidence in the redundant traversal; measure H, V, S, touched partitions, sweep time and the wait for an off-thread sweep. Validate emitted spans over slot clears, endpoint shrinkage and shadow replacement.
