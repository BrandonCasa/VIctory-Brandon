# Maintain the relay's write coverage as flights enter and leave

## Location and execution path

`crates/victory-gpu/src/host.rs:2910–2932,5195–5226,5424–5439,5478–5493` connects `send_late` and `execute_own` to `join_ranges`, prefix landing in `relay_land` and lookup in `touches`. All share the relay line's lock.

## Current cost

Adding a flight sorts and recopies the existing union even if the flight contributes zero ranges. After a prefix lands, the relay walks every remaining item, recounts lists, collects shader and texture write ranges, and sorts and compacts again. This extends a critical section shared with submitters.

## Proposed redesign

Keep the sorted union that `touches`, `late_blocked` and submission snapshots already consume. Skip empty additions and ranges wholly covered by an existing interval before sorting or copying the union. Count each flight's `order.len().max(1)` and whether it contributes any nonempty shader or texture range. After readbacks, writes and signals finish, subtract the landed counts under the line lock. Keep the union if no writer landed, clear it if no writer survives, and rebuild from surviving flights only for a mixed removal. This includes enqueues during landing and preserves `settling`, `in_hand`, `landing` and the non-relay full drain. Consider a counted interval index only if those mixed rebuilds dominate after accounting for fragmented overlaps and the cost of supplying sorted snapshots to readers.

## Before and after

Today a prefix removal visits all R surviving declarations and can sort them in O(R log R), with O(R) temporary space. An ordered coverage index can update affected endpoints and crossed segments only. It uses extra persistent storage and highly fragmented or overlapping ranges can still require substantial work. Empty additions become constant cost.

## Expected impact and confidence

High for a deep relay with recurring write declarations. The rebuild and lock ownership are explicit. Shallow queues reduce the payoff. Measure lock hold/wait time, flight depth, surviving vs. removed ranges and segment fragmentation.
