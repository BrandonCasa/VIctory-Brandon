# Exclude already-searched texture blocks from the allocation fallback

## Location and execution path

`crates/victory-gpu/src/pool.rs:134–155,224–258,654–673` connects the texture placement call under the `TEXTURES` lock to `Pool::take_ripe` and `Ranges::take`. The texture pool and ahead allocation are enabled by default (lines 89–106).

## Current cost

When every ripe block fails, the fallback collects and sorts every live block by age, then probes their free ranges. Ripe blocks have just failed the identical aligned request and a failed `Ranges::take` leaves them unchanged. Fragmentation makes these repeated scans longer.

## Proposed redesign

Only collect and sort unripe blocks for the fallback. Keep the existing `(made, index)` order among those, the first pass's ripe ordering, alignment, retirement, growth cooldown and ahead reserve. No new state is needed across requests.

## Before and after

For B live blocks and U unripe blocks, fallback sorting falls from O(B log B) to O(U log U), plus the existing O(B) classification. Failed ripe free-list walks are done once. Temporary sort storage falls from B to U entries. Success in the first pass is unchanged.

## Expected impact and confidence

Medium during texture streaming into fragmented pools. The duplicated search is certain on misses; the number of ripe free ranges controls the payoff. Measure first-pass miss rate, scanned ranges and texture-pool lock hold time.
