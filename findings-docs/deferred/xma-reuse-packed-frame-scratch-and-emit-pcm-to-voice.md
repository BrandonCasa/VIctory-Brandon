# Decode XMA frames into the voice's pending sample queue

## Problem

The XMA worker in `crates/victory-hle/src/modules/audio.rs:495–559` pumps each enabled voice until its output ring fills. Its `decode` function (573–671) calls `read_frame` in `crates/victory-formats/src/xma.rs:128–151`, then `Decoder::decode` in the same file (646–673). Loop trimming and sample skipping happen before the HLE code appends samples to the voice queue.

Each successfully framed input allocates a byte vector for packed frame bits and a second vector for interleaved PCM. Both die after that frame. At 48 kHz and 512 samples per channel per frame, one continuously decoded voice drives about 94 frames and 188 such allocations per second. The decoder also appends a full frame of silence on decode failure to preserve timing. End-of-input flush creates a separate temporary for the overlap tail.

## Change

Reuse a packed-frame buffer on the single decoder worker. Clear its logical length on each attempt and retain the owning read_frame wrapper. Compute the accepted sample range from the loop endpoint and outstanding skip, in interleaved channel units. Emit that range directly into the voice queue before the decoder shifts its overlap buffer for the next frame. Keep the public append-to-Vec decoder entry point for existing callers. Keep failure silence, queued PCM, flush and overlap lifetimes intact.

## Expected result

For F frames, the packed-byte scratch grows to the largest packetized frame encountered. The F temporary PCM buffers and the copy of each accepted sample into the voice queue disappear. The queue still grows to hold unread output, and each retained sample is still converted and stored once. Decode and overlap processing remain necessary even when a frame is trimmed.

[Related correctness finding](../xma-preserve-decoder-overlap-across-context-handoffs.md)
