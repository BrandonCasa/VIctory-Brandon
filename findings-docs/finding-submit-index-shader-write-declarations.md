# Collect declared shader writes without a second traversal of all command steps

## Location and execution path

In `crates/victory-gpu/src/host.rs:7242–7253`, `HostList::defer_gpu_write` records a `Step::GpuWrite`. During submission, `run_deferred` processes these steps in order, flushing pending refreshes and calling `shadow.writing` (around 7720–7741). Later the same submission calls `declared_writes` before `card_before` (around 5924–5934). The collector (4016–4039) reacquires each list lock and visits every step to find unique write ranges.

## Current cost

A list rich in copies, refreshes or CPU callbacks but with few shader writes still incurs another traversal of its full step stream. Even a write-free list is inspected end to end. Optional queue joining also asks for the declarations before execution (`joinable`, 3300–3314). The existing record-time set suppresses repeated writes only until a title write intervenes.

## Proposed redesign

Maintain a compact list-level summary of distinct (shadow identity, address, length) declarations in first-occurrence order over the lifetime of the recording. Publish it with the sealed list and merge summaries for the submission. Keep the ordered GPU-write steps in place for snapshot and refresh behavior. Use the summary for pre-submit join decisions as well. Cover resets, split recordings, repeated references to a list, and submissions combining several lists; retain cross-list deduplication and the shadow lifetime.

## Before and after

For T steps and U per-list unique declarations summed over the submitted list occurrences, the collector examines O(U) entries instead of O(T), using O(U) extra metadata across the recordings. Recording adds bookkeeping for declarations. Work saved is largest when U is small relative to T; a stream made mostly of unique writes may lose.

## Expected impact and confidence

Medium to high for CPU-bound submission with long, sparse-write recordings. The full extra traversal is explicit and independent of driver behavior. Measure T/U, record-time overhead, submit-lock duration and equality of declared ranges and copy-back results.
