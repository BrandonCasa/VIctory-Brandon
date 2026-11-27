# Keep the write destination stable through a physical-page fill

## Problem

The archive builder takes a pointer into the title's shared 64 KiB page table at EXE RVAs `0x4ffd520` and `0x4ffd8c0`. It can update entries before Submit. Even after submit, optional `xmem.rs::arrange` can move a section slice while its page ID and the section's owning Arc remain valid.

## Change

Read page IDs in `dstorage.rs::into_destination` after submission. Under the mapping lock, resolve the backing slices and reserve their assignments through the direct fill or buffered scatter. Make reassignment wait for the reservation to end. If reservation fails, retry with a fresh mapping before touching guest bytes. Retain post-fill list rereads and retries. Carry the successful attempt's actual backing and written extent into notification, clipping a short fill to its produced prefix.

## Expected result

A reassignment racing a fill cannot redirect bytes to another allocation. A short inflate writes and notifies only its accepted prefix even when an unused suffix page is unknown.

[Deferred optimization](deferred/finding-dstorage-resolve-physical-page-runs-once.md)
