# Run independent raw reads during a staging-capacity stall

## Problem

`crates/victory-hle/src/modules/dstorage.rs:775–800,840–879,969–977` connects the worker condition variable, `Lanes::start`, completion and `staging_bytes`. The scheduler chooses a class and lane before testing whether the head fits.

When the winning head exceeds the available staging allowance, `start` returns no job. An idle worker then sleeps even if another lane's front read requires zero staging. This can also occur when a single already-admitted oversized request has put usage above the normal limit.

Ghidra, Scarlett EXE SHA-256 prefix `e95020b2fa4a721d`. RVA `0x1d8e4d0` asks for 32 MiB of staging. The no-chunk-table branch at `0x4ffd7ab` emits raw file-to-address reads in pieces up to 128 MiB, with equal source and destination sizes and options zero. Each of those reads costs zero under the fork's `staging_bytes`. The archive branch at `0x4ffd6a1` can set the page-destination bit even for raw file data. Those reads incur a positive charge under the current rule. The launch log confirms 17 file queues, all at priority 2 and capacity 4,159, and the 32 MiB setting. It does not record a blocked head with a runnable alternative.

The 17 observed file queues share the highest class because the fork clamps priority 2 into that class.

## Change

Keep the usual winner when it fits, including any head with `staging_bytes == 0` even during an oversized request. Only when a positive reservation is refused, select the best other *submitted* zero-charge head, if present, using the same class, lane and sequence tie-breaks. Charge the actual start, but derive the arrival-time baselines from the minimum clocks of waiting classes and lanes so bypassing a blocked head cannot inflate those baselines. Try the normal winner on every start. Wake capacity waiters when staged bytes are released or the limit increases, rather than on every zero-byte completion.

## Expected result

With spare workers and an independent raw head, an idle wait can overlap productive I/O or copying without reserving another byte of staging. A fallback scan costs O(L) for L lanes on a refusal. Single-lane queues and fully occupied workers gain nothing.
