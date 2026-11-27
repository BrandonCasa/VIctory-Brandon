# Preserve XMA overlap state across the title's context handoff

## Problem

The HLE creates a fresh decoder on enable and ignores context word 8. The title zeroes a 512-byte overlap area at EXE RVA `0x785d0`, installs its APU address at `0x787f4`, and copies those bytes between two live contexts at `0x781c6`. Resetting there loses the preceding stream's overlap.

## Change

Implement this title's explicit handoff in the audio HLE. Associate the outgoing decoder's continuation state with its word-8 backing before an end flush can consume it. At the title’s 512-byte copy call (RVA `0x781c6`, callee `0x5f40560`), keep the original byte copy and transfer an owned snapshot between the two corresponding word-8 areas. Serialize that transfer with the decoder worker so destination decode cannot race snapshot publication. Consume it once for the receiving stream before its first dependent frame. Invalidate snapshots on fresh initialization, backing reuse and teardown. Keep unread PCM with its owning voice, preserve loop and frame trimming, and emit the overlap tail only once across the handoff. Use the existing fresh decoder path for unrelated contexts. This implements the observed copy without assuming a hardware encoding for arbitrary guest edits.

## Expected result

Decode identical input continuously and split at the title's context handoff. The PCM spanning the split must match sample for sample, with no extra flush tail. A newly zeroed or unrelated context starts with fresh state.

[Deferred optimization](deferred/finding-xma-reuse-frame-scratch.md)
