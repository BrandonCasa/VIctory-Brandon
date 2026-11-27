# Use a shared timer for delayed XAsync work

## Location and execution path

In `crates/victory-hle/src/modules/xasync.rs`, `XTaskQueueSubmitDelayedCallback` (378–387) and `XAsyncSchedule` (611–646) reach `Queue::submit` (214–245). For each positive delay it records the callback, clones the queue, spawns an OS thread and sleeps until the deadline. On wake it claims the task by ID and delivers it to the requested port. Zero delay already delivers directly.

## Current cost

N outstanding delays create N timer threads and N stack reservations. Terminating a queue cancels its stored tasks in submission order (391–420), but the sleepers keep their threads and queue references until the original deadlines. Bursty scheduling incurs thread creation and teardown even when callbacks do little work.

## Proposed redesign

Start one timer on the first positive delay. Key timers by monotonic deadline and submission ID, retaining the queue until expiry or cancellation. Record the deadline beside the queue-local task so termination can unlink timers directly and cancel in submission order. Publish both records under the queue lock with its termination recheck. Wake the timer when the earliest deadline changes and release its lock before taking queue locks. For a non-immediate port with no monitors, hold the monitor lock across the claim by ID and port submission to avoid a second dispatch. For other expirations, let reusable workers claim by ID and call the existing `deliver`, preserving monitor order and allowing blocking callbacks to run concurrently. Retire excess idle workers after bursts. Leave zero-delay delivery direct.

## Before and after

Timer threads fall from O(N) to one, plus workers actually delivering callbacks. Timer metadata stays O(N); a deadline heap costs O(log N) per insertion/removal. Remove canceled entries or release retained payloads promptly to avoid transferring the existing retention problem into the timer queue.

## Expected impact and confidence

High for titles that post many overlapping delays, particularly for thread count and virtual memory. Frequency is title-dependent; steady FPS impact is unknown. Measure peak simultaneous delays, thread count, wake latency, and canceled-work lifetime under a timer burst.
