# Decode XMA frames into the voice's pending sample queue

## Location and execution path

The XMA worker in `crates/victory-hle/src/modules/audio.rs:495–559` pumps each enabled voice until its output ring fills. Its `decode` function (573–671) calls `read_frame` in `crates/victory-formats/src/xma.rs:128–151`, then `Decoder::decode` in the same file (646–673). Loop trimming and sample skipping happen before the HLE code appends samples to the voice queue.

## Current cost

Each successfully framed input allocates a byte vector for packed frame bits and a second vector for interleaved PCM. Both die after that frame. At 48 kHz and 512 samples per channel per frame, one continuously decoded voice drives about 94 frames and 188 such allocations per second. The decoder also appends a full frame of silence on decode failure to preserve timing. End-of-input flush creates a separate temporary for the overlap tail.

## Proposed redesign

Add a caller-buffer form of `read_frame` for the worker, clearing the logical length on each attempt, including after an incomplete frame. Keep the public owning `read_frame` as a wrapper for existing callers. Give `Decoder` a bulk output interface that exposes one decoded frame before advancing its overlap, retaining the existing `Vec<i16>` interface as a wrapper. The HLE can apply its loop-end trim, then its outstanding skip, and extend the voice's reserved PCM deque directly with the retained interleaved samples. Use a statically dispatched iterator or bulk writer so the inner loop has no indirect callback per sample. Drain the flush tail into the same deque before resetting the decoder. Keep unread samples already queued, silence and diagnostics on bad frames, guest context update order and the next loop's skip.

## Before and after

For F frames, the packed-byte scratch grows to the largest packetized frame encountered. The F temporary PCM buffers and the copy of each accepted sample into the voice queue disappear. The queue still grows to hold unread output, and each retained sample is still converted and stored once. Decode and overlap processing remain necessary even when a frame is trimmed.

## Expected impact and confidence

Medium for allocator activity in voice-heavy playback, lower for FPS than render-thread changes. High confidence in allocation removal after warmup. Measure allocations per decoded second, worker CPU and ring underruns at representative voice counts. The relative cost depends on how much time the transform takes.
