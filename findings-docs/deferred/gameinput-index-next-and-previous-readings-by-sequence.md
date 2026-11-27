# Index relative input readings by their existing sequence numbers

## Problem

In `crates/victory-hle/src/modules/gameinput.rs:592–621,660–715`, the input hub polls every 8 ms plus processing time. Each changed value increments its slot's sequence exactly once and appends one reading to a deque capped at 256 entries. `next_reading` and `previous_reading` delegate to `relative` (845–908) and return an owned reference through `hand` (784–794).

Forward lookup starts at the oldest reading and skips every entry up to the reference, even to report that nothing follows the newest reading. Walking a backlog one reading at a time repeatedly scans the same prefix while holding the hub's state mutex. Each device history has a single input kind (389–395, 442–447, 677–684).

Ghidra shows the keyboard loop in `game_scarlett_master.exe` (SHA-256 prefix `e95020b2fa4a721d`) calling GameInput slot `+0x20` at RVA `0x43a1d0e` for the **current** reading, then slot `+0x28` at `0x43a1e6b` with that reading, the same device and keyboard mask `0x10`. It repeats only if a later reading arrives. With no intervening input update, today's forward scan examines the entire retained history to return no reading. The pad wrapper at `0x2211c4c` instead calls slot `+0x30` for the previous reading with a concrete device and mask `0x40000`.

## Change

Under the existing hub lock, validate the reference device and oldest retained sequence first. A forward request at the newest sequence can immediately return `hand(out, None)`. For an actual neighbor, calculate its position from the oldest sequence with checked arithmetic and index the deque directly. Check its kind and return through `hand`. Preserve the old-reference error and its precedence over a kind mismatch, null output handling and COM ownership. Look up the current history on every call so an 8 ms poll between current and next remains visible.

## Expected result

For H retained readings and K successive forward calls, worst-case work falls from O(KH) to O(K); storage is unchanged. H is bounded at 256. A previous lookup immediately behind the newest reading already examines only one or two elements.
