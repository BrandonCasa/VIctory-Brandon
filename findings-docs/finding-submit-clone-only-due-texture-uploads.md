# Clone texture upload records only after their submission claim succeeds

## Location and execution path

`crates/victory-gpu/src/texture.rs:728–807,825–875` records eligible first uses in `upload_from` and implements `PreludeUpload::finish_refresh`. `crates/victory-gpu/src/host.rs:7614–7632,7745–7775` handles each `Step::Texture` in `HostList::run_deferred` before passing accepted uploads to the prelude.

## Current cost

Each recorded texture is cloned into a scratch vector before the refresh result is known. Each clone takes references to staging and the host texture, and optionally the on-demand owner. Even a clean texture incurs the vector traffic and reference-count increments and decrements.

## Proposed redesign

Store each `PreludeUpload` once in an ordered, recording-owned vector and put its vector index in `Step::Texture`. A refresh inserted by `take_coming` then cannot invalidate an index. Share the nonempty vector as a unit once the list closes. Use its entries for the existing ordered staging checks and refreshes around title writes. After the final `batch.refresh_textures()`, walk the vector outside the list mutex, call `finish_refresh` in order and clone only successful claims. This keeps descriptor and residency callbacks out of the list lock and needs neither a close-time scan nor a per-submit vector of candidates. Reset drops the recording's ownership.

## Before and after

For T recorded uses and D successful claims the ownership copies fall from T to D. All T still need a claim check. An index list consumes O(T) small integers once per recording and pays off particularly on replay or clean submits.

## Expected impact and confidence

Medium-to-lower relative priority. The unconditional cloning is explicit, while mostly dirty batches save little. Measure T/D, submission allocations and CPU time with many clean textures. This does not depend on reducing bytes in a dirty upload.
