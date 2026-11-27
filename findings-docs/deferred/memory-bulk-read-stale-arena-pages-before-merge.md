# Read a stale arena page once before merging its pieces

## Problem

`crates/victory-gpu/src/memory.rs:82–86,499–542,633–655,980–984` defines 64 KiB pages and the 4 KiB comparisons in `Memory::bring` and `Memory::take`. Shader address preparation can call `bring`; deferred builder landing can call `take`. `crates/victory-gpu/src/host.rs:8846–8916` implements the common mapped-copy fast path and hole handling.

After freshness checks select a page, a full update calls `copy_mapped` sixteen times. Pieces are needed for comparison, watch notification or merge semantics, but the function also repeats the successful read setup for every piece under the shared memory lock. A call is not necessarily a kernel transition.

Ghidra shows two addresses in the title's rehydration path. In `game_scarlett_master.exe` (SHA-256 prefix `e95020b2fa4a721d`), RVA `0xea6c60` conditionally translates an address for CPU access, adds the header offset and stores it in a side array at `0xea6dc2`. It puts the original address plus that offset in descriptor field `+0x20` at `0xea6e06`. In `XboxBvhBuilder.dll` (`99f803ac22c01ef3`), RVA `0x14700` reads the header through that side array when supplied. The first dispatch binds the descriptor's source and destination GPU addresses. A readable CPU header at record time establishes neither a complete payload snapshot nor the lifetime of the GPU source mapping.

## Change

On the first `bring` for a page, `merge(..., never = true)` overwrites the entire arena page. Use `copy_mapped` directly into that arena page with `title_dst = false`, count the full page as changed, and keep the full resend. Preserve the diagnostic no-copy behavior separately. For later merges and `take`, the default safe-copy path uses `rep movsb`, and a 4 KiB scratch is immediately reused; fewer calls alone do not justify a 64 KiB scratch there. Target bulk scratch reads first at multi-piece spans using the `ReadProcessMemory` backend. Read exactly the selected span into reusable scratch, retain the current piece boundaries for comparisons and watch notifications, and preserve sparse zero-fill and whole-resend behavior.

Apply the bulk read at the existing read point. Only the first `bring` can overwrite `held.at` directly. At submission retain `land`'s mapping and header checks and `take`'s exact interval, including when two structures share a page.

## Expected result

For P full, nonfaulting pages eligible for bulk reads, the top-level calls fall from 16P to P. The first read needs no intermediate scratch. Later bulk merges still compare the selected bytes and use up to 64 KiB of scratch per active owner. The default safe-copy path keeps its 4 KiB scratch for later merges. Faulting ranges can incur additional fallback work.
