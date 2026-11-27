# Reject textures without a cacheable mip prefix before copying their payload

## Location and execution path

`Staging::keep_whole` in `crates/victory-gpu/src/texture.rs:1466–1485` prepares footprints for the optional disk texture store. `texstore::keep` in `crates/victory-gpu/src/texstore.rs:174–185` hashes the complete tile data and, for unseen content, clones the layout and bytes into an unbounded channel. The worker calls `kept` (197–206), where `key_levels` (159–168) can produce no usable key.

## Current cost

A single-mip texture can never produce a smaller-mip lookup key. Nor can a texture whose full size is at most the 32 KiB minimum prefix, because the prefix must also be strictly smaller than the full texture. These cases pass the producer's full-texture checks. On first sight the producer copies all B bytes for a job whose worker immediately returns without writing an index entry or a blob. There are also larger layouts with no qualifying split.

## Proposed redesign

After the store and whole-texture gates, reject a single mip or a size at most `MIN_PREFIX` before building the parts vector. For other layouts, use an iterator shared with `key_levels` to stop at the first qualifying prefix and remember that eligibility only if the staging object is reused. Keep full key enumeration and prefix hashing on the worker. For eligible content, build job parts and clone bytes only after the digest wins the existing seen check. Keyless layouts then leave the seen set available for eligible shapes with identical bytes.

## Before and after

For an ineligible unseen payload, O(B) producer hashing, O(B) copying, O(B) queued storage and a worker wakeup become a metadata check. For repeated ineligible content the existing seen check already avoids the copy, but still hashes. Eligible content retains the existing transfer and persistence costs.

## Expected impact and confidence

High for memory traffic if store-enabled streaming contains many ineligible uploads, zero with the store disabled. Confidence is high in the eligibility result and unknown in the workload mix. Count rejected payload bytes, channel high-water memory and refresh time with a representative asset stream.
