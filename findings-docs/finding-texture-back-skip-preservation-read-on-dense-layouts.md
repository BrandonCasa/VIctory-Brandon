# Skip the old texture payload read when retiling covers every byte

## Location and execution path

`crates/victory-gpu/src/texback.rs:30–45,201–244` selects eligible texture shapes and lands GPU readbacks into title memory. `land` first reads the whole title allocation, then tiles every returned footprint into that temporary and publishes it. `crates/victory-formats/src/texture.rs:316–330,735–743,824–841,952–956` establishes a concrete dense case.

## Current cost

One eligible shape is a 256 × 256 format-41, single-mip, single-slice linear texture. Each row writes 1,024 bytes and all 256 rows fit its 262,144-byte layout exactly. Every byte read to preserve padding is overwritten before publication. Other eligible layouts can contain real padding.

## Proposed redesign

Cache a conservative full-coverage predicate for the actual layout and recorded footprint set. Initially restrict it to validated, tightly packed linear rows with an exact ticket extent. Map the readback, tile into owned scratch and validate before publishing. Preserve the current skip-on-unreadable-page gate with reliable whole-range readability under the same mapping/ticket guards. Fall back to the existing full read for uncertain protection, partial coverage, metadata mismatch or padding. `host.rs:8840–8862,8944–8946` shows why attempting the final write alone cannot substitute for the read gate.

## Before and after

For dense coverage of B bytes the old-value payload read falls from B bytes to a region/page readability check. The tiling and final B-byte publication still occur. A sparse layout keeps the old read; very fragmented mappings can make the check more expensive than reading sequentially. No GPU bandwidth saving is assumed.

## Expected impact and confidence

Medium on the landing thread for repeatedly copied-back dense textures. The redundant bytes are demonstrable for an allowed shape; title-specific usage is unknown. Measure dense matches, pre-read and validation time, and verify identical failure behavior with one inaccessible page.
