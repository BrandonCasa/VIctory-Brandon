# Reuse XMA quantization gains for repeated integer exponents

## Problem

`crates/victory-formats/src/xma.rs:963,1037–1065,1082–1104` computes a per-band gain inside `Decoder::subframe`, before weighting coefficients and running the inverse transform. Active voices reach this from `crates/victory-hle/src/modules/audio.rs:625–632`.

Each transmitted subframe evaluates `10f32.powf(exp as f32 / 20.0)` for every processed channel and band. The integer exponent completely determines the result. Many decoder instances can request the same gains, while silent subframes already bypass this path.

## Change

Build one shared contiguous table with the existing f32 expression. An unescaped step tops out at 120, the stereo modifier adds at most 128, and the band adjustment only subtracts. An upper bound of 248 therefore covers that side of every unescaped case. Cache exponents from -256 through 248 inclusive (505 f32 entries, 2,020 bytes). The lower bound caps storage. The fallback covers every smaller value. Acquire the table once per transmitted subframe, index it directly in the band loop, and use the original expression on an out-of-range exponent. Preserve the coefficient multiplication order and check bit-exact gain and PCM, including underflow and overflow.

## Expected result

For B band evaluations and H table hits, steady-state power evaluations fall from B to B−H, plus one-time table initialization. Multiplication and inverse transforms still run. A larger exponent window costs additional static memory and construction time.
