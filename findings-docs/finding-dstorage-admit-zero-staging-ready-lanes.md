# Run independent raw reads during a staging-capacity stall

## Location and execution path

`crates/victory-hle/src/modules/dstorage.rs:775–800,840–879,969–977` connects the worker condition variable, `Lanes::start`, completion and `staging_bytes`. The scheduler chooses a class and lane before testing whether the head fits.

## Current cost

When the winning head exceeds the available staging allowance, `start` returns no job. An idle worker then sleeps even if another lane's front read requires zero staging. This can also occur when a single already-admitted oversized request has put usage above the normal limit.

## Proposed redesign

Keep the usual winner when it fits, including any head with `staging_bytes == 0` even during an oversized request. Only when a positive reservation is refused, consider other lane heads with zero charge using the same class, lane and sequence tie-breaks. Charge the actual start, but derive the arrival-time baselines from the minimum clocks of waiting classes and lanes so bypassing a blocked head cannot inflate those baselines. Reconsider the normal winner on every start. Wake capacity waiters when staged bytes are released or the limit increases, rather than on every zero-byte completion.

## Before and after

With spare workers and an independent raw head, an idle wait can overlap productive I/O or copying without reserving another byte of staging. A fallback scan costs O(L) for L lanes on a refusal. Single-lane queues and fully occupied workers gain nothing.

## Expected impact and confidence

Medium to high under mixed-queue staging pressure. The admission failure and zero-byte class are explicit. Measure staging refusals with eligible alternative heads, worker idle time, staged-request fairness and latency by class.
