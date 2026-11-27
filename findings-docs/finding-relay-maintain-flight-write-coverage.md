# Maintain the relay's write coverage as flights enter and leave

## Location and execution path

`crates/victory-gpu/src/host.rs:2910–2932,5195–5226,5424–5439,5478–5493` connects `send_late` and `execute_own` to `join_ranges`, prefix landing in `relay_land` and lookup in `touches`. All share the relay line's lock.

## Current cost

Adding a flight sorts and recopies the existing union even if the flight contributes zero ranges. After a prefix lands, the relay walks every remaining item, recounts lists, collects shader and texture write ranges, and sorts and compacts again. This extends a critical section shared with submitters.

## Proposed redesign

Assemble a flight's address coverage when `submit` constructs `Sent`, after `picked::with` and while collecting texture-back ranges. Keep the exact per-shadow `back` ranges for landing. Use the texture-range workspace for the combined sorted union, with cheap empty and single-range cases. Both enqueue paths can merge sorted flight coverage into `Line::written` under the lock, skipping empty or already-covered additions.

Count list entries as `send_late` does and count an `execute_own` flight as one even though its order is empty. Also count flights with nonempty coverage. Keep the landed prefix represented until its writes and signals finish. Under the lock, subtract only that prefix's contributions from the current totals. Flights enqueued during landing remain counted and covered. Keep the union if no writer landed, clear it if none survive, and rebuild from surviving summaries on a mixed removal. Reset the counters on non-relay whole-queue settlement. Keep queued writes, in-hand writes and landing writers on their existing paths.

## Before and after

An enqueue that needs a full merge takes O(U + H) for U existing union intervals and H flight intervals, after preparing the flight outside the line lock. Removing a prefix of read-only flights or landing the last writer requires no survivor scan. A mixed removal can still collect and sort all surviving summary intervals. A flight with many disjoint shader writes retains extra intervals alongside `back`. Compare that memory and submission-thread preparation against the lock time saved.

## Expected impact and confidence

High for a deep relay with recurring write declarations. The rebuild and lock ownership are explicit. Shallow queues reduce the payoff. Measure lock hold/wait time, flight depth, surviving vs. removed ranges and segment fragmentation.
