# Poll enabled XMA voices and let an idle decoder sleep

## Problem

In `crates/victory-hle/src/modules/audio.rs:246–285`, `AcpHalAllocateShapeContexts` allocates one mutex slot per requested XMA context. Mixer commands reach `Xma::enable`, `disable`, and `advance_read` through `xma_command` (412–510, 719–735). On enable or read advancement, `wake_decoder` unparks a single worker. After each pass that worker parks for up to 2 ms (700–715).

Every pass takes the XMA read guard and visits and locks every allocated slot just to find the enabled voices. With all voices disabled the worker still wakes on the timeout for the rest of the process. With a large reserved pool and a handful of audible voices, most lock acquisitions can produce no samples.

The pool size is known for this title. In `game_scarlett_master.exe` (SHA-256 prefix `e95020b2fa4a721d`), RVA `0x43aec68` writes `0x15e` to count 4 before calling `AcpHalAllocateShapeContexts` at `0x43aec73`. The success path clears 350 × 64 bytes of contexts. The output consumer at `0x78120` calculates readable bytes from the shared read/write indices and writable flag, then sends command 18 with the context index and the number of 256-byte blocks consumed. The launch log records single-context enable, read advance and disable (commands 5, 18 and 7). It does not count simultaneous voices.

## Change

Allocate an enabled bitset alongside the context array, sized from the actual request. This build needs six 64-bit words. Update membership under the voice lock when the slot crosses between enabled and disabled. Copy the words under a short membership lock, release it, then visit set bits in index order and recheck each slot under its lock. Keep the array read guard through the pass. Release it before parking. Park indefinitely after an empty snapshot and retain the 2 ms poll otherwise. Publish enables before the existing `unpark` so a racing enable leaves a wake token. Re-enabling an active index must still replace its decoder state and wake the worker. A read advance still updates the shared words even when its slot is disabled.

## Expected result

For C allocated contexts and A enabled voices, each nonempty pass copies ⌈C/64⌉ words and probes about A voice locks. The baseline probes 350 voice locks per pass on this build, including idle passes once the worker has started. Bitset storage is O(C/64) words. Keep timed polling for enabled voices since the shared words can change independently of the read-advance command, including resupplied input with `VICTORY_XMA_REFILL=1` (514–595).
