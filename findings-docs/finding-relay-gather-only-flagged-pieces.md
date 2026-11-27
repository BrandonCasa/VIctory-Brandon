# Compare each declared relay piece once per flight

## Location and execution path

When completed flights with card-resident write ranges land, `Host::relay_card_back` in `crates/victory-gpu/src/host.rs:4844–4915` reads GPU comparison results and constructs a fetch list. It materializes a (shadow, piece, wanted) triple for every non-picked range piece. `flagged_pieces` (2720–2727) copies true entries into another vector, sorts and deduplicates. A second range traversal binary-searches that result once per piece.

## Current cost

`card_before` merges byte ranges by shadow, but two disjoint ranges can still touch the same 64 KiB piece. Each gets a result word and a GPU hash of the whole piece. In a flown plan the ordinary compare also advances the baseline, so a later occurrence can report no change. Landing reconstructs membership across P occurrences so that a changed piece brings back every declared slice of that piece.

## Proposed redesign

In `card_before`, keep the exact byte spans for landing and derive comparison runs from their already sorted piece intervals. Reuse the overlap and adjacency rule in `shadow::joined_piece_runs`. For a flown plan whose ordinary pieces all have a baseline and fit under the existing admission check, dispatch each run once and point each span's `at` at its first shared result word. Keep the complete word for torn-read checks. Landing can consult that word and the suspect set while visiting each original slice. Keep picked comparisons separate.

Mixed plans can also drop the flag vector without changing their GPU dispatches. Ordinary spans are disjoint, in byte order and grouped by shadow, so all occurrences of the same piece are consecutive. Look ahead to OR their flags, treating an unchecked occurrence or missing word as wanted, then walk that group again to emit its original slices in order if wanted or suspect. Two cursors into the existing spans suffice even if many slices share one piece. Preserve per-range accounting, exact-byte clipping, cross-flight deduplication, already-fetched checks and the existing skip when the result mapping itself is absent.

## Before and after

For P ordinary piece occurrences covering U distinct pieces, a fully compared plan launches U ordinary hash groups and reserves U ordinary result words. Planning adds one linear pass over sorted spans and one descriptor per contiguous run. Landing takes O(P) time for flag membership and O(1) extra membership storage on either path. The fetch list and cross-flight deduplication still scale with the selected slices. With no shared pieces the GPU hashes the same bytes.

## Expected impact and confidence

The GPU saving depends on disjoint declarations sharing pieces. The CPU gather can shrink even when all pieces are distinct. Compare hash groups, result words and landing time. Check two disjoint slices of one piece, picked views, a full result buffer and two flights writing the same piece.
