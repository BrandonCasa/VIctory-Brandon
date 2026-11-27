# Preserve XMA overlap state across the title's context handoff

## Problem

The HLE starts each enabled voice with a fresh decoder and ignores context word 8. The title zeroes a 512-byte overlap area at EXE RVA `0x78670`, installs its APU address at `0x787f4`, and copies those bytes from the current context to the alternate context at `0x781c6`, just before releasing the current slot. The fork loses the outgoing decoder’s overlap at that boundary.

## Change

At handoff, take each channel’s unquantized overlap samples and previous block length from the outgoing decoder. If it has already flushed, use the snapshot saved against its word-8 backing immediately before `Decoder::flush` emitted PCM and reset that state. The title’s copy and zero calls go through the imported `memcpy` and `memset`. Extend the HLE import chain in `host.rs` so a 512-byte copy from a known overlap area to a live APU backing carries an owned snapshot alongside the normal byte copy. Key the pending handoff by the destination allocation generation and byte offset so the receiving context can claim it through word 8 on first enable. The matching zero-fill invalidates the old generation. Keep this behavior active with the optional GPU copy wrappers disabled, and forward ordinary CRT operations directly through the existing chain.

Serialize publication with the voice worker and restore the snapshot before decoding the receiving stream’s first dependent frame, including when its slot is enabled after the copy. Preserve the destination’s rate, channel setup and skips. Keep unread PCM on its original voice, respect the title’s output trim, and retire snapshots on backing reuse or teardown. Unrelated contexts start fresh.

## Expected result

Decode identical input continuously and split at the title's context handoff. The PCM spanning the split must match sample for sample, with no extra flush tail. A newly zeroed or unrelated context starts with fresh state.

[Deferred optimization](deferred/finding-xma-reuse-frame-scratch.md)
