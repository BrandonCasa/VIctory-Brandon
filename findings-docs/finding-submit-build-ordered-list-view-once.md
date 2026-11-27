# Build the submission's ordered list view once

## Location and execution path

A nonempty late submission reaches `Host::execute_parts` in `crates/victory-gpu/src/host.rs:5382–5415`. It allocates and fills an ordered vector of `&HostList` solely to call `card_steps::cut_for`, drops it, then `send_late` builds the same vector for `submit`. In `host/card_steps.rs:77–102,463–471` the split check normally returns before inspecting even the first list. Card steps and placed-order splitting are opt-in. The release relay ordinarily uses the late path (`shadow.rs:938–945`, `host.rs:2549–2564`).

## Current cost

An N-list submission allocates two N-entry pointer arrays and visits the order indices twice on the no-cut path. This is fixed overhead for even short submissions. Splits require fresh views for genuinely different parts.

## Proposed redesign

In a scoped borrow, build the ordered references once, make the split decision and submit immediately when there is no cut. Move ownership into the flight or recursive split only after that borrow ends. Alternatively guard the split-only gather before allocation when the mode is disabled. Preserve duplicate indices, logical order, part offsets and the existing no-cut fallback when an attempted split cannot transfer ownership.

## Before and after

On the normal path, two allocations and 2N index-to-pointer resolutions become one allocation and N resolutions. Both versions use O(N) peak storage; the old arrays have disjoint lifetimes. GPU work and fence behavior are identical.

## Expected impact and confidence

Lower than the texture and shader-write passes, but recurs on the default submission path and removes one whole allocation/traversal deterministically. Measure submissions per frame, list-count distribution and CPU cost on short-list-heavy scenes.
