# Reuse unchanged input-assembler bindings on consecutive draws

## Problem

In `crates/victory-gpu/src/draw.rs:1440–1505`, `State::prepare_for` resolves the pipeline's vertex inputs, schedules shadow refreshes and calls `IASetVertexBuffers`. Indexed draws also set the index buffer. Ordinary and indexed draws reach this through `State::draw` and `State::draw_indexed` (3745, 3853). The pipeline state itself already has a host-side equality check in `bind` (around 1616).

Two adjacent indexed draws with the same resolved streams and index buffer still emit both IA commands into the host command list. Reusing a mesh for several draws or changing only root arguments is sufficient to trigger this. The code rebuilds the view array from guest state each time.

## Change

Keep the last emitted vertex views and index view on the recording. Continue resolving and registering every shadow refresh, then compare the supplied vertex prefix by address, size and stride, including zero views. Skip an equal prefix; on a mismatch issue one call for the prefix and update the cache, preserving slots outside it. This caps the changing-stream path at its current one vertex call. Handle the index view independently, including its format and unresolved-view failure. Track or invalidate only the slots overwritten by rectangle expansion, even if expansion later fails. Mark the cache unknown on list reset or replay into a fresh list.

## Expected result

For D eligible draws, V emitted changed vertex-binding ranges and I index-binding changes, up to 2D IA calls become V + I calls. Consecutive equal draws emit zero IA calls after the first. Comparison and shadow refresh still run. Tracking costs at most 32 vertex views and one index view per recording. Constantly changing streams can make comparisons a net cost.
