# Build relay readback membership from wanted pieces only

## Location and execution path

When completed flights with card-resident write ranges land, `Host::relay_card_back` in `crates/victory-gpu/src/host.rs:4844–4915` reads GPU comparison results and constructs a fetch list. It materializes a (shadow, piece, wanted) triple for every non-picked range piece. `flagged_pieces` (2720–2727) copies true entries into another vector, sorts and deduplicates. A second range traversal binary-searches that result once per piece.

## Current cost

A plan declaring P pieces pays for P triples even if only one piece changed. Overlapping ranges can legitimately cover different bytes of one 64 KiB piece. A positive flag in any overlapping range applies to them all, so simply fetching at the first positive flag would lose data.

## Proposed redesign

Accumulate a local 64-bit mask for each aligned group of pieces visited within a range. OR only nonzero masks into a per-plan map keyed by (shadow identity, group), so dense runs require one table update per word and widely separated ranges allocate no intervening words. Keep the existing rules for unexamined, missing and suspect pieces. After all ranges have contributed, revisit each non-picked range in order, intersect its bounds with each stored mask and enumerate set bits in ascending order. This preserves overlapping partial ranges, picked-view discovery, per-range counters, exact-byte deduplication across plans and the check for bytes already fetched.

## Before and after

For P candidate occurrences and F unique wanted pieces, the sparse representation needs O(F) membership space instead of O(P + F). Expected lookup and insertion make the two traversals O(P), replacing sorting and per-occurrence binary search. Hash storage can cost more for tiny or dense plans; select the representation according to observed density.

## Expected impact and confidence

Medium to high when large declarations produce sparse changes on the relay. It saves CPU in the gathering phase, with identical GPU transfers. Measure candidate counts, wanted density, gather time and relay completion latency. Overlapping partial ranges with a positive flag in only the later range are a decisive correctness case.
