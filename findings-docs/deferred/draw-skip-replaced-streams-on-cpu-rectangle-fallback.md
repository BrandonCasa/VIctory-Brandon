# Skip ordinary per-vertex shadow preparation for CPU-expanded rectangles

## Problem

`crates/victory-gpu/src/draw.rs:3745–3767` prepares the normal vertex streams before deciding whether a direct rectangle draw needs CPU expansion. `prepare_for` (1465–1491) resolves and refreshes each ordinary input shadow and binds a vertex view. `rectangles` (3793–3849) queues a separate read of the used guest vertex range, expands it into list scratch and replaces the per-vertex IA bindings before the draw. `pipeline/host.rs:705–716,778–785` shows when geometry-stage expansion succeeds or falls back.

For each stream replaced on this path, the normal IA view has no draw consumer. Its full declared range can also cause a deferred shadow refresh in addition to the narrower guest read for expansion. `HostList::defer_refresh` already deduplicates exact ranges (`host.rs:7222–7235`), so redundant transfer bytes depend on other uses in the list.

## Change

Precompute eligible per-vertex slot masks and the ordered (offset, format) list on the pipeline. Preserve appended-offset progression across instance elements. Apply the paired correctness finding’s eligibility conditions before skipping any preparation. On eligible direct CPU rectangles, expand the replaced slots directly into scratch and prepare instance-only slots normally. Share the layout with deferred readers and retain their dependency on the exact guest bytes until submission.

## Expected result

Today `rectangles` filters E input elements for each of 32 slots and builds a vector for each replaced stream on every fallback draw. Grouping once moves this layout work to the pipeline and allows each deferred read to share its slot's elements. For R replaced streams, the draw also avoids R ordinary shadow lookups and refresh registrations. Avoided transfer bytes depend on dirty ranges and other list uses. Scratch and expansion still scale with the vertices drawn.

[Related correctness finding](../draw-complete-rectangles-after-vertex-shading.md)
