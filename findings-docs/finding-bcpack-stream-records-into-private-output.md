# Stream BCPack lane records when the destination can be discarded on error

## Location and execution path

`bcpack::decode` in `crates/victory-formats/src/bcpack.rs:258–289` allocates records for every output block, calls `lanes` (418–469), then feeds the completed records sequentially to the BC reconstructor. DirectStorage calls the decoder through `execute` in `crates/victory-hle/src/modules/dstorage.rs:1408–1426`. For a fragmented physical destination, `into_pages` (1645–1661) first decodes into private scratch and only scatters it on success.

## Current cost

BC1/4 use 12 record bytes per 8-byte output block. BC3/5 use 24 per 16-byte block, and BC6/7 use 32 per 16. Workers therefore allocate an additional 1.5–2 times the decoded payload, write the records, then reread them in order. The lane scheduler advances only when every lane has delivered that symbol position. The block decoder's history is bounded independently of the full record array.

## Proposed redesign

Add a streaming decoder for private, discardable output. Complete one record or a small batch, reconstruct its blocks and retain lane state and block history across batches. Preserve compressed input consumption order, lane stalls, interleaving and format-specific block numbering. If reconstruction fails before the lane stream ends, continue validating lanes when necessary to preserve the currently reported error. Use the existing two-phase decoder for guest-visible output where early lane failure currently leaves the destination untouched.

## Before and after

For B output blocks of record stride R, private-path record storage falls from B × R to a fixed batch × R, in addition to the existing decoded destination and history. This removes the full-size intermediate allocation and long-distance write/read pass. The decoder still produces and consumes each symbol. Direct contiguous guest destinations see no benefit under this conservative rollout.

## Expected impact and confidence

High for peak memory and potentially streaming throughput on large fragmented BCPack jobs. The size ratio and discard-on-error boundary follow from source; the frequency of fragmented destinations is unknown. Measure request mix, peak concurrent scratch, decode bandwidth, and equivalence on valid and corrupt streams for every format.
