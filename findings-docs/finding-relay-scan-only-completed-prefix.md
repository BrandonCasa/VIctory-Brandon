# Stop the relay's fence scan at the first unfinished flight

## Location and execution path

`send_late` and `execute_own` append runs, writes or associated events to the in-flight line in `crates/victory-gpu/src/host.rs:5412–5470,5473–5510`. The relay worker (3601–3668) identifies which prefix can land after enqueue or fence completion. The normal release configuration enables the relay unless overridden or incompatible with the selected mode (`shadow.rs:938–945`, `host.rs:2549–2564`).

## Current cost

Under the shared line mutex, the worker builds a vector containing one optional fence for *every* queued item. Only then does `landable` (2708–2710) stop at the first incomplete flight. If the first item is unfinished, no other entry influenced the decision. The same suffix can be copied again after a wake. Submitters share the mutex.

## Proposed redesign

Read the completed fence once and inspect the queue in place. Return either the length of the landable prefix or, when that length is zero, the first flight's fence to wait for. Count preceding writes and signals in order even when there is no completed flight. Keep the current drain, in-hand write publication, landing and failure handling, and release the lock before waiting.

## Before and after

For N queued items and K landable items, identifying the prefix costs O(N) time and space today. It needs O(min(N, K + 1)) time and O(1) extra space. Draining the K items still has its original cost. The list limit defaults to 256 lists, but writes and signals are additional line entries.

## Expected impact and confidence

Medium for backlogged submissions, especially when many wakeups find an unfinished head. The discarded suffix traversal is certain, while benefit for a queue of one item is small. Measure queue depth, prefix length and line-lock hold/wait times.
