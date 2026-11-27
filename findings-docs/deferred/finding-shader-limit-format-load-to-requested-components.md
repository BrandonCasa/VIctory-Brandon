# Skip unused component conversions in explicit-format loads

## Problem

`crates/victory-shader/src/emit/lower.rs:12236,12656–12672` routes an explicit-format buffer load through `format_load`, then optionally packs d16 results. At 7469–7505 and 7602–7610 the lowerer builds a raw load mask from the full format width, converts all components, pads missing channels and truncates to the requested count.

A one-channel instruction on a four-word format emits a four-word raw load mask. Building the unused field extractions and conversions also costs translator work. Downstream dead-code elimination can already remove computations unreachable from the result.

The 1,119 distinct successful shader hashes in the November 27 launch log all match original containers in the EXE or `shaders.rpf` (archive SHA-256 prefix `0bc1634594e51ca8`). Their reachable GFX10 code contains 2,201 explicit-format load instructions. At 2,138 sites the instruction consumes every stored field or asks for more. The remaining 63 comprise 50 `8_8_8_8_UNORM` and 13 `2_10_10_10_UNORM` loads. Each format occupies one dword, so **none of the 2,201 sites can reduce the raw dword count**. For example, archive record `0x40158`, `MSNC_VS_Cable_da99eb95_Wrapped` (hash `38ad582f523fd7cf`), has an XYZ load of the latter format at native PC `0x1e8`. It can omit the fourth conversion while fetching the same word.

## Change

In `format_load`, extract and convert only the first `min(channels, bits.len())` fields. Keep the raw-load mask and choose alignment from the entire element width. Apply the same zero/one defaults and d16 packing to the requested outputs.

## Expected result

The translator omits one unused fourth-component conversion at each of 63 packed XYZ load sites. All 2,201 reachable sites fetch the same number of dwords, and the values returned to the shader stay identical. Downstream dead-code elimination may already remove the unused conversions from the final GPU program.
