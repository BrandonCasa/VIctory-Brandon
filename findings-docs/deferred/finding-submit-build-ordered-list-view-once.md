# Build the submission's ordered list view once

## Problem

A nonempty late submission reaches `Host::execute_parts` in `crates/victory-gpu/src/host.rs:5382–5415`. It allocates and fills an ordered vector of `&HostList` solely to call `card_steps::cut_for`, drops it, then `send_late` builds the same vector for `submit`. In `host/card_steps.rs:77–102,463–471` the split check normally returns before inspecting even the first list. Card steps and placed-order splitting are opt-in. The release relay ordinarily uses the late path (`shadow.rs:938–945`, `host.rs:2549–2564`).

The original EXE (SHA-256 prefix `e95020b2fa4a721d`) gathers list pointers into a stack array at RVA `0x2482175`, with a flush threshold of 256. At `0x24821dd` it passes that array, its current count, a frame token and flags 3 to queue slot `+0x118`. As soon as the call returns, it loops over a parallel array and makes one callback per entry (`0x24821ee–0x2482209`). A queued host submission must own its list sequence before returning to that caller. On the thin path `queue::execute_x` reaches `thin::submitted`, which acquires the recordings in order. The queue worker later obtains their back lists and detaches them in `queue/behind.rs::Job::unfold` and `jobs_of`.

An N-list submission allocates two N-entry pointer arrays and visits the order indices twice on the no-cut path. This is fixed overhead for even short submissions. Splits require fresh views for genuinely different parts.

## Change

In a scoped borrow, build the ordered references once, make the split decision and submit immediately when there is no cut. Move ownership into the flight or recursive split only after that borrow ends. Preserve duplicate indices, logical order, part offsets and the existing no-cut fallback when an attempted split cannot transfer ownership.

For large batches, address the earlier duplicate search in `list::detach` too. It scans all previously detached lists for each title list. Index successful detaches by title pointer for the duration of that call, using the index only for lookup and appending occurrences to the original order vector. In `detach_parts`, retain the existing same-entry, single-part replay rule and its handling of a repeated cut list. Keep the linear lookup for small batches. Neither the title's stack array nor pointers borrowed from it can replace the recordings or detached owners.

## Expected result

On the normal path, two allocations and 2N index-to-pointer resolutions become one allocation and N resolutions. Both versions use O(N) peak storage; the old arrays have disjoint lifetimes. GPU work and fence behavior are identical.

For N distinct successfully detached lists, the upstream search separately makes N(N−1)/2 pointer comparisons, up to 32,640 for a 256-entry title flush. An index takes expected O(N) work and O(N) temporary entries. The binary gives the capacity at this call site.
