# Skip the old texture payload read when retiling covers every byte

## Problem

`crates/victory-gpu/src/texback.rs:30–45,201–244` selects eligible texture shapes and lands GPU readbacks into title memory. `land` first reads the whole title allocation, then tiles every returned footprint into that temporary and publishes it. `crates/victory-formats/src/texture.rs:316–330,735–743,824–841,952–956` establishes a concrete dense case.

One eligible shape is a 256 × 256 format-41, single-mip, single-slice linear texture. Each row writes 1,024 bytes and all 256 rows fit its 262,144-byte layout exactly. Every byte read to preserve padding is overwritten before publication. Other eligible layouts can contain real padding.

## Change

Classify the copy when `texback::record` has both the ticket's guest layout and the device's actual footprints. Store a direct-copy offset and length in `Back` only for a single linear mip and slice whose format, row width, pitch, row count, depth and full ticket extent match. Replays can reuse this decision. At landing, hold the mapping and ticket guards and recheck ticket validity. Use `read_into` as the readability gate unless a live mapping guard already guarantees readability for the entire destination interval. On success, keep the readback buffer mapped through `copy_mapped`. Layouts with padding or mismatches still use preservation and retiling.

## Expected result

A matching B-byte layout avoids retiling B bytes on every landing, including replay. It can also avoid the B-byte preservation read if current readability is established more cheaply. Direct publication still copies B bytes to the title. Classification is paid once per recording and does not cache mapping permissions.
