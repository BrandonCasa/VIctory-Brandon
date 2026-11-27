# Avoid recounting empty texture blocks and retrying failed ripe blocks

## Location and execution path

`crates/victory-gpu/src/pool.rs:134–155,224–258,654–673` connects the texture placement call under the `TEXTURES` lock to `Pool::take_ripe` and `Ranges::take`. The texture pool and ahead allocation are enabled by default (lines 89–106).

## Current cost

When all ripe blocks fail, the fallback sorts every live block by age and probes the same ripe free ranges again. A failed `Ranges::take` leaves them unchanged. Even on a first-pass hit, ahead allocation scans all blocks to count the empty reserve while holding the texture pool lock.

## Proposed redesign

Have `Pool` maintain its empty-block count alongside the reserve limit. Mark each new block empty. Clear that mark and decrement the count on its first successful allocation. Reuse `reclaim`'s existing `whole(BLOCK)` check to mark a block empty when its last occupied range completes the rest period, and decrement the count when a sweep drops it. Use the count after allocation to make the existing ahead-growth decision. Failed probes leave it alone. On a first-pass miss, collect only unripe blocks and keep their existing `(made, index)` fallback order. Preserve alignment, sweep order and growth cooldown.

## Before and after

With B live blocks and U unripe ones, the reserve check goes from O(B) to O(1) on each successful ahead allocation, for one pool counter and an empty flag per block. A fallback still classifies B blocks, but sorts only U and never repeats failed ripe free-list walks. An actual retirement sweep can still visit the pool.

## Expected impact and confidence

Medium during texture streaming into fragmented pools. The duplicated search is certain on misses; the number of ripe free ranges controls the payoff. Measure first-pass miss rate, scanned ranges and texture-pool lock hold time.
