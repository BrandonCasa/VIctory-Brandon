# Skip ordinary per-vertex shadow preparation for CPU-expanded rectangles

## Location and execution path

`crates/victory-gpu/src/draw.rs:3745–3767` prepares the normal vertex streams before deciding whether a direct rectangle draw needs CPU expansion. `prepare_for` (1465–1491) resolves and refreshes each ordinary input shadow and binds a vertex view. `rectangles` (3793–3849) queues a separate read of the used guest vertex range, expands it into list scratch and replaces the per-vertex IA bindings before the draw. `pipeline/host.rs:705–716,778–785` shows when geometry-stage expansion succeeds or falls back.

## Current cost

For each stream replaced on this path, the normal IA view has no draw consumer. Its full declared range can also cause a deferred shadow refresh in addition to the narrower guest read for expansion. `HostList::defer_refresh` already deduplicates exact ranges (`host.rs:7222–7235`), so redundant transfer bytes depend on other uses in the list.

## Proposed redesign

Have the pipeline own the expansion layout. When `GraphicsState::elements` resolves appended offsets, include instance elements in offset progression and record per-vertex slots in a mask. Group their `(offset, format)` pairs in declaration order once for CPU expansion, using shared ownership so deferred reads outlive draw recording. The indirect check can use the same mask. For direct CPU rectangles, prepare one final IA array from the resolved pipeline. Expand each marked slot, including mixed slots, and prepare instance-only slots normally. Preserve bounds and shadow checks, scratch lifetime and submission-time guest reads. Keep the layout available when rectangle topology is selected dynamically.

Make the deferred CPU read's dependency explicit before dropping its ordinary refresh. `blocked_by` skips `Step::Host`, `stale_under_flights` examines only refreshes, and `card_steps::does_of` marks a ranged host read as unblocked. At submission, check the exact bytes the expansion will read against outstanding flight writes in both the wait decision and the card-step planner. An overlap must wait for those bytes to land even if unrelated fills can run on the GPU. Retain the existing placed-copy reader check.

## Before and after

Today `rectangles` filters E input elements for each of 32 slots and builds a vector for each replaced stream on every fallback draw. Grouping once moves this layout work to the pipeline and allows each deferred read to share its slot's elements. For R replaced streams, the draw also avoids R ordinary shadow lookups and refresh registrations. Avoided transfer bytes depend on dirty ranges and other list uses. Scratch and expansion still scale with the vertices drawn.

## Expected impact and confidence

Measure fallback draws, otherwise unique refresh bytes and time spent waiting for overlapping writers. Check expanded vertex contents when a prior flight writes the source, and check that a disjoint declared write adds no wait for that read. Successful geometry-stage draws see no saving.
