# Keep the original title bytes through the CPU shadow merge

## Problem

In `crates/victory-gpu/src/shadow.rs:3383–3438`, `Shadow::copy_back_with` handles changed pieces for host-memory shadows. `back_piece` (1905–1963) narrows each piece to its changed interval, reads title bytes, merges them with the GPU result and snapshot, overwrites its temporary title buffer, and saves an XOR mask. The caller rebuilds the original bytes with a fresh allocation and passes both versions to `put_changed` (2307 onward).

The merge already has the original title data and writes the merged bytes into the host shadow. Yet it builds byte runs from the mask, uses those runs only to test whether any exist, and traverses the mask again to reconstruct the just-read title data. Every surviving changed interval pays these passes and an allocation. An earlier difference flag already identifies the nonempty case.

## Change

Keep the title bytes from the trimmed read in reusable scratch. After a successful read, merge into the host interval with the existing branchless title-wins rule and OR-reduce `merged ^ original` in the same pass. Always update the host, even when the reduction is zero. For a nonzero result, pass the original scratch and merged interval directly to `put_changed` on ordinary write-back system memory. For a `Shadow::mapped` GPU-upload window, also emit the merged bytes into reusable CPU scratch during the merge so the writer and its retries never reread the mapping across the bus. Reuse scratch across pieces and leave dword selection, retries and whole-write mode to the existing writer.

## Expected result

For N bytes across J merged intervals requiring a write, this removes the XOR-mask construction, byte-run discovery, O(N) reconstruction and J temporary original-byte allocations. The merge and the final write comparison remain linear. A card-resident shadow exits before this logic, and the relay has a separate merge implementation.
