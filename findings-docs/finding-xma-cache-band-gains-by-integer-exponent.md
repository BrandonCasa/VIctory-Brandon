# Reuse XMA quantization gains for repeated integer exponents

## Location and execution path

`crates/victory-formats/src/xma.rs:963,1037–1065,1082–1104` computes a per-band gain inside `Decoder::subframe`, before weighting coefficients and running the inverse transform. Active voices reach this from `crates/victory-hle/src/modules/audio.rs:625–632`.

## Current cost

Each transmitted subframe evaluates `10f32.powf(exp as f32 / 20.0)` for every processed channel and band. The integer exponent completely determines the result. Many decoder instances can request the same gains, while silent subframes already bypass this path.

## Proposed redesign

Use a shared bounded table for common integer exponents populated with the existing f32 expression. Fall back to the existing calculation outside the table because escaped quantization steps can be large. Preserve the precise operation order for weighting coefficients and use bit-for-bit validation of gain and final PCM, including underflow or overflow.

## Before and after

For B band evaluations and H table hits, steady-state power evaluations fall from B to B−H, plus one-time table initialization. Multiplication and inverse transforms still run. A larger exponent window costs additional static memory and construction time.

## Expected impact and confidence

Medium for decoder CPU with active voices. The transcendental call site is certain; the exponent distribution and its share of total decode time need measurement. Compare exponent histograms, gain bit patterns and per-frame decode CPU.
