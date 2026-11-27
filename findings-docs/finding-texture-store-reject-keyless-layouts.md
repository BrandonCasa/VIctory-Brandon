# Reject textures without a cacheable mip prefix before copying their payload

## Location and execution path

`Staging::keep_whole` in `crates/victory-gpu/src/texture.rs:1466–1485` prepares footprints for the optional disk texture store. `texstore::keep` in `crates/victory-gpu/src/texstore.rs:174–185` hashes the complete tile data and, for unseen content, clones the layout and bytes into an unbounded channel. The worker calls `kept` (197–206), where `key_levels` (159–168) can produce no usable key.

## Current cost

A single-mip texture can never produce a smaller-mip lookup key. Nor can a texture whose full size is at most the 32 KiB minimum prefix, because the prefix must also be strictly smaller than the full texture. These cases pass the producer's full-texture checks. On first sight the producer copies all B bytes for a job whose worker immediately returns without writing an index entry or a blob. There are also larger layouts with no qualifying split.

## Proposed redesign

After the store and whole-texture gates, reject single-mip or too-small layouts immediately. For the remaining shapes, share the existing eligibility rules between the producer, worker and lookup. `Layout::bytes_from(k)` scans the mip suffix for its largest byte extent. Calling it from `read_from` and again for the length at each candidate can make a full eligibility check quadratic in mip count. Factor the extent calculation into the layout code and visit mips from smallest to largest with a running maximum. Apply the same whole-layout, tail, level and prefix-size checks to that value.

The producer stops at its first eligible level, then builds parts and clones bytes only after the digest passes the seen check. The worker enumerates eligible keys in one metadata pass. For `stored_levels`, keep the requested level's existing tail clamp and check just that suffix. Keyless uploads never reserve a blob identity.

## Before and after

For an ineligible unseen payload of B bytes and M mips, the producer avoids the store's O(B) hash, payload copy and queued storage. The metadata check takes at most O(M) time and constant scratch. Staging still performs any hash it needs for change detection. Eligible content retains the existing transfer and persistence costs.

## Expected impact and confidence

High for memory traffic if store-enabled streaming contains many ineligible uploads, zero with the store disabled. Confidence is high in the eligibility result and unknown in the workload mix. Count rejected payload bytes, channel high-water memory and refresh time with a representative asset stream.
