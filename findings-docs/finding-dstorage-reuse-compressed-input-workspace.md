# Reuse compressed-file and intermediate buffers across streaming requests

## Location and execution path

In `crates/victory-hle/src/modules/dstorage.rs:604–640,775–798`, long-lived workers fetch and execute streaming jobs. Compressed file requests allocate a zeroed source vector and overwrite it with the file contents in `read_source` and `read_into` (1523–1586). Combined zlib plus BCPack requests also allocate a zeroed intermediate, inflate into it, truncate to the bytes produced and then decode it (1408–1424).

## Current cost

File payloads and combined-codec intermediates are both discarded at job completion. On a worker handling a stream of similarly sized requests this repeatedly allocates and initializes full buffers whose contents are replaced. The zlib decoder itself already persists per thread (1437–1443). Raw contiguous file reads go directly into their destination and memory sources are borrowed.

## Proposed redesign

Give each worker bounded reusable source and intermediate scratch, borrowing separate live slices while both stages need them. Reuse initialized capacity without clearing the portion guaranteed to be overwritten. Expose the source only after an exact successful read and pass only the initialized byte count returned by inflate. Preserve retry offsets, short output, errors, and the input-borrowing and direct-to-destination paths. Discard oversized allocations or enforce a shared retained-byte budget across workers.

## Before and after

For F file requests and Z combined-codec jobs, up to F + Z transient allocations become buffer growth and oversized exceptions. Read and decode traffic is unchanged; repeated full-buffer initialization is avoidable on reuse. Memory retention is the main tradeoff because worker count follows available parallelism.

## Expected impact and confidence

Medium for asset streaming, strongest when many medium/large requests reuse a size class. High confidence in per-request allocation; allocator and OS zero-page behavior affect physical savings. Measure worker allocation bytes, retained RSS, throughput and time until requested textures are ready.
