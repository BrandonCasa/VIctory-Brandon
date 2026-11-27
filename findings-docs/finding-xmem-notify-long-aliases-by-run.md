# Notify current aliases of the physical bytes actually written

## Problem

The archive builder at EXE RVA `0x4ffd520` includes a byte offset and length with the page-list pointer. Two requests can write different intervals on the same page. Re-resolving page IDs after an `arrange` reassignment can notify aliases of different backing, while rounding to full pages invalidates unrelated GPU data.

## Change

Pass the successful writer's section, slice offsets and written extent from DirectStorage to `xmem::pages_written`. Under the alias-table lock, intersect that backing interval with its current virtual aliases, clip both ends, then unlock before notifying. For buffered scatter publish the successfully filled prefix. Preserve the existing conservative extent for a direct fill that can modify guest memory before returning an error.

## Expected result

Two writes to disjoint portions of one page notify only their corresponding intervals on aliases of the bytes actually written. Rebinding an alias before notification cannot redirect the notification to unrelated backing.

[Deferred optimization](deferred/finding-xmem-notify-long-aliases-by-run.md)
