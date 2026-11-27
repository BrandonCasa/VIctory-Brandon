# Observe decoder writes before accepting a texture as clean

## Problem

A clean journal at the staging VA can hide writes through the decoder's alias. The EXE registers per-slot and per-plane buffers at `0x442f9b8`, then starts and waits for async decode at `0x2ef265` and `0x2ef274`. Unmap can precede the write. `Bink2Scarlett.dll` saves the registered table at `0x48e0` and consumes it at `0x6090`.

## Change

Before the clean return in `texture.rs::Staging::refresh_timed`, require coverage of the producer's backing and generation. Propagate dirty spans through the shared backing to its current aliases. For an untracked decoder write, compare source bytes after decode completion or take the existing full-refresh path. Track baselines per slot and plane and invalidate the uploaded baseline on destination replacement, another staging writer or a GPU write.

## Expected result

After slot rotation and an alias write following Unmap, the consumed plane contains the new decoded frame even if its staging-VA journal is clean. Replacing the destination forces the plane to be populated again.

[Deferred optimization](deferred/finding-texture-upload-only-dirty-subresources.md)
