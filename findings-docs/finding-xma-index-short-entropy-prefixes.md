# Decode common XMA entropy codes with a prefix lookup

## Location and execution path

`crates/victory-hle/src/modules/audio.rs:620–631` invokes the XMA decoder for an active voice. In `crates/victory-formats/src/xma.rs:238–291` shared `Vlc` tables use `partition_point` for each symbol. Coefficient/vector decoding (824–895) and scale-factor loops (924–949) call it repeatedly during transmitted subframes.

## Current cost

Codebooks are shared already. Searching their sorted left-aligned codes still costs a sequence of dependent comparisons for every decoded symbol. An escaped vector may require further vector and scalar codebook lookups.

## Proposed redesign

Build a fixed-width first-level lookup as each `Vlc` is initialized. A prefix whose completions all resolve to the same short code stores its symbol and exact consumed length; ambiguous prefixes retain the original full-width lookup. Preserve `peek32` zero padding, position advancement and eventual overread/error behavior on truncated input. Validate every code's prefix boundaries against the existing decoder.

## Before and after

For N symbols and a codebook of K entries, the old path performs O(N log K) comparisons. With H direct hits, H searches become indexed loads and the remainder take a fallback. A b-bit prefix costs 2^b entries per table. At b = 8, between 232 and 255 of the 256 possible prefixes resolve directly in each of the seven current codebooks. The number of possible prefixes gives no playback hit-rate estimate.

## Expected impact and confidence

Medium for CPU in multi-voice scenes. The repeated searches are certain; small resident tables may already perform well. Measure direct-hit rates, decoder CPU per channel and bit-exact PCM across valid and truncated frames.
