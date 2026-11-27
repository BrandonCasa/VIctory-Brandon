# Wake a compiler worker that can service the new pipeline

## Location and execution path

In `crates/victory-gpu/src/pipeline/make.rs:543–641`, pipeline creation/first use enqueues or promotes a pending PSO, then calls `AVAILABLE.notify_all()`. All workers wait on the same condition variable. Ordinary workers accept screen, requested, early and permitted slow jobs. Screen-only workers consume only the screen queue. The default ordinary pool scales to 2–16 workers, with two extra screen workers when screen prioritization is enabled (638–673).

## Current cost

One ordinary job can wake every idle compiler worker. Each awakened worker reacquires the shared queue mutex, but only one can claim that job and some cannot claim it at all. This repeats for staggered enqueues and promotions. During a saturated compile burst few workers are asleep, which limits the opportunity.

## Proposed redesign

Keep the existing priority queues and claim state. Give ordinary and screen-only waiters distinct wake channels under the queue mutex, with idle counts. Signal one eligible idle ordinary worker for ordinary work. For screen work, prefer an idle screen worker and otherwise wake an ordinary one. On release of slow capacity, worker replacement or promotion, notify according to the newly runnable jobs and worker classes. The job remains discoverable to an arriving worker even if no one was waiting. Leave draw completion and frame wait budgets on their existing protocol.

## Before and after

For one runnable job and W idle workers, wakeups and competing mutex reacquisitions fall from up to W to one under stable conditions. Queue state adds O(1) counters/channels. The cost and order of the compilation itself stay the same.

## Expected impact and confidence

Medium for intermittent pipeline arrivals while the title is active, low when the pool is constantly busy. The broadcast and eligibility mismatch are explicit. Measure wakeups that claim no work, queue-lock contention and latency for first-use screen PSOs.
