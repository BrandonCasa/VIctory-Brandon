# Skip ordinary per-vertex shadow preparation for CPU-expanded rectangles

## Location and execution path

`crates/victory-gpu/src/draw.rs:3745–3767` prepares the normal vertex streams before deciding whether a direct rectangle draw needs CPU expansion. `prepare_for` (1465–1491) resolves and refreshes each ordinary input shadow and binds a vertex view. `rectangles` (3793–3849) queues a separate read of the used guest vertex range, expands it into list scratch and replaces the per-vertex IA bindings before the draw. `pipeline/host.rs:705–716,778–785` shows when geometry-stage expansion succeeds or falls back.

## Current cost

For each stream replaced on this path, the normal IA view has no draw consumer. Its full declared range can also cause a deferred shadow refresh in addition to the narrower guest read for expansion. `HostList::defer_refresh` already deduplicates exact ranges (`host.rs:7222–7235`), so redundant transfer bytes depend on other uses in the list.

## Proposed redesign

Decide which per-vertex slots the fallback will replace before ordinary IA preparation. Skip the ordinary refresh/binding only for slots whose final source is expansion scratch. Keep checks that establish source validity, and keep `defer_host_read` (`host.rs:7596`) so expansion observes submission-time bytes and outstanding writes. Preserve per-instance streams, mixed-layout slot semantics, skipped draws on failure, resource lifetimes, and independent consumers of the shadow. Leave indexed, indirect and geometry-stage draws on their existing paths.

## Before and after

For R replaced streams the ordinary shadow lookups, refresh registrations and overwritten binding work can be removed for those streams. Physical bytes avoided range from zero on an existing refresh to the affected dirty shadow ranges. Scratch generation and the required guest reads remain proportional to expanded vertices.

## Expected impact and confidence

Medium to high on titles exercising the fallback with large declared vertex buffers. The overwrite is explicit; successful geometry-stage pipelines receive no benefit. Count fallback draws, replaced streams, otherwise unique refresh bytes and submission CPU before estimating frame-time gains.
