# Clone texture upload records only after their submission claim succeeds

## Location and execution path

`crates/victory-gpu/src/texture.rs:728–807,825–875` records eligible first uses in `upload_from` and implements `PreludeUpload::finish_refresh`. `crates/victory-gpu/src/host.rs:7614–7632,7745–7775` handles each `Step::Texture` in `HostList::run_deferred` before passing accepted uploads to the prelude.

## Current cost

Each recorded texture is cloned into a scratch vector before the refresh result is known. Each clone takes references to staging and the host texture, and optionally the on-demand owner. Even a clean texture incurs the vector traffic and reference-count increments and decrements.

## Proposed redesign

Keep an immutable list of texture-step indices with the sealed recording. After `batch.refresh_textures()` borrow those records in original order, claim each with `finish_refresh` and clone only successful claims for the prelude. Tie the index view to the recording generation and rebuild on reset. Keep locks released during provider refresh and prelude execution. Continue to register due staging and honor intervening title writes in their existing order.

## Before and after

For T recorded uses and D successful claims the ownership copies fall from T to D. All T still need a claim check. An index list consumes O(T) small integers once per recording and pays off particularly on replay or clean submits.

## Expected impact and confidence

Medium-to-lower relative priority. The unconditional cloning is explicit, while mostly dirty batches save little. Measure T/D, submission allocations and CPU time with many clean textures. This does not depend on reducing bytes in a dirty upload.
