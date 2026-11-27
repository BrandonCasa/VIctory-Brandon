# Serialize delayed callback handoff with queue termination

## Problem

`Queue::submit` in `modules/xasync.rs` removes an expired callback from the delayed map before publishing it to the port. `XTaskQueueTerminate` can drain that port in between and leave an accepted callback stranded. Both the EXE and `libHttpClient.GDK.dll` schedule 10 ms retries (RVAs `0x55f90f8` and `0x2f9f3`).

## Change

Serialize expiry, direct submission and termination admission under the queue lock. For a manual or serialized port, claim and publish the task to its FIFO before unlocking. Close admission under the same lock before draining and canceling remaining tasks in order. Invoke guest callbacks and monitors outside the locks, preserve port-before-monitor delivery, and retain the async block's `is_current` check.

## Expected result

When termination overlaps expiry or direct submission, every accepted task is dispatched or canceled exactly once. The terminated queue has no callback left in its FIFO.

[Deferred optimization](deferred/finding-xasync-share-delayed-task-timer.md)
