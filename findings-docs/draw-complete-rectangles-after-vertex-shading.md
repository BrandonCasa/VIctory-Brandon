# Complete rectangle primitives after vertex shading

## Problem

Topology 17 supplies three vertices per rectangle. Expanding a guest vertex stream before the vertex shader can change the inputs the shader fetches. The original procedural shader `VS_Clear_f05fef9c_Wrapped` (EXE offset `0x688cd3c`) fetches position and varyings from a root SRV using the incoming vertex ID. Resetting the first vertex or issuing six invocations changes those addresses. Without the geometry stage, a nonindexed indirect draw with no per-vertex inputs falls back to the first triangle. Other indirect rectangles and direct indexed rectangles are dropped.

## Change

`pipeline/host.rs` already attaches `rect_list_gs` for pipeline topology type 19. Select the geometry-stage variant when the list draws topology 17 and the ordinary variant for ordinary triangles. Pass the original draw arguments through. The existing geometry stage completes distinct, axis-aligned corners with equal positive clip-space `w`. Outside that shape it emits only the input triangle, so its successful creation alone cannot establish full rectangle support.

Restrict `draw.rs::rectangles` to direct, nonindexed draws with independent instance streams, a side-effect-free shader insensitive to the changed vertex IDs, and exactly representable affine outputs. `expand_rects` assumes vertex 1 is the common corner and can round or clamp the new vertex when it repacks the stream. Use it only where that corner order, packing, flat values and any pixel-shader primitive ID remain equivalent. Route the procedural and indirect paths through post-shading expansion. Treat an unavailable equivalent path as unsupported.

`defer_host_read` already records the exact source range, and `card_steps` consumes it. Include that same range in `host::blocked_by`, which currently skips `Step::Host`. Run a CPU expansion only after any ordered producer’s bytes are visible in title memory. A writer earlier in the same GPU list requires post-shading expansion or a split that makes its result available before the CPU read.

## Expected result

With a nonzero first vertex, supported direct, indexed and indirect rectangles on the geometry-stage path cover both triangles using the original three vertex IDs. An overlapping earlier GPU write supplies the bytes used by the CPU fallback.

[Deferred optimization](deferred/draw-skip-replaced-streams-on-cpu-rectangle-fallback.md)
