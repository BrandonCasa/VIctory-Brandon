# Wake a compiler worker that can service the new pipeline

## Problem

In `crates/victory-gpu/src/pipeline/make.rs:543–641`, pipeline creation/first use enqueues or promotes a pending PSO, then calls `AVAILABLE.notify_all()`. All workers wait on the same condition variable. Ordinary workers accept screen, requested, early and permitted slow jobs. Screen-only workers consume only the screen queue. The default ordinary pool scales to 2–16 workers, with two extra screen workers when screen prioritization is enabled (638–673).

One ordinary job can wake every idle compiler worker. Each awakened worker reacquires the shared queue mutex, but only one can claim that job and some cannot claim it at all. This repeats for staggered enqueues and promotions. During a saturated compile burst few workers are asleep, which limits the opportunity.

## Change

Use separate condition variables for ordinary and screen-only workers under the existing queue mutex. Signal an ordinary sleeper for newly eligible ordinary work. For screen work, prefer a screen-only sleeper that has not already been signaled, then an ordinary sleeper. Account for outstanding signals under the mutex so a burst can wake additional eligible workers before the first has reacquired the lock. Keep the existing priority/claim loop on wake and on worker entry. When a slow slot is released on completion or in `lost`, signal one ordinary sleeper if slow work is pending. A capped slow enqueue alone needs no wake. Leave `DONE` and frame wait budgets unchanged.

## Expected result

For one runnable job and W idle workers, wakeups and competing mutex reacquisitions fall from up to W to one under stable conditions. Queue state adds O(1) counters/channels. The cost and order of the compilation itself stay the same.
