# Reuse compressed-file input buffers across streaming requests

## Problem

In `crates/victory-hle/src/modules/dstorage.rs:604–640,775–798`, long-lived workers fetch and execute streaming jobs. Compressed file requests allocate a zeroed source vector and overwrite it with the file contents in `read_source` and `read_into` (1523–1586). Combined zlib plus BCPack requests also allocate a zeroed intermediate, inflate into it, truncate to the bytes produced and then decode it (1408–1424).

File payloads and combined-codec intermediates are both discarded at job completion. On a worker handling a stream of similarly sized requests this repeatedly allocates and initializes full buffers whose contents are replaced. The zlib decoder itself already persists per thread (1437–1443). Raw contiguous file reads go directly into their destination and memory sources are borrowed.

## Change

Give each worker a lazy compressed file-input buffer. Keep its initialized high-water length, lend only source_size bytes and decode only after read_into succeeds. Cap retained capacity per worker and use temporary storage for oversized requests. Keep memory inputs borrowed and raw file reads direct. The two inspected builders issue raw, zlib or BCPack requests separately, so leave the combined intermediate request-local. Preserve any bytes a suspended job still needs before the worker reuses the buffer.

## Expected result

For F qualifying file requests, up to F transient allocations become buffer growth and oversized exceptions. Combined-codec intermediate allocations remain on callers that use that API path. Read and decode traffic is unchanged; repeated full-buffer initialization is avoidable on reuse. Memory retention is the main tradeoff because worker count follows available parallelism.

[Related correctness finding](../dstorage-decode-zlib-archive-swizzle-modes.md)
