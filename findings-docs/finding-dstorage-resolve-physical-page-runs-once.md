# Keep the write destination stable through a physical-page fill

## Problem

The archive builder takes a pointer into the title's shared 64 KiB page table at EXE RVAs `0x4ffd520` and `0x4ffd8c0`. It can update entries before Submit. Even after submit, optional `xmem.rs::arrange` can move a section slice while its page ID and the section's owning Arc remain valid.

## Change

Accept one page-list snapshot after submission. The title holds its page references until the status entry completes. Retain the bounded wait for initially unknown pages. Once a fill succeeds, complete that request exactly once. Replaying it onto a changed list can overwrite a second allocation.

Reserve each resolved slice under the page-table lock before allowing a direct fill. Release the lock before I/O. Have `arrange` skip reserved slices, and mark the slices committed by either `Section::window` or the fallback `View::map` before releasing the reservation. The fallback currently leaves the freshness bit clear after a write. If the full direct window is unavailable, fill private scratch, then reserve and map only the runs covering the produced prefix before its first store. A host mapping failure can then retry with the decoded bytes intact. Pass the retained backing and written extent into notification.

## Expected result

A reassignment racing a fill cannot redirect bytes to another allocation. A short inflate writes and notifies only its accepted prefix even when an unused suffix page is unknown.

[Deferred optimization](deferred/finding-dstorage-resolve-physical-page-runs-once.md)
