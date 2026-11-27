# Remaining findings

Ranks reflect impact and confidence. Independent fixes can proceed in parallel. For lighting, pair BVH publication (rank 2) with page residency (5), ordered uploads (6 and 7) and compacted-size feedback (8).

Checkboxes reflect individual findings at the last static review. Check the current code and its callers before picking up a row. End-to-end behavior across findings still needs a build and runtime pass.

## Open work

- [ ] **1.** [Keep draws and dispatches whose pipelines are still compiling](present-retain-plane-crops-color-and-blend-order.md) - Keep dependent submissions and presents behind their producer fences when a present would force an unmet wait.
- [ ] **2.** [Restore the scene geometry used by lighting rays](lighting-publish-complete-bvh-builds-before-tracing.md) - Failed in-place refits now have a rollback path. Complete first-access coverage (rank 5) before enabling the builder in normal playback.
- [ ] **3.** [Run the three refused wave64 compute shaders on wave32 GPUs](shader-emulate-wave64-compute-on-wave32.md) - Logical-wave scheduling is in place. Confirm that all three title kernels preserve cross-half lane reads, shared memory and barrier order on wave32.
- [ ] **5.** [Make every reachable BVH page available on first access](lighting-map-reachable-bvh-pages-before-first-access.md) - Known structures reserve their full range. Resolve the reachable range for untracked trace addresses that still fall back to one page.
- [ ] **8.** [Publish the compacted BVH size before the game allocates its destination](lighting-publish-compacted-bvh-size-at-guest-completion.md) - The query now reads back its exact eight bytes before guest completion. Bound the compaction write to its destination allocation when no buffer object supplies that extent.
- [ ] **10.** [Track which physical bytes back each streamed texture subresource](texture-track-subresource-backing-across-remaps.md) - Staging tracks backing changes per mip and slice. Carry that readiness through indexed descriptor publication so a remap preserves the view’s original mip and layer.
- [ ] **11.** [Make texture writes visible through every alias](texture-publish-writes-across-backing-aliases.md) - Each heap consumes write notifications. Order the first indexed read after an alias write, including when the GPU owns the latest bytes.
- [ ] **12.** [Give placed texture UAVs and their readers the same backing](texture-share-placed-uav-writes-with-alias-readers.md) - Compatible placed UAVs resolve to writable textures. Preserve exact bytes across overlapping views that need a different format or storage layout.
- [ ] **13.** [Map the entire guest descriptor heap to valid host slots](bindless-map-full-guest-view-heap-to-host-slots.md) - Pending slots are filtered by descriptor type. Narrow same-type candidates to the slots a draw can reach when the resident arena fills.
- [ ] **14.** [Use the title's probe resolve for final diffuse lighting](lighting-use-title-probe-resolve-for-diffuse-light.md) - The stock playback profile still requests the ambient estimate. Use the title’s resolve once tracing and placed-UAV publication are ready (ranks 2 and 12).
- [ ] **15.** [Read the exact bytes of unaligned constant buffers](draw-bind-exact-bytes-for-unaligned-cbvs.md) - Unaligned CBVs have raw-view and ordered-copy paths. Preserve the accessible range for root CBVs whose source extends beyond the 64 KiB cap.
- [ ] **16.** [Preserve buffer views that start between host elements](bindless-preserve-byte-offsets-in-buffer-views.md) - Ordered reads can copy from a preceding writer. Handle overlapping writable views whose byte offsets cannot be represented by native element-aligned descriptors.

## Implemented and closed findings

- [x] **4.** [Use the same descriptor address for memory preparation and shader binding](draw-snapshot-descriptor-addresses-per-stage-bind.md)
- [x] **6.** [Publish streamed BVH bytes and their page-directory entries together](memory-publish-bvh-landing-writes-in-command-order.md)
- [x] **7.** [Keep refresh planning on the correct side of each BVH upload](submit-order-refreshes-around-bvh-landing-writes.md)
- [x] **9.** [Decode the archive builder's zlib swizzle modes](dstorage-decode-zlib-archive-swizzle-modes.md)
- [x] **17.** [Complete rectangles after the original three vertex invocations](draw-complete-rectangles-after-vertex-shading.md)
- [x] **18.** [Upload the Bink slot and plane that finished decoding](texture-upload-completed-bink-frame-planes.md)
- [x] **19.** [Accept live DirectStorage page lists at Submit](dstorage-honor-submit-boundary-for-live-page-lists.md)
- [x] **20.** [Keep a DirectStorage fill on its accepted physical backing](dstorage-pin-physical-backing-through-fill.md)
- [x] **21.** [Notify the aliases of the exact physical bytes written](xmem-notify-aliases-of-written-physical-byte-ranges.md)
- [x] **22.** [Restore cached graphics pipelines from exact title recipes](pipeline-restore-graphics-psos-from-exact-cache-recipes.md)
- [x] **23.** [Use the title's compute description for cached pipeline requests](pipeline-resolve-cached-compute-psos-from-title-description.md)
- [x] **24.** [Retain each display plane's crop, color space and blend order](pipeline-retain-commands-until-first-use-pso-is-ready.md)
- [x] **25.** [Render and capture the single-plane image the game requested](present-render-and-capture-transformed-single-plane-output.md)
- [x] **26.** [Deliver or cancel each task on its own queue](xasync-preserve-callback-ownership-during-termination.md)
- [x] **27.** [Preserve XMA audio overlap when the title switches contexts](xma-preserve-decoder-overlap-across-context-handoffs.md)
- [x] **28.** [Group GPU fills for recording and descriptor writes](deferred/card-group-fills-for-recording-and-descriptor-writes.md)
- [x] **29.** [Share root signatures across equivalent binding layouts](deferred/pipeline-intern-root-signatures-by-binding-layout.md)
- [x] **30.** [Select only the texture slots the upload pass can service](deferred/texture-select-only-due-slots-within-upload-budget.md)
- [x] **31.** [Detile and upload only the texture footprints that changed](deferred/texture-detile-and-upload-only-changed-footprints.md)
- [x] **32.** [Reuse closed-list refresh summaries and fold live lookahead](deferred/submit-reuse-refresh-summaries-and-fold-lookahead.md)
- [x] **33.** [Skip large holes between disjoint bindless buffer reads](deferred/bindless-refresh-disjoint-buffer-read-spans.md)
- [x] **34.** [Index declared shader writes while recording](deferred/submit-index-declared-shader-writes-per-recording.md)
- [x] **35.** [Stop scanning relay fences at the first unfinished flight](deferred/relay-retire-only-the-completed-flight-prefix.md)
- [x] **36.** [Maintain relay write coverage as flights enter and leave](deferred/relay-maintain-write-coverage-per-flight.md)
- [x] **37.** [Build the ordered command-list view once per submission](deferred/submit-reuse-ordered-command-list-views.md)
- [x] **38.** [Reject textures with no usable cache key before copying them](deferred/texture-store-reject-uncacheable-mip-prefixes-early.md)
- [x] **39.** [Keep original CPU bytes through shadow copyback](deferred/copyback-reuse-original-title-bytes-during-shadow-merge.md)
- [x] **40.** [Cache buffer-span summaries per bindless sweep partition](deferred/bindless-cache-buffer-spans-by-sweep-partition.md)
- [x] **41.** [Clone only texture uploads claimed by this list occurrence](deferred/submit-clone-texture-uploads-only-after-claim.md)
- [x] **42.** [Poll only enabled XMA voices and sleep when all are idle](deferred/xma-poll-enabled-voices-and-sleep-when-idle.md)
- [x] **43.** [Reuse compressed input buffers per storage worker](deferred/dstorage-reuse-worker-compressed-input-buffers.md)
- [x] **44.** [Index stage-bind address snapshots with precomputed ordinals](deferred/draw-index-stage-bind-address-snapshots-by-ordinal.md)
- [x] **45.** [Reuse unchanged vertex and index buffer bindings](deferred/draw-reuse-unchanged-vertex-and-index-bindings.md)
- [x] **46.** [Hash borrowed DXIL only when a pipeline library needs a key](deferred/pipeline-hash-borrowed-dxil-only-for-library-keys.md)
- [x] **47.** [Reuse texture digests for matching content and layouts](deferred/texture-store-reuse-layout-qualified-digests.md)
- [x] **48.** [Share resolved physical runs across validation and writing](deferred/dstorage-share-resolved-page-runs-across-fill-paths.md)
- [x] **49.** [Reuse packed-frame scratch and emit PCM into the voice queue](deferred/xma-reuse-packed-frame-scratch-and-emit-pcm-to-voice.md)
- [x] **50.** [Hash each declared relay piece once per flight](deferred/relay-hash-each-declared-piece-once-per-flight.md)
- [x] **51.** [Maintain the empty texture-block count and skip duplicate probes](deferred/texture-pool-count-empty-blocks-and-skip-failed-retries.md)
- [x] **52.** [Reuse each mip’s tiled column runs across slices](deferred/texture-reuse-tiled-column-runs-per-mip.md)
- [x] **53.** [Reuse retired readback buffers and host footprints](deferred/texture-readback-reuse-retired-buffers-and-footprints.md)
- [x] **54.** [Wake a compiler worker eligible for the queued job](deferred/pipeline-wake-workers-eligible-for-new-jobs.md)
- [x] **55.** [Use one timer for delayed callbacks](deferred/xasync-share-one-timer-for-delayed-callbacks.md)
- [x] **56.** [Stream BCPack records into discardable output](deferred/bcpack-stream-lane-records-into-private-scratch.md)
- [x] **57.** [Bulk-read stale arena pages on eligible paths](deferred/memory-bulk-read-stale-arena-pages-before-merge.md)
- [x] **58.** [Register each numbered shadow once per refresh interval](deferred/memory-register-numbered-shadows-once-per-list-interval.md)
- [x] **59.** [Cache repeated XMA band gains](deferred/xma-cache-quantization-gains-by-exponent.md)
- [x] **60.** [Decode common XMA entropy symbols by prefix](deferred/xma-decode-common-vlc-symbols-by-prefix.md)
- [x] **61.** [Share immutable XMA band layouts per sample rate](deferred/xma-share-band-layouts-by-sample-rate.md)
- [x] **62.** [Index input history by reading sequence](deferred/gameinput-index-next-and-previous-readings-by-sequence.md)
- [x] **63.** [Copy dense linear readbacks directly](deferred/texture-readback-copy-dense-linear-footprints-directly.md)
- [x] **64.** [Skip barriers in empty indirect clamp groups](deferred/indirect-skip-barriers-in-empty-clamp-groups.md)
- [x] **65.** [Release storage workers while submitted page requests retry](deferred/dstorage-suspend-jobs-during-post-submit-page-retries.md)
- [x] **66.** [Run independent raw reads during staging stalls](deferred/dstorage-admit-zero-charge-reads-during-staging-stalls.md)
- [x] **67.** [Intersect long aliases once per written physical run](deferred/xmem-intersect-long-aliases-once-per-physical-run.md)
- [x] **68.** [Index overlapping card-copy ranges when assigning levels](deferred/card-index-overlapping-copy-ranges-for-level-assignment.md)
- [x] **69.** [Query the GPU budget only for optional tile grants](deferred/residency-query-budget-only-for-optional-tile-grants.md)
- [x] **70.** [Skip redundant preparation on CPU-expanded rectangles](deferred/draw-skip-replaced-streams-on-cpu-rectangle-fallback.md) (superseded by rank 17)
- [x] **71.** [Reuse compositor descriptors on fence-safe slots](deferred/present-reuse-plane-descriptors-on-retired-slots.md)
- [x] **72.** [Transition composed output directly to its next consumer](deferred/present-transition-composed-output-to-its-next-use.md)
- [x] **73.** [Skip unused conversions in packed shader loads](deferred/shader-skip-unused-packed-format-conversions.md)
- [x] **74.** [Keep the current wide scalar-load lowering](deferred/shader-keep-existing-wide-scalar-load-lowering.md) (no eligible reads in the captured shaders)
