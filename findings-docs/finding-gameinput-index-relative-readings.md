# Index relative input readings by their existing sequence numbers

## Location and execution path

In `crates/victory-hle/src/modules/gameinput.rs:592–621,660–715`, the input hub polls every 8 ms plus processing time. Each changed value increments its slot's sequence exactly once and appends one reading to a deque capped at 256 entries. `next_reading` and `previous_reading` delegate to `relative` (845–908) and return an owned reference through `hand` (784–794).

## Current cost

Forward lookup starts at the oldest reading and skips every entry up to the reference, even to report that nothing follows the newest reading. Walking a backlog one reading at a time repeatedly scans the same prefix while holding the hub's state mutex. Each device history has a single input kind (389–395, 442–447, 677–684).

## Proposed redesign

After validating the reference device and the oldest retained sequence, calculate the requested neighbor's position relative to the oldest sequence using checked arithmetic. Index the deque directly, check the kind, then return through `hand`. Preserve the old-reference error and its precedence over a kind mismatch, boundary behavior, null output handling and COM ownership. Timestamp lookups use different semantics.

## Before and after

For H retained readings and K successive forward calls, worst-case work falls from O(KH) to O(K); storage is unchanged. H is bounded at 256. A previous lookup immediately behind the newest reading already examines only one or two elements.

## Expected impact and confidence

Medium to lower overall, with a precise benefit for titles repeatedly draining or polling a full input history. High confidence in dense per-slot numbering. Measure entries examined and time holding the hub mutex for actual title lookups.
