# Skip group synchronization in empty indirect clamp groups

## Problem

Ghidra shows that `game_scarlett_master.exe` (SHA-256 prefix `e95020b2fa4a721d`) initializes one-argument command signatures at RVA `0x2479466`. The draw, dispatch and mesh callers at RVAs `0x248d198`, `0x248cbba` and `0x4e33f91` each call list slot `+0x1e8` with a maximum of one record, a resource-relative byte offset, a null count buffer and a zero count offset.

`list::execute_indirect` forwards those arguments to `draw::State::execute_indirect`. After the pipeline and argument-buffer checks, `State::batched` can reuse a table or open one. `indirect::open_batch` records the four-group dispatch before it returns the empty table to its caller. Later calls add jobs or increment a matching job's reuse count in `indirect/batch.rs::Table::place`. The final number of jobs can therefore exceed one even though each title call names one record.

The count is filled through the job table. Entirely empty groups still initialize shared state, write 64 zero costs and reach two group barriers. Their eventual path has no record, budget or diagnostic update.

## Change

Read the final count from the existing upload table when the clamp shader runs. Return the whole group when `SV_GroupID.x * 64 >= count`, before touching shared state. Keep inactive lanes in the last partially populated group participating in both barriers. Keep the four-group recording, source offsets, per-job reuse charges, resource barriers and active-group budget atomics. The title's maximum of one cannot size the host dispatch at recording time.

## Expected result

For J jobs (up to 256), four groups still launch, but only ceil(J/64) groups execute the shared-memory and barrier body. There is a small uniform test per group. Full batches save no barriers.
