# Reuse packet and PCM scratch across XMA frames

## Location and execution path

The XMA worker in `crates/victory-hle/src/modules/audio.rs:495–559` pumps each enabled voice until its output ring fills. Its `decode` function (573–671) calls `read_frame` in `crates/victory-formats/src/xma.rs:128–151`, then `Decoder::decode` in the same file (646–673). Loop trimming and sample skipping happen before the HLE code appends samples to the voice queue.

## Current cost

Each successfully framed input allocates a byte vector for packed frame bits and a second vector for interleaved PCM. Both die after that frame. At 48 kHz and 512 samples per channel per frame, one continuously decoded voice drives about 94 frames and 188 such allocations per second. The decoder also appends a full frame of silence on decode failure to preserve timing. End-of-input flush creates a separate temporary for the overlap tail.

## Proposed redesign

Add a caller-buffer form of `read_frame` that returns frame offsets and bit count, retaining the owning interface for existing callers. Give the single worker reusable packed-byte and PCM vectors. Clear lengths before each frame or flush and retain capacity. Finish logging, truncating and copying samples into the correct voice queue before servicing another voice. Keep per-voice decoder overlap, ring indices and pending samples independent.

## Before and after

For F frames, scratch allocation count falls from approximately 2F to capacity growth for the largest packed frame and channel count encountered. Retained scratch is bounded by these maxima per worker. Packet gathering across gaps, transform work and copying accepted samples into the voice queue still scale with the amount decoded.

## Expected impact and confidence

Medium for allocator activity in voice-heavy playback, lower for FPS than render-thread changes. High confidence in allocation removal after warmup. Measure allocations per decoded second, worker CPU and ring underruns at representative voice counts. The relative cost depends on how much time the transform takes.
