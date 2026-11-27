# Handle the extra transform flags emitted by the archive request builder

## Problem

The archive builder in the Scarlett EXE (`0x4ffd70c–0x4ffd76f`) emits zlib requests with a nonzero DirectStorage swizzle mode. `dstorage.rs::execute` rejects those option bits with `E_NOTIMPL`. Inflating directly to the destination would also leave macro-shuffled data there.

## Change

Decode the 16-bit `SwizzleMode` at option bits 5–20 separately from zlib and BCPack. Implement the four macro-unshuffle layouts emitted by the builder.

| Archive mode | SwizzleMode | Segment sizes (KiB) |
| --- | --- | --- |
| 9 | 1 | 4, 4, 8 |
| 11 | 3 | 4, 12, 4, 4, 8 |
| 12 | 2 | 4, 4, 24 |
| 13 | 4 | 4, 12, 4, 12 |

For these requests, inflate the payload and apply the selected DirectStorage macro-unshuffle before committing the output to either destination path. Leave the final micro-unshuffle to the title's GPU shader. Preserve mode zero and the error path for unknown option values. Microsoft's texture-shuffling sample ships prepared textures and the matching runtime shaders. [GDK mode definitions](https://learn.microsoft.com/en-us/gaming/gdk/docs/reference/system/dstorage/enums/dstorage_swizzle_mode) · [Reference textures and shaders](https://learn.microsoft.com/en-us/samples/microsoft/xbox-gdk-samples/directstoragetextureshuffling/)

## Expected result

All four valid modes complete with the requested byte length. For each corresponding GDK sample texture, applying the sample's final shader to the HLE output must reproduce the original tiled BC blocks byte for byte.

[Deferred optimization](deferred/finding-dstorage-reuse-compressed-input-workspace.md)
