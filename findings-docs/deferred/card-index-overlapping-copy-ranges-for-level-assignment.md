# Assign card-copy levels from overlapping byte ranges

## Problem

`crates/victory-gpu/src/host.rs:5717–5732` enters `Host::card_steps` under the submission lock on a late submission. `crates/victory-gpu/src/host/card_steps.rs:77–83,654–693` checks the stage, relay, outstanding writes and blocked work before `plan`. After choosing operations, `plan` scans previously chosen operations to assign dependency levels (350–377).

For each copy, the planner iterates over every earlier operation, checking for conflicts between its read/write ranges and their ranges. Each fill checks the earlier copies. With many independent copies, it repeatedly establishes that unrelated addresses do not overlap. Only opt-in stage 2 admits copies. Stage 1's all-fill case already avoids these comparisons.

Only three of 348 stage-2 cutscene reports had a positive completed-copy average, peaking at 1.33 per frame. Fill-only plans already perform zero pair checks.

## Change

Split `plan` at the placed-copy conflict check. `cut_for` only consumes `BEHIND_PLACED` and the offending pair. Run the same dependency closure, early refusals and ordered conflict search, then return from the probe. The real plan still checks CPU readers and assigns levels under the submission lock. Keep the all-fill path and use the pairwise scan for small plans or zero-length steps.

For copy-heavy plans, partition the address space at nonempty copy-destination endpoints. Keep separate read and write range maxima, initially zero. In command order, query prior writes over each read and write, and prior reads over each write. The largest result is the new level. For each touched interval, raise its stored value to the maximum of its old value and the new level plus one. A later independent read can have a lower level than an earlier read of the same cell. Partial reads mark every cell they intersect because every indexed write covers whole cells.

## Expected result

A no-cut probe that reaches the placed-copy check can omit the remaining validation and scheduling. A real plan of T chosen operations with C nonempty copies can still need O(TC) pair checks. At most 2C endpoints bound the index. Sorting them and using lazy range-maximum updates gives O(T log(C + 1)) level assignment and O(C) extra index space beyond the existing output. Dependency closure and the separate CPU-reader checks keep their current costs.
