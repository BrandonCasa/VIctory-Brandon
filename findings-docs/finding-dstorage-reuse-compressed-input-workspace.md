# Reuse compressed-file and intermediate buffers across streaming requests

## Location and execution path

In `crates/victory-hle/src/modules/dstorage.rs:604–640,775–798`, long-lived workers fetch and execute streaming jobs. Compressed file requests allocate a zeroed source vector and overwrite it with the file contents in `read_source` and `read_into` (1523–1586). Combined zlib plus BCPack requests also allocate a zeroed intermediate, inflate into it, truncate to the bytes produced and then decode it (1408–1424).

## Current cost

File payloads and combined-codec intermediates are both discarded at job completion. On a worker handling a stream of similarly sized requests this repeatedly allocates and initializes full buffers whose contents are replaced. The zlib decoder itself already persists per thread (1437–1443). Raw contiguous file reads go directly into their destination and memory sources are borrowed.

## Proposed redesign

Give each worker separate, lazily allocated file-input and inflate buffers. Keep their initialized lengths at the retained high-water marks, grow only the missing tails and lend slices of the exact requested lengths. Decode file input only after `read_into` succeeds and pass only the prefix `inflate` returns. Divide a total retention allowance across workers and count both buffers' capacities against each share. Use a request-local allocation for an oversized buffer so one outlier cannot displace the usual scratch or inflate idle RSS. Borrow memory inputs and keep raw reads direct. A suspended request must own any bytes it still needs before another job reuses that worker.

## Before and after

For F file requests and Z combined-codec jobs, up to F + Z transient allocations become buffer growth and oversized exceptions. Read and decode traffic is unchanged; repeated full-buffer initialization is avoidable on reuse. Memory retention is the main tradeoff because worker count follows available parallelism.

## Expected impact and confidence

Medium for asset streaming, strongest when many medium/large requests reuse a size class. High confidence in per-request allocation; allocator and OS zero-page behavior affect physical savings. Measure worker allocation bytes, retained RSS, throughput and time until requested textures are ready.
