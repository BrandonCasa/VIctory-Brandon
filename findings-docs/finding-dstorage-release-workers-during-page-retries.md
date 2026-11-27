# Let other storage requests run while physical destinations are unavailable

## Location and execution path

`crates/victory-hle/src/modules/dstorage.rs:775–800` runs `execute` synchronously on each worker until `Lanes::finish`. On page destinations, `page_list` (1717–1734) sleeps and rereads an unknown list every millisecond for up to 1.5 seconds. `into_pages` (1645–1680) can independently back off on failed physical writes.

## Current cost

A sleeping request keeps its worker. Several unavailable destinations can occupy the worker pool while other queues hold runnable requests. The write retry already owns decoded bytes, so rerunning the entire read or decode on each wake would add work.

## Proposed redesign

Keep the ready path synchronous. On a retry, park the job until its next poll or backoff using a scheduler deadline and a timed condition-variable wait. Page-list polling must reread the guest list until the original deadline, then use its last contents. A failed scatter retries the same page IDs and filled prefix without rereading or decoding. Keep any source or intermediate needed to refill a changed list independently of worker scratch. Cap parked jobs and their total owned allocation capacity, including zero-staging memory requests; fall back to the current in-worker retry when full. Keep the original staging charge and running sequence until final notification and completion, so queue close still waits.

## Before and after

For W workers and S simultaneous sleepers, available execution capacity changes from max(0,W−S) to W while eligible requests exist, subject to staging and memory limits. Polls/retries may still occur. Pending states need scheduling metadata and ownership of any buffered payload.

## Expected impact and confidence

High conditional impact on streaming tail latency when destination pages arrive late. The worker blockage is certain in that case. Quantify simultaneous sleepers, duration, ready work and staged bytes; healthy ready-page requests gain little.
