# Use a shared timer for delayed XAsync work

## Location and execution path

In `crates/victory-hle/src/modules/xasync.rs`, `XTaskQueueSubmitDelayedCallback` (378–387) and `XAsyncSchedule` (611–646) reach `Queue::submit` (214–245). For each positive delay it records the callback, clones the queue, spawns an OS thread and sleeps until the deadline. On wake it claims the task by ID and delivers it to the requested port. Zero delay already delivers directly.

## Current cost

N outstanding delays create N timer threads and N stack reservations. Terminating a queue cancels its stored tasks in submission order (391–420), but the sleepers keep their threads and queue references until the original deadlines. Bursty scheduling incurs thread creation and teardown even when callbacks do little work.

## Proposed redesign

Put positive delays on a process-wide deadline queue serviced by one timer thread. Wake it when an earlier deadline arrives. Retain the owning queue while the callback is live and use the existing queue-local claim-by-ID protocol for cancellation versus delivery. Hand due deliveries to runnable workers. Immediate-port callbacks and monitors can block or re-enter, so they must run outside the timer lock and must not delay unrelated expirations. Keep zero-delay delivery, port semantics and queue termination order.

## Before and after

Timer threads fall from O(N) to one, plus workers actually delivering callbacks. Timer metadata stays O(N); a deadline heap costs O(log N) per insertion/removal. Remove canceled entries or release retained payloads promptly to avoid transferring the existing retention problem into the timer queue.

## Expected impact and confidence

High for titles that post many overlapping delays, particularly for thread count and virtual memory. Frequency is title-dependent; steady FPS impact is unknown. Measure peak simultaneous delays, thread count, wake latency, and canceled-work lifetime under a timer burst.
