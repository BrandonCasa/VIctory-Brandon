# Stop the relay's fence scan at the first unfinished flight

## Problem

`send_late` and `execute_own` append runs, writes or associated events to the in-flight line in `crates/victory-gpu/src/host.rs:5412–5470,5473–5510`. The relay worker (3601–3668) identifies which prefix can land after enqueue or fence completion. The normal release configuration enables the relay unless overridden or incompatible with the selected mode (`shadow.rs:938–945`, `host.rs:2549–2564`).

Under the shared line mutex the relay materializes an optional fence for every queued item. `landable` stops at the first unfinished flight. When a nonempty prefix lands, draining the front of `Vec<Late>` also moves the entire surviving suffix. New submissions share this lock.

## Change

Store the mixed FIFO in `Line` as a `VecDeque<Late>`. Read the completed fence once and walk from the front through ready flights, writes and signals. Wait outside the lock on the first unfinished fence if it is at the head. Otherwise move just the ready prefix into the landing batch and publish `settling`, `in_hand` and the landing writers under the same lock as today. Let `card_steps::writers` accept queue iteration across a wrap. Keep the whole-queue `mem::take` for non-relay settlement.

## Expected result

For N queued items and K ready items, prefix selection examines at most min(N, K + 1) entries with O(1) scan storage. Removing K entries moves those entries into the landing batch without shifting the N − K survivors. The landing batch still needs O(K) space. The largest saving is a short ready prefix in a deep queue.
