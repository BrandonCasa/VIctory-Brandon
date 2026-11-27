# Notify current aliases of the physical bytes actually written

## Problem

The archive builder at EXE RVA `0x4ffd520` includes a byte offset and length with the page-list pointer. Two requests can write different intervals on the same page. Re-resolving page IDs after an `arrange` reassignment can notify aliases of different backing, while rounding to full pages invalidates unrelated GPU data.

## Change

Pass the writer's retained section, slice offsets and written extent from DirectStorage to `xmem::pages_written`. Acquire the shared title-mapping guard across alias lookup and GPU notification so a remap cannot reuse a selected virtual address between them. Under `SHOWN`, intersect the written intervals with the current aliases and clip both ends. Release `SHOWN` before calling the GPU. On success publish the produced prefix for either fill path. On direct-fill failure preserve the conservative extent because the guest bytes may already have changed.

## Expected result

Two writes to disjoint portions of one page notify only their corresponding intervals on aliases of the bytes actually written. Rebinding an alias before notification cannot redirect the notification to unrelated backing.

[Deferred optimization](deferred/finding-xmem-notify-long-aliases-by-run.md)
