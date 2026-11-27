# Serialize delayed callback handoff with queue termination

## Problem

`Queue::submit` in `modules/xasync.rs` removes an expired callback from the delayed map before publishing it to the port. `XTaskQueueTerminate` can drain that port in between and strand an accepted manual callback. The same drain can cancel another queue’s work because composite queues share port FIFOs and the entries carry no owner. Both the EXE and `libHttpClient.GDK.dll` schedule 10 ms retries (RVAs `0x55f90f8` and `0x2f9f3`).

## Change

Track the submitting queue and logical port on pending tasks, including delayed and pool work. Under the queue lock, move each task from its delay into its port FIFO or claim it for execution. Use the same transition for direct submissions. Keep immediate callbacks on the submitting thread and invoke callbacks and monitors outside the locks, preserving port-before-monitor delivery and the async block’s `is_current` check.

Terminate in GDK order. Close work admission, dispatch that queue’s pending work as canceled through its configured port, and let running work finish while its completion port can still accept callbacks. Then close and drain completion the same way. Leave sibling entries in FIFO order, even when both logical ports use the same physical port. Run the termination callback on completion, and honor the caller’s `wait` flag. [GDK termination contract](https://learn.microsoft.com/en-us/xbox/gdk/docs/reference/system/xtaskqueue/functions/xtaskqueueterminate)

## Expected result

When termination overlaps expiry or direct submission, every accepted task of the terminating queue runs exactly once, with the cancellation flag set for pending work. Other queues continue using the shared port.

[Deferred optimization](deferred/xasync-share-one-timer-for-delayed-callbacks.md)
