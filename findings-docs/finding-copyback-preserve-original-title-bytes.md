# Keep the original title bytes through the CPU shadow merge

## Location and execution path

In `crates/victory-gpu/src/shadow.rs:3383–3438`, `Shadow::copy_back_with` handles changed pieces for host-memory shadows. `back_piece` (1905–1963) narrows each piece to its changed interval, reads title bytes, merges them with the GPU result and snapshot, overwrites its temporary title buffer, and saves an XOR mask. The caller rebuilds the original bytes with a fresh allocation and passes both versions to `put_changed` (2307 onward).

## Current cost

The merge already has the original title data and writes the merged bytes into the host shadow. Yet it builds byte runs from the mask, uses those runs only to test whether any exist, and traverses the mask again to reconstruct the just-read title data. Every surviving changed interval pays these passes and an allocation. An earlier difference flag already identifies the nonempty case.

## Proposed redesign

Leave the read scratch unchanged. While merging, write the result into the host interval as today and accumulate a boolean indicating any change from the title. Pass the original scratch and merged host interval directly to `put_changed`; if reading that host mapping a second time is expensive, use reusable, cacheable merged scratch. Preserve host updates even when no guest write is needed, the first/last-difference trimming and the title-wins rule for concurrent changes. Keep the exact dword writes, safecopy retry behavior and optional whole-write behavior in the existing writer.

## Before and after

For N bytes across J merged intervals requiring a write, this removes the XOR-mask construction, byte-run discovery, O(N) reconstruction and J temporary original-byte allocations. The merge and the final write comparison remain linear. A card-resident shadow exits before this logic, and the relay has a separate merge implementation.

## Expected impact and confidence

High within CPU copy-back workloads, lower in a relay-heavy configuration. High confidence in the unused run detail and inverse transformation. Measure changed interval bytes, copy-back CPU time and the memory characteristics of the retained host view. Verify title/GPU conflicting writes, unchanged pieces and page-remapping retries.
