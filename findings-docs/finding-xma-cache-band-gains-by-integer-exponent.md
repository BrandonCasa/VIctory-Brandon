# Reuse XMA quantization gains for repeated integer exponents

## Location and execution path

`crates/victory-formats/src/xma.rs:963,1037–1065,1082–1104` computes a per-band gain inside `Decoder::subframe`, before weighting coefficients and running the inverse transform. Active voices reach this from `crates/victory-hle/src/modules/audio.rs:625–632`.

## Current cost

Each transmitted subframe evaluates `10f32.powf(exp as f32 / 20.0)` for every processed channel and band. The integer exponent completely determines the result. Many decoder instances can request the same gains, while silent subframes already bypass this path.

## Proposed redesign

Build one shared contiguous table with the existing f32 expression. An unescaped step tops out at 120, the stereo modifier adds at most 128, and the band adjustment only subtracts. An upper bound of 248 therefore covers that side of every unescaped case. Choose the lower bound from observed band exponents to keep initialization and cache footprint small. Acquire the table once per transmitted subframe, index it directly in the band loop, and use the original expression on an out-of-range exponent. Preserve the coefficient multiplication order and check bit-exact gain and PCM, including underflow and overflow.

## Before and after

For B band evaluations and H table hits, steady-state power evaluations fall from B to B−H, plus one-time table initialization. Multiplication and inverse transforms still run. A larger exponent window costs additional static memory and construction time.

## Expected impact and confidence

Medium for decoder CPU with active voices. The transcendental call site is certain; the exponent distribution and its share of total decode time need measurement. Compare exponent histograms, gain bit patterns and per-frame decode CPU.
