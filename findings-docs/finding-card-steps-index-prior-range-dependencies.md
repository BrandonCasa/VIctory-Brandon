# Assign card-copy levels from overlapping byte ranges

## Location and execution path

`crates/victory-gpu/src/host.rs:5717–5732` enters `Host::card_steps` under the submission lock on a late submission. `crates/victory-gpu/src/host/card_steps.rs:77–83,654–693` checks the stage, relay, outstanding writes and blocked work before `plan`. After choosing operations, `plan` scans previously chosen operations to assign dependency levels (350–377).

## Current cost

For each copy, the planner iterates over every earlier operation, checking for conflicts between its read/write ranges and their ranges. Each fill checks the earlier copies. With many independent copies, it repeatedly establishes that unrelated addresses do not overlap. Only opt-in stage 2 admits copies. Stage 1's all-fill case already avoids these comparisons.

## Proposed redesign

Keep the linear all-fill path. Use the existing scan for zero-length steps and when the estimated pair comparisons are cheaper than building an index. For copy-heavy plans, sort and deduplicate just the endpoints of nonempty copy destinations. Each write covers whole cells between those endpoints; a read queries or updates every cell it intersects, including partially covered boundary cells. In original order, query prior writes against each read and write, and prior reads against each write. Then apply range-maximum updates to separate read and write trees. Store level plus one so an untouched cell yields level zero for the current operation. Preserve closure, placed-copy checks and refusals, including zero-length steps.

## Before and after

For T chosen operations with C real copies, current level assignment can make O(TC) comparisons. A compressed range-max implementation with range updates and queries can assign levels in O(T log T) time and O(T) space including endpoint preparation. The 12,288-step and 4 MiB intermediate limits bound applicability. The earlier dependency closure and actual GPU copies retain their costs.

## Expected impact and confidence

High conditional on stage-2 copy-heavy batches, with zero impact under the default disabled setting. The pairwise scan and lock scope are certain. Measure chosen copies, pair checks, maximum level and submission time; verify identical levels and rejection outcomes on overlapping, adjacent and independent ranges.
