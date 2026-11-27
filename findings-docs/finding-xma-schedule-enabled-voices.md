# Poll enabled XMA voices and let an idle decoder sleep

## Location and execution path

In `crates/victory-hle/src/modules/audio.rs:246–285`, `AcpHalAllocateShapeContexts` allocates one mutex slot per requested XMA context. Mixer commands reach `Xma::enable`, `disable`, and `advance_read` through `xma_command` (412–510, 719–735). On enable or read advancement, `wake_decoder` unparks a single worker. After each pass that worker parks for up to 2 ms (700–715).

## Current cost

Every pass takes the XMA read guard and visits and locks every allocated slot just to find the enabled voices. With all voices disabled the worker still wakes on the timeout for the rest of the process. With a large reserved pool and a handful of audible voices, most lock acquisitions can produce no samples.

## Proposed redesign

Maintain a compact set of enabled indices as part of the context array. Update membership together with the slot on enable/disable, including bulk commands. Have the worker snapshot membership, release the membership lock, then check each slot under its existing voice lock. Keep the array read guard while using indices. Publish membership before waking the worker and use a wake token or predicate so an enable racing the transition to idle is observed. When the set is empty, park until signaled. Clear membership with the array on release.

## Before and after

For C allocated contexts, A enabled voices and W passes, voice-lock probes fall from W × C to about W × A. The index costs O(A) storage with a compact list. Audio decoding itself is unchanged. Keep timed polling for enabled voices since the title can change shared context words without a command, including resupplying input with `VICTORY_XMA_REFILL=1` (514–595).

## Expected impact and confidence

Medium for background CPU with sparse pools, lower for frame time than work directly on the submission thread. High confidence in the idle wakeups and slot visits. Compare context count, enabled count, worker wakeups, CPU time and mixer-command latency during silence and during playback. A nearly full pool limits the scan saving.
