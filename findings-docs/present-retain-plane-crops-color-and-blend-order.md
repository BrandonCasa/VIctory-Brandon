# Keep first-use draws in the frame that requested them

## Location and execution path

In `crates/victory-gpu/src/pipeline/make.rs::ready`, first use queues the pipeline, waits within the frame budget, then returns `p.made.get()?.as_ref()`. This collapses an unfinished compile and a completed refusal into the same result. In `draw.rs`, `State::prepare_for`, `dispatch` and `execute_indirect` can consequently return without recording the D3D12 command. The worker has no link back to that command.

## Current cost

The cutscene log starts with zero previously used keys loaded early (line 82), 13 ordinary workers (175) and a 500 ms frame budget (199). The scheduler also provides two screen workers and a default 10 s screen budget. Two windows show 1,075 and 1,268 pipelines pending, with 1,555.1 and 4,280.2 command drops per frame attributed to pipelines being made (7370–7424, 8447–8503). A screen PSO succeeded after a 1,145 ms wait (19127). A one-time lighting bake dropped this way can affect every subsequent frame.

## Proposed redesign

1. Give `Pending` a single compile owner shared by workers, `start` and `now`. Expose `Ready(host)`, `Pending(ticket)` or `Failed(stage, HRESULT/reason)`. Publish worker failures too. Register a waiter and recheck completion atomically. Release locks before compiling or resuming.
2. Retain the ordered command stream for each list generation. Reuse `list/thin.rs::Front` and `Recording` for ordinary/direct entry points too. A direct list keeps its translated prefix, open `HostList`, pending host steps and `draw::State`. At the first pending command, park **before** binding, modifying indirect arguments or publishing any write. Keep accepting the suffix and scan its pipeline setters/consumers to schedule later PSOs concurrently. Copy immediate arguments and retain COM references. Preserve the original recording-time versus execution-time descriptor semantics. A reset detaches the old generation if already submitted.
3. Change the wait budget to bound inline waiting only. Resume the parked cursor exactly once with its original state. An unsubmitted list can be abandoned. A terminal refusal completes the dependency with its real failure, allowing title-defined recovery or a failed frame. Never silently skip a required producer and present its consumers.

   ```text
   Ready    -> issue original opcode, advance cursor
   Pending  -> subscribe this generation and cursor, yield worker
   Failed   -> resolve dependent submission as failed
   ```

4. In `queue/behind.rs::Lane::step`, make an unfinished list and its producer fences hard dependencies. Neither relaxed present ordering nor forcing an apparent queue cycle may bypass them. Run independent queue heads. Deliver fences, readbacks and the back-buffer present only after the original submission reaches them. Retain allocations and descriptor versions through retirement.
5. Precompile ahead of the title's watchdog. Save each **demanded recipe** with its used key through `pipeline::known`, even when recording parks. Today the complete book is opt-in and `apps/victory/src/warm.rs::select_pipelines` can warm only known keys in the used set. A warm process releases its objects into the driver's disk cache. On the live device, retain prewarmed host PSOs in a bounded cache keyed by the canonical recipe. Make `hand_out` reuse that entry or its in-flight `Pending`; it currently creates a fresh `Arc<Pending>` for each call. Track successful creation separately from processed refusals and invalidate by translator/binding ABI, adapter and driver. On a fresh install, schedule exact recipes as the title supplies them during loading. Give first-use demand priority. An archive shader alone supplies no graphics state.

## Before and after

A valid PSO completing just after the budget currently leaves a hole. The retained list instead submits its producer, dependent pass and original present in order. Cold work can delay the frame. Bound queued bytes/list generations at submit. The historical 1,200-PSO burst exceeded the title's roughly 30 s watchdog. Preserve the watchdog, title clock and real fence values. If an unknown PSO first becomes available inside a timed frame and its driver invocation alone exceeds the deadline, there is no faithful guarantee of completing that frame on time.

## Expected impact and confidence

First-use geometry and one-time bakes appear in the frame that issued them. The uncertainty is how many expensive recipes are available during loading. After implementation, hold one valid PSO past its inline budget and verify its producer, cross-queue consumer and original present execute in that order.
