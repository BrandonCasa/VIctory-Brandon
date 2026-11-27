# Decode common XMA entropy codes with a prefix lookup

## Problem

`crates/victory-hle/src/modules/audio.rs:620–631` invokes the XMA decoder for an active voice. In `crates/victory-formats/src/xma.rs:238–291` shared `Vlc` tables use `partition_point` for each symbol. Coefficient/vector decoding (824–895) and scale-factor loops (924–949) call it repeatedly during transmitted subframes.

Codebooks are shared already. Searching their sorted left-aligned codes still costs a sequence of dependent comparisons for every decoded symbol. An escaped vector may require further vector and scalar codebook lookups.

## Change

Build a shared 8-bit prefix table as each `Vlc` is initialized. Pack each entry into four bytes (7 KiB across the seven codebooks). A resolved entry holds the signed symbol and consumed length. For an ambiguous prefix, store the first and last candidate indices from the existing search at the prefix's two extremes. Read `peek32` once and search only that slice on a miss. Advance by the decoded length in both paths, preserving zero padding and overread behavior. Check each code boundary against the existing lookup.

## Expected result

For N symbols and a codebook of K entries, the old path performs O(N log K) comparisons. With H direct hits, H searches become indexed loads and the remainder take a fallback. A b-bit prefix costs 2^b entries per table. At b = 8, between 232 and 255 of the 256 possible prefixes resolve directly in each of the seven current codebooks. The number of possible prefixes gives no playback hit-rate estimate.
