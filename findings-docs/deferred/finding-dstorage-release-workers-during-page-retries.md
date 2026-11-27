# Release storage workers during post-submit page retries

## Problem

`crates/victory-hle/src/modules/dstorage.rs:775–800` runs `execute` synchronously on each worker until `Lanes::finish`. On page destinations, `page_list` (1717–1734) sleeps and rereads an unknown list every millisecond for up to 1.5 seconds. `into_pages` (1645–1680) can independently back off on failed physical writes.

A sleeping request keeps its worker. Several unavailable destinations can occupy the worker pool while other queues hold runnable requests. The write retry already owns decoded bytes, so rerunning the entire read or decode on each wake would add work.

The November 27 run recorded no page-list wait or scatter retry. The benefit applies during post-submit stalls.

## Change

Keep the ready path synchronous. When a post-submit page-list poll or scatter retry would sleep, suspend the job until its next poll or backoff. Page-list polling rereads the list until the original deadline. A failed scatter retries the same page IDs and filled prefix without repeating I/O or decode. Suspended jobs own their buffers and retain the staging charge and sequence through final notification. Bound their count and allocation capacity, including zero-staging requests, and use the current in-worker retry when the limit is reached.

## Expected result

For W workers and S simultaneous sleepers, available execution capacity changes from max(0,W−S) to W while eligible requests exist, subject to staging and memory limits. Polls/retries may still occur. Pending states need scheduling metadata and ownership of any buffered payload.

[Related correctness finding](../finding-dstorage-release-workers-during-page-retries.md)
