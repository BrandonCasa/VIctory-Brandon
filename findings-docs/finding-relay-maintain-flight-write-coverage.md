# Maintain the relay's write coverage as flights enter and leave

## Location and execution path

`crates/victory-gpu/src/host.rs:2910–2932,5195–5226,5424–5439,5478–5493` connects `send_late` and `execute_own` to `join_ranges`, prefix landing in `relay_land` and lookup in `touches`. All share the relay line's lock.

## Current cost

Adding a flight sorts and recopies the existing union even if the flight contributes zero ranges. After a prefix lands, the relay walks every remaining item, recounts lists, collects shader and texture write ranges, and sorts and compacts again. This extends a critical section shared with submitters.

## Proposed redesign

First bypass empty additions. Maintain list totals and reference-counted interval coverage from each flight's contributed ranges. Remove only the completed contributions when current landing visibility ends, after readbacks, writes and signals. Serialize updates with new enqueues and retain the existing `settling`, `in_hand` and `landing` protection. An address must remain hazardous until its last contributor leaves.

## Before and after

Today a prefix removal visits all R surviving declarations and can sort them in O(R log R), with O(R) temporary space. An ordered coverage index can update affected endpoints and crossed segments only. It uses extra persistent storage and highly fragmented or overlapping ranges can still require substantial work. Empty additions become constant cost.

## Expected impact and confidence

High for a deep relay with recurring write declarations. The rebuild and lock ownership are explicit. Shallow queues reduce the payoff. Measure lock hold/wait time, flight depth, surviving vs. removed ranges and segment fragmentation.
