# Share XMA band layouts across decoder instances of the same rate

## Problem

`crates/victory-hle/src/modules/audio.rs:620–631` lazily constructs a decoder per voice. `crates/victory-formats/src/xma.rs:573–629` normalizes the sample rate to four choices and computes band counts, boundaries and cross-block scale-factor mappings. Decoding uses the tables in the scale-factor and subframe paths (around 914 and 1086).

Every new voice computes and owns the same three arrays as every other decoder at that normalized rate. Building the cross-block mappings nests block-size and band loops with a linear search of the target bands. Channel count and stream history do not participate in these calculations.

The title constructs this cache key before the HLE sees it. In `game_scarlett_master.exe` (SHA-256 prefix `e95020b2fa4a721d`), the routine at RVA `0x785d0` encodes its input rate into context word 1, bits 27–28, using upper boundaries of 24,000, 32,000 and 44,100 Hz. The HLE already maps that field to the four canonical rates. Four lazy entries keyed by the decoder's existing normalized rate cover the title and the public constructor. Channels, physical context index and addresses need no place in that key. The title separately copies 512 bytes of per-context state at `0x781c6`, so that data has its own lifetime.

## Change

Construct one immutable layout per normalized rate on first use and store a shared reference in `Decoder`. Keep channel overlap buffers, predictive state and reset behavior private to each decoder. Preserve integer arithmetic, boundary rounding and the four rate thresholds exactly.

## Expected result

For V decoder constructions covering R rates, layout builds fall from V to R (R at most four). Live voices share arrays instead of owning individual copies. The cache retains used layouts for the process lifetime. Long-lived voices already pay this construction only once.
