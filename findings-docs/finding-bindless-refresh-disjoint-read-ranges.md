# Keep holes between bindless buffer reads out of shadow refreshes

## Location and execution path

`crates/victory-gpu/src/draw.rs:903–917,954–992` resolves non-table bindless buffer views in `heap_buffer_pass`, then records one enclosing span per shadow. `crates/victory-gpu/src/host.rs:8047,8274,8452,8555` also uses enclosing ranges for submission planning or freshness. `crates/victory-gpu/src/shadow.rs:3960–3995,4037–4062` performs the actual update.

## Current cost

Two small views can expose opposite ends of a large unwatched shadow. Under the default refresh interval of one frame, the copy covers the intervening bytes too. Tracking changes to descriptor slots alone would still issue this copy.

## Proposed redesign

Carry a union of read intervals per shadow through heap recording, deferred batching, write boundaries and the ahead-of-time freshness test. Merge adjoining intervals and optionally tiny gaps where a second copy costs more. Include independently declared table reads, direct root reads to the end of a buffer (`draw.rs:2002`) and whole-buffer by-address reads (`memory.rs:744–752`). A new request in an earlier hole must trigger a refresh, including when the optional multi-frame interval is enabled. Retain pending GPU-write exclusions.

## Before and after

For an unwatched shadow with enclosing width W, union width U and K runs, copied bytes fall from W to U absent other readers or pending writes. Two 64 KiB reads at opposite ends of 64 MiB would require 128 KiB instead of 64 MiB under those conditions. Tracking uses O(K) intervals and can issue K copies. Dense or independently whole-buffer reads give little benefit.

## Expected impact and confidence

High priority and high confidence in the copy mechanism. Placed shadows skip copying, and write-watched ranges already filter stale bytes. Measure enclosing and union widths after considering every reader, actual unwatched bytes copied and submission CPU time.
