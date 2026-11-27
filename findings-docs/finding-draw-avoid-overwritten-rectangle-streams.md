# Complete rectangle primitives after vertex shading

## Problem

Topology 17 supplies three vertices per rectangle. Expanding a guest vertex stream before the vertex shader can change the inputs the shader fetches. The original procedural shader `VS_Clear_f05fef9c_Wrapped` (EXE offset `0x688cd3c`) fetches position and varyings from a root SRV using the incoming vertex ID. Resetting the first vertex or issuing six invocations changes those addresses. The indirect fallback also draws only one triangle.

## Change

Select a rectangle PSO using `rect_list_gs` in `pipeline/host.rs`. Pass the original vertex, index and instance arguments through so the geometry stage constructs the fourth corner from three shaded outputs. In `draw.rs::rectangles`, allow CPU expansion only for a supported layout and shader with equivalent affine outputs, preserved vertex IDs and independent instance streams. Treat other failed rectangle PSOs as unsupported. For CPU expansion, register the exact source byte range with the flight wait and card-step planner before reading it at submit.

## Expected result

With a nonzero first vertex, direct, indexed and indirect rectangles cover both triangles using the original three vertex IDs. An overlapping earlier GPU write supplies the bytes used by the CPU fallback.

[Deferred optimization](deferred/finding-draw-avoid-overwritten-rectangle-streams.md)
