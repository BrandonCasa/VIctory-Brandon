# Observe decoder writes before accepting a texture as clean

## Problem

A clean journal at the staging VA can hide writes through the decoder's alias. The EXE registers per-slot and per-plane buffers at `0x442f9b8`, then starts and waits for async decode at `0x2ef265` and `0x2ef274`. Unmap can precede the write. `Bink2Scarlett.dll` saves the registered table at `0x48e0` and consumes it at `0x6090`.

## Change

Base the upload on the completed decode for the registered frame slot and plane. For a tracked producer, publish dirty spans by backing and generation so every alias sees them. Otherwise take a stable copy of that plane after decode completion and before slot reuse. Compare it with the version installed in the destination or upload the complete plane. Both the one-shot and journal clean returns in `Staging::refresh_timed` must honor that decision. Invalidate the installed version on destination replacement, another staging writer or a GPU write. A timed-out `BinkDoFrameAsyncWait` leaves the decode pending.

## Expected result

After slot rotation and an alias write following Unmap, the consumed plane contains the new decoded frame even if its staging-VA journal is clean. Replacing the destination forces the plane to be populated again.

[Deferred optimization](deferred/texture-detile-and-upload-only-changed-footprints.md)
