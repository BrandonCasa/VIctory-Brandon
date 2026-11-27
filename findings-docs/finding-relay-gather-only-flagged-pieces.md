# Build relay readback membership from wanted pieces only

## Location and execution path

When completed flights with card-resident write ranges land, `Host::relay_card_back` in `crates/victory-gpu/src/host.rs:4844–4915` reads GPU comparison results and constructs a fetch list. It materializes a (shadow, piece, wanted) triple for every non-picked range piece. `flagged_pieces` (2720–2727) copies true entries into another vector, sorts and deduplicates. A second range traversal binary-searches that result once per piece.

## Current cost

A plan declaring P pieces pays for P triples even if only one piece changed. Overlapping ranges can legitimately cover different bytes of one 64 KiB piece. A positive flag in any overlapping range applies to them all, so simply fetching at the first positive flag would lose data.

## Proposed redesign

In the first phase insert only wanted (shadow, piece) pairs directly into per-plan membership. A sparse set suits low flag density; grouped bitsets suit dense ranges. Then run the existing range-order pass against that membership. Preserve the forced fetch for unexamined/missing results, suspect-piece policy, picked-view discovery, cross-plan deduplication of exact byte intervals, and the check for bytes already fetched. The iteration order for output continues to come from the original ranges.

## Before and after

For P candidate occurrences and F unique wanted pieces, the sparse representation needs O(F) membership space instead of O(P + F). Expected lookup and insertion make the two traversals O(P), replacing sorting and per-occurrence binary search. Hash storage can cost more for tiny or dense plans; select the representation according to observed density.

## Expected impact and confidence

Medium to high when large declarations produce sparse changes on the relay. It saves CPU in the gathering phase, with identical GPU transfers. Measure candidate counts, wanted density, gather time and relay completion latency. Overlapping partial ranges with a positive flag in only the later range are a decisive correctness case.
