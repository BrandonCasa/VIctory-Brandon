# Make reachable BVH pages visible on their first real GPU access

## Location and execution path

In `crates/victory-gpu/src/memory.rs::ready`, the fork maps 64 KiB title pages from candidate addresses in constants, root views and builder write targets. `Memory::packed_at` and `packed_len` can recover the full extent of a known packed header. `Memory::land` revisits it at submit. `named_in_upload` prepares the first page of candidate instance targets. Other views get one page or a bounded window. New builder destinations can get a fixed `BUILT` window.

## Current cost

`Memory::take_faults` explicitly describes an access to an absent page as reading zero or dropping its store. Later-frame feedback cannot repair a ray already evaluated or a one-time rehydration store. `Memory::place` can also fail for lack of slots at submission. This is an enabled-path failure documented in the fork. The RTX log shows zero builds and zero packed structures in the sample at line 11183.

## Proposed redesign

1. In `memory::ready`, replace a guessed window as the success condition with a plan of exact read/write spans. Start from the bound root/spill layout and allocation/mapping generations. Validate packed-header version, checked node offsets, input length and destination extent. Include the instance records, reachable bottom levels, indirect arguments and scratch. The original builder accepts both a contiguous instance array and an array of pointers. Cover the pointer array and its referenced records for the second layout. Track an existing dynamic structure by its producer's output generation. Share uploads by physical page and content version while installing a directory entry for each guest virtual page that aliases it. Guessed addresses may prefetch but cannot certify readiness.
2. For GPU-produced instance references, run a read-only discovery pass after the actual producer. Walk active references and validated structure extents, recording missing page ranges in a bounded work buffer. Upload the missing pages and resume discovery on those branches. Retain a visited set. Process overflowing work in batches until the reachable set closes. Never replay real builder atomics to learn addresses. For a genuinely data-dependent destination, execute against a private output version and publish only after all writes fit verified owner extents. A failed attempt can be discarded in full.
3. At submit reserve physical pages, descriptors and a directory snapshot. Recheck mapping generations, taking streamed bytes from the current mapping and GPU-authored bytes from the producing version. Upload payload and directory entries, transition them and establish any cross-queue fence **before** the real dispatch. Retain pages, descriptor runs and directory snapshots through the final consuming fence. Reuse retired pages under pressure without recycling an active version.
4. Return `Ready(version)`, `AwaitingBacking(ticket)` or `Invalid(reason)` to the original command. A known earlier producer that has not finished parks the consumer. A genuine invalid guest address follows the established guest access semantics. An unrepresentable working set fails its dependent generation rather than publishing an empty node or partial write.

## Before and after

A ray can currently find the first page of a structure and silently miss a leaf on the second. With full residency it runs after both are visible and returns the title's intersection. A static rehydration stage likewise writes all of its pages once.

## Expected impact and confidence

Geometry that should occlude a light or appear in a reflection becomes reachable on the first traversal. This assumes valid allocation bounds for that structure variant. After implementation, verify a ray whose target leaf page arrives after list recording hits it on its first real dispatch.
