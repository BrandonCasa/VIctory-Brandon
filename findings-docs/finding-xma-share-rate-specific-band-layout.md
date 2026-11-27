# Share XMA band layouts across decoder instances of the same rate

## Location and execution path

`crates/victory-hle/src/modules/audio.rs:620–631` lazily constructs a decoder per voice. `crates/victory-formats/src/xma.rs:573–629` normalizes the sample rate to four choices and computes band counts, boundaries and cross-block scale-factor mappings. Decoding uses the tables in the scale-factor and subframe paths (around 914 and 1086).

## Current cost

Every new voice computes and owns the same three arrays as every other decoder at that normalized rate. Building the cross-block mappings nests block-size and band loops with a linear search of the target bands. Channel count and stream history do not participate in these calculations.

## Proposed redesign

Construct one immutable layout per normalized rate on first use and store a shared reference in `Decoder`. Keep channel overlap buffers, predictive state and reset behavior private to each decoder. Preserve integer arithmetic, boundary rounding and the four rate thresholds exactly.

## Before and after

For V decoder constructions covering R rates, layout builds fall from V to R (R at most four). Live voices share arrays instead of owning individual copies. The cache retains used layouts for the process lifetime. Long-lived voices already pay this construction only once.

## Expected impact and confidence

Medium to lower for steady frame time, more relevant for many short-lived voices and decoder footprint. High confidence in duplicated initialization and storage. Measure voice creation rate, live decoder count and initialization time.
