# Honor the title's submit boundary for live page lists

## Problem

The archive builder at EXE RVA `0x4ffd520` enqueues a pointer into a live page table. At `0xe88f0` the title applies mapping changes, increments per-page references, enqueues status index zero and submits. `VICTORY_DSTORAGE_HALF` can start a request earlier while the old allocation's page IDs still pass ownership checks. Retrying later cannot undo that write.

## Change

Remove the opt-in half-full submission on this title path. The default already waits for explicit Submit, and the observed 4,095-request batches fit the 4,159-entry queues. Keep the separate full-queue liveness fallback and the existing `Lanes::finish` / `Lane::flush` ordering. The title releases its page references after IsComplete(0) succeeds in the routine at `0xe8960`.

## Expected result

With an owned old page in the table at enqueue and a replacement published before Submit, only the replacement receives the payload. The old bytes survive and status index zero completes after publication.

[Deferred optimization](deferred/finding-dstorage-release-workers-during-page-retries.md)
