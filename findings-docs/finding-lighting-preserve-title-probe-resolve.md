# Leave the title's probe resolve in control of the final diffuse-light image

## Location and execution path

The stock launch wrapper `scripts/play.ps1` requests `VICTORY_AMBIENT=0.8`. After D3D12 executes a recognized title compute dispatch, `crates/victory-gpu/src/draw.rs::State::dispatch` calls `self.ambient(list, p, role)`. The launch log records four such 1280×720 radiance captures at strength 0.8 (11103–11108). `ambient.rs::after` limits the messages for all roles together to four, so the log does not determine which later resolves were modified.

## Current cost

The fork adds a second lighting algorithm after the title shader. `Role::Radiance` reduces visible light to 64×36 cells. `Role::Resolve` gathers it at 160×90 points and dispatches `pass.raise` into the **title's diffuse UAV**. The optional `Role::Air` writes its scattering volume. Neither the estimate nor the constant mode accounts for off-screen emitters and occlusion as the title's probe rays do. `ambient::on` enables the substitution based on global builder PSO availability. A room can become brighter while the light's direction, tint and visibility are still wrong.

## Proposed redesign

1. Remove the `ambient::on()/self.ambient(...)` post-dispatch branch from the ordinary rendering path. Stop selecting the estimate in the stock playback profile. If the existing comparison mode is retained, require an explicit non-reference rendering choice. Keep its cells, mean, raise pass and optional air clamp out of a normal frame. Implementing this finding requires no new lighting shader.
2. Submit the title's actual radiance, probe-trace, temporal-resolve and scattering commands as recorded. Retain their original dimensions, indirect counts, root/heap bindings, per-subresource UAV writes and queue dependencies. Let the title choose or reset temporal history through its own resources and commands. Its radiance and depth views can be placed or indexed. Resolve them through the ordinary binding machinery. The helper's narrow `State::table_view` and global `KEPT` flag have no role in native rendering.
3. Feed the next composite the precise mip, slice and generation the title's resolve wrote. Reuse the normal [placed-UAV write path](finding-texture-preserve-placed-uav-writes.md) and [ordered command recording](finding-pipeline-retain-first-use-commands.md). When a prerequisite is pending, hold the dependent pass. Honor a fallback explicitly issued by the title. A terminally unsupported required pass is a rendering failure, never grounds to synthesize light or reuse a previous shot's color.

## Before and after

Today the host can overwrite a correctly computed probe value just because its global builder state says “off.” With the change, the next title pass sees the probe resolve's exact packed output, including legitimately dark pixels. Removing the substitution alone can darken interiors while the builder is still broken. The [BVH producer finding](finding-lighting-submit-complete-bvh-builds.md) restores the missing source of real interior light.

## Expected impact and confidence

Once the title's rays hit the correct scene geometry, shaded faces and rooms take their brightness and color from the game's lighting. Off-screen lights can contribute, and an occluded surface can stay dark. The overwriting code is explicit. After implementation, compare one title resolve's output with the same subresource at its next composite, accounting for any intervening title writes.
