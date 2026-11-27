# Use a shared timer for delayed XAsync work

## Problem

In `crates/victory-hle/src/modules/xasync.rs`, `XTaskQueueSubmitDelayedCallback` (378–387) and `XAsyncSchedule` (611–646) reach `Queue::submit` (214–245). For each positive delay it records the callback, clones the queue, spawns an OS thread and sleeps until the deadline. On wake it claims the task by ID and delivers it to the requested port. Zero delay already delivers directly.

N outstanding delays create N timer threads and N stack reservations. Terminating a queue cancels its stored tasks in submission order (391–420), but the sleepers keep their threads and queue references until the original deadlines. Bursty scheduling incurs thread creation and teardown even when callbacks do little work.

## Change

Use a shared monotonic timer ordered by deadline and submission ID. Keep the callback owned by its queue until the port claims it. Pop due IDs in order, release the timer lock, and use the queue’s synchronized handoff to publish them to their ports. A canceled ID becomes a no-op. Keep blocking guest callbacks and monitor calls off the timer thread, and preserve FIFO publication order for callbacks due on the same port. Unlink canceled deadlines promptly and wake the timer when its earliest deadline changes. Keep zero-delay delivery direct. The EXE schedules at zero delay on begin (RVA 0x55f91c9) and can retry after 10 ms.

## Expected result

Timer threads fall from O(N) to one, plus workers actually delivering callbacks. Timer metadata stays O(N); a deadline heap costs O(log N) per insertion/removal. Remove canceled entries or release retained payloads promptly to avoid transferring the existing retention problem into the timer queue.

[Related correctness finding](../finding-xasync-share-delayed-task-timer.md)
