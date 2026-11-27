# Collect declared shader writes without a second traversal of all command steps

## Location and execution path

In `crates/victory-gpu/src/host.rs:7242–7253`, `HostList::defer_gpu_write` records a `Step::GpuWrite`. During submission, `run_deferred` processes these steps in order, flushing pending refreshes and calling `shadow.writing` (around 7720–7741). Later the same submission calls `declared_writes` before `card_before` (around 5924–5934). The collector (4016–4039) reacquires each list lock and visits every step to find unique write ranges.

## Current cost

A list rich in copies, refreshes or CPU callbacks but with few shader writes still incurs another traversal of its full step stream. Even a write-free list is inspected end to end. Optional queue joining also asks for the declarations before execution (`joinable`, 3300–3314). The existing record-time set suppresses repeated writes only until a title write intervenes.

## Proposed redesign

Reuse the lookup already paid by `defer_gpu_write`. Map each (shadow identity, address, length) to its last title-write generation and append a declaration to an ordered summary only on first insertion. Advance the generation where `title_written` currently clears `gpu_written`. Emit the ordered step on the first declaration in each generation, or on every call with `VICTORY_DECLARE_ONCE=0`. Reset both structures with `Deferred`. Merge the summaries in run order for `declared_writes` and `joinable`, retaining submission-wide deduplication and shadow ownership. This avoids an additional recording-time hash lookup or a close-time scan, while retaining keys across title writes costs space proportional to distinct declarations in the whole recording.

## Before and after

For T steps and U per-list unique declarations summed over the submitted list occurrences, the collector examines O(U) entries instead of O(T), using O(U) extra metadata across the recordings. Recording adds bookkeeping for declarations. Work saved is largest when U is small relative to T; a stream made mostly of unique writes may lose.

## Expected impact and confidence

Medium to high for CPU-bound submission with long, sparse-write recordings. The full extra traversal is explicit and independent of driver behavior. Measure T/U, record-time overhead, submit-lock duration and equality of declared ranges and copy-back results.
