# Stream BCPack lane records when the destination can be discarded on error

## Problem

`bcpack::decode` in `crates/victory-formats/src/bcpack.rs:258–289` allocates records for every output block, calls `lanes` (418–469), then feeds the completed records sequentially to the BC reconstructor. DirectStorage calls the decoder through `execute` in `crates/victory-hle/src/modules/dstorage.rs:1408–1426`. For a fragmented physical destination, `into_pages` (1645–1661) first decodes into private scratch and only scatters it on success.

BC1/4 use 12 record bytes per 8-byte output block. BC3/5 use 24 per 16-byte block, and BC6/7 use 32 per 16. Workers therefore allocate an additional 1.5–2 times the decoded payload, write the records, then reread them in order. The lane scheduler advances only when every lane has delivered that symbol position. The block decoder's history is bounded independently of the full record array.

In Ghidra, `game_scarlett_master.exe` (SHA-256 prefix `e95020b2fa4a721d`), the archive builder at RVA `0x4ffd520` encodes modes 1–7 as BCPack. The destination helper at `0x4ffd8c0` selects a page-list pointer for eligible addresses, otherwise the builder passes a virtual address. A page-list request also carries the low 16 bits of the destination. This selects the destination interface only. A single-run page list can still decode directly into guest-visible memory through `page_window`. The executable cannot tell us how often the host ends up allocating private scratch.

## Change

Have `into_destination` and `into_pages` tell the fill callback whether it received guest-visible memory or private scratch. Decide after attempting the physical window, on each fill, since even a contiguous mapping can fail and fall back to scratch. For private output, let the lane scheduler feed completed record batches directly to reconstruction, preserving lane consumption order, stalls, format-specific block numbering and decoder history. Share the lane logic with the guest-visible path, which still validates all lanes before reconstruction starts. Preserve error precedence if reconstruction fails before a later lane error. Scatter only the successfully filled prefix under the existing retry and notification rules.

## Expected result

For B output blocks of record stride R, private-path record storage falls from B × R to a fixed batch × R, in addition to the existing decoded destination and history. This removes the full-size intermediate allocation and long-distance write/read pass. The decoder still produces and consumes each symbol. Direct contiguous guest destinations see no benefit under this conservative rollout.
