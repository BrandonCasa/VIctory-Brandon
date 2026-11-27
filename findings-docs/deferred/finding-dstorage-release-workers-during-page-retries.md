# Release storage workers during post-submit page retries

## Problem

`crates/victory-hle/src/modules/dstorage.rs:775–800` runs `execute` synchronously on each worker until `Lanes::finish`. On page destinations, `page_list` (1717–1734) sleeps and rereads an unknown list every millisecond for up to 1.5 seconds. `into_pages` (1645–1680) can independently back off on failed physical writes.

A sleeping request keeps its worker. Several unavailable destinations can occupy the worker pool while other queues hold runnable requests. The write retry already owns decoded bytes, so rerunning the entire read or decode on each wake would add work.

The November 27 run recorded no page-list wait or scatter retry. The benefit applies during post-submit stalls.

## Change

Keep the ready path synchronous. When a post-submit page-list poll or scatter retry would sleep, suspend the job until its next poll or backoff. Page-list polling keeps its original deadline. A failed scatter retries the same page IDs and filled prefix without repeating I/O or decode. Keep the job in `lane.running` through final notification so status publication and queue closure wait for it. A due retry can resume on an idle worker using its original staging reservation, including when that reservation fills the budget. Bound suspended buffer capacity and job count, including zero-charge jobs, and use the current in-worker retry at the limit.

## Expected result

For W workers and S simultaneous sleepers, available execution capacity changes from max(0,W−S) to W while eligible requests exist, subject to staging and memory limits. Polls/retries may still occur. Pending states need scheduling metadata and ownership of any buffered payload.

[Related correctness finding](../finding-dstorage-release-workers-during-page-retries.md)
