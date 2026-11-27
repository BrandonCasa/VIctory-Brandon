# Materialize the exact title bytes for an unaligned constant buffer

## Location and execution path

At 06:56:07 UTC the log reports a CBV at title address `0xa3683f0`, aligned to 16 bytes. `descriptor::create_cbv` stores its address and size. In `crates/victory-gpu/src/descriptor/host.rs:4314-4347`, `make_twinned` translates the address onto a shadow and returns `None` if `gpu % 256 != 0`. The tested address is the host VA.

Inline root constants go directly to `Set*Root32BitConstants`. Root CBVs already use `draw::root_view` and `fill_scratch` for an unaligned address. Known table CBVs get aligned `scratch_held` storage in `State::tables`. A shader selecting a CBV by computed heap index can bypass both. `HostHeap::note_buffer` discovers buffer-view strides, which a `View::Constant` does not have.

## Current cost

A valid indexed CBV can read zero. Rounding its start down shifts every register. The optional raw-CBV path also depends on the consuming shader and host agreeing on a raw twin. The logged host has no twin region.

## Proposed redesign

1. Give root, table and indexed CBV consumers a common resolved binding containing the title VA, title-visible range, source allocation and content version, and required DXIL access form. Bind the original host address when aligned with sufficient backing. Use a raw view only where the emitted loads implement the same guest addressing, conversion and bounds. Otherwise allocate a 256-byte-aligned buffer, copy the title bytes from the exact start to offset zero and round only the host allocation/descriptor length. Keep padding inaccessible to guest loads. Where native CBV bounds or size (64 KiB) differ from an established title access, lower that access against the source range with the title's bounds. A root CBV supplies no size. Use the actual resource extent and consuming loads to establish equivalence, rather than silently discarding an accessible register.
2. Register indexed CBVs in the logical heap alongside `mirror` and copied records. Resolve the candidate bindings at the consuming submission for opaque indices. Source each copy from the authoritative CPU mapping or preceding GPU writer. Preserve the title's visibility point, including fresh tables, list resubmission and intervening writes. Keep a record-keyed table cache entry only while the source version also agrees.
3. Suppose draw A reads slot S, an intervening dispatch writes its constants, then draw B reads S. Record A with descriptor P0, aligned bytes B0 and heap-map root M0 resolving S to P0. Reserve B1/P1 and M1 for the second read. After the writer, transition its output to copy source, copy the exact range to a default-heap B1, transition B1 to constant-buffer read, and bind M1 before B. For explicit tables or root CBVs, bind the new table base or native GPU VA. Use `HostList::defer_constants`, `host::card_steps` and `descriptor::constant_slot` with a versioned destination. If the current prelude can only run before A, split the host list at the intervening producer and replay state for the continuation. Old bindings live through A's fence. A temporarily unavailable source or allocation remains a dependency. An access whose original bounds cannot be represented remains unsupported.

## Before and after

Register zero now begins at the title's first byte, even when its native CBV starts on a different host boundary. Two draws sharing a title slot can each see the constants that precede their own use.

## Expected impact and confidence

High confidence in the indexed gap. The warning does not identify its consumer. After implementation, compare the first and last used registers for the logged record across an intervening GPU write. A read already served by a correct table snapshot would rule out this explanation for that draw.
