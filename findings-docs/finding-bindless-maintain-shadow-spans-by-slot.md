# Maintain bindless buffer spans as descriptor slots change

## Location and execution path

`State::heap_buffers` in `crates/victory-gpu/src/draw.rs:2031–2087` starts a buffer sweep on the eligible bindless path, normally once per frame. `BufferSweep` retains each slot's shadow and range (599–648). After the initial pass, `heap_buffer_pass` updates changed slots and normally rechecks one of 16 partitions (859–882). It nevertheless traverses the *entire* retained slot vector and rebuilds a per-shadow hash map of minimum and maximum addresses (940–991).

## Current cost

The dense vector extends to the highest retained slot index, including holes. A heap with many aliases of the same buffers rehashes those unchanged contributions every pass to produce the same small list of refresh spans. This work occupies the per-heap sweep lock. Off-thread sweeps can shift where the cost is paid, but submission still needs the results.

## Proposed redesign

Update span contributions when a slot changes. A simple bounded option is to maintain a summary per periodic partition and rebuild only partitions touched by writes or revalidation, then merge their per-shadow summaries. For heaps with scattered changes, maintain counted per-shadow endpoints instead. Remove an old contribution when its slot becomes empty or points elsewhere, so extrema can shrink. Preserve `Arc` identity/lifetime, first-pass rebuilding, periodic stale-view detection, optional unordered-view reporting, and unwatched-frame policy. If refresh coverage is later tracked as disjoint intervals, maintain their union per shadow and let the output cost scale with its interval count. Emit a refresh for every active shadow that currently qualifies, even if its view span is unchanged.

## Before and after

Let H be the highest retained slot plus one, V the live views and S their distinct shadows. Current aggregation adds O(H + V) work every pass before O(S) output. With counted endpoints, C changed or revalidated contributions cost O(C log V) and enumeration costs O(S), at the price of O(V) additional metadata. Partition summaries offer a different tradeoff when C is large. Neither scheme claims to eliminate required shadow refresh bytes.

## Expected impact and confidence

High for large, slowly changing bindless heaps, especially with sparse slot numbers or heavy aliasing. High confidence in the redundant traversal; measure H, V, S, touched partitions, sweep time and the wait for an off-thread sweep. Validate emitted spans over slot clears, endpoint shrinkage and shadow replacement.
