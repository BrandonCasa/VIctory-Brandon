# Keep holes between bindless buffer reads out of shadow refreshes

## Problem

`crates/victory-gpu/src/draw.rs:903–917,954–992` resolves non-table bindless buffer views in `heap_buffer_pass`, then records one enclosing span per shadow. `crates/victory-gpu/src/host.rs:8047,8274,8452,8555` also uses enclosing ranges for submission planning or freshness. `crates/victory-gpu/src/shadow.rs:3960–3995,4037–4062` performs the actual update.

Two small views can expose opposite ends of a large unwatched shadow. Under the default refresh interval of one frame, the copy covers the intervening bytes too. Tracking changes to descriptor slots alone would still issue this copy.

## Change

For known unwatched, non-placed shadows, keep a single span inline and split only across large gaps. Bound the run count by coalescing small gaps or copying the enclosing span when fragmented. Carry those same runs through deferred, front and ahead-of-write planning. Union them with table, root and whole-buffer by-address reads within each existing write boundary. Refresh all runs of one shadow on one worker, excluding pending GPU writes. The ahead map must test scheduled coverage. When the multi-frame throttle encounters a newly requested hole, schedule the current union before advancing its single refresh timestamp. Keep the enclosing-span path for watched and placed shadows.

## Expected result

For an unwatched shadow with enclosing width W, union width U and K runs, copied bytes fall from W to U absent other readers or pending writes. Two 64 KiB reads at opposite ends of 64 MiB would require 128 KiB instead of 64 MiB under those conditions. Tracking uses O(K) intervals and can issue K copies. Dense or independently whole-buffer reads give little benefit.
