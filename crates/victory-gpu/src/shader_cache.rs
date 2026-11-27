//! Translated shaders kept on disk (decision 0003): under the configured
//! cache folder, keyed by the container's content, by the translator's
//! version ([`victory_shader::TRANSLATOR_VERSION`]) and by its settings.
//! A rebuild that leaves the version keeps every translation: the store
//! grows by the shaders a change made translate, not by a whole new set
//! each build (16 GB of sets of earlier builds had piled up when the key
//! was the translator's source). Entries go through `victory-pipeline`'s
//! content-addressed cache, which writes them atomically; an entry that
//! does not read back is translated again.
//!
//! Each entry also names the build of the translator that made it (the
//! BLAKE3 of its sources, `build.rs`). One of an earlier build is served
//! as it is, and counted; `victory shaders` ([`precompile`]) translates it
//! again, keeps the new one and says how many came out different (then
//! the version should have been bumped). `VICTORY_SHADER_FRESH=1` does the
//! same in a run; `VICTORY_SHADER_KEY=source` keys by the translator's
//! source as before (a separate set for each build).
//!
//! The entry is our own encoding of [`HostShader`]: a field added to it
//! must be added to `encode` and `decode` too, and [`FORMAT`] bumped.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The encoding of entries ([`encode`]), part of the key.
const FORMAT: u32 = 6;

use victory_pipeline::{Cache, CacheKey, Converter};
use victory_shader::emit::{HostShader, StaticSampler, UserData};
use victory_shader::resource::RootIndexedView;
use victory_shader::{HwStage, RootType};

mod cooperative;

static CACHE: OnceLock<Cache> = OnceLock::new();
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
/// Hits on entries an earlier build of the translator made (served as they
/// are, or when refreshing translated again); of those translated again,
/// how many came out different.
static STALE: AtomicU64 = AtomicU64::new(0);
static REFRESHED: AtomicU64 = AtomicU64::new(0);
static CHANGED: AtomicU64 = AtomicU64::new(0);

/// The build of the translator (the BLAKE3 of its sources, `build.rs`).
pub(crate) const SOURCE: &str = env!("VICTORY_TRANSLATOR");

/// How cached translations are keyed (`VICTORY_SHADER_KEY`).
fn keyed_by_source() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_SHADER_KEY").is_ok_and(|v| v == "source"))
}

/// Whether a run translates entries of an earlier build again
/// (`VICTORY_SHADER_FRESH=1`; [`precompile`] always does).
fn refresh_in_runs() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SHADER_FRESH").is_some_and(|v| v == "1"))
}

/// Keeps translated shaders in `dir` from now on.
pub fn use_shader_cache(dir: PathBuf) {
    match Cache::open(dir) {
        Ok(cache) => {
            let _ = CACHE.set(cache);
        }
        Err(e) => tracing::warn!(error = %e, "no shader cache"),
    }
}

/// Shaders found in the cache and translated since the start.
pub(crate) fn counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// Since the start: entries of an earlier build of the translator met,
/// those of them translated again, and those that came out different.
pub fn stale_counts() -> (u64, u64, u64) {
    (STALE.load(Ordering::Relaxed), REFRESHED.load(Ordering::Relaxed), CHANGED.load(Ordering::Relaxed))
}

/// What identifies a translation beside its container: the translator's
/// version (or with `VICTORY_SHADER_KEY=source` its sources), the form of
/// entries, the settings it runs with, and what it was told of the host.
/// Not our executable: a rebuild of anything else used to translate every
/// shader again.
fn build() -> [u8; 32] {
    // The view heap's null slot differs by host GPU: a translation cached
    // without the clamp must not be served (`host_settings` below).
    crate::descriptor::host::probe_view_limit();
    static FIXED: OnceLock<Vec<u8>> = OnceLock::new();
    let fixed = FIXED.get_or_init(|| {
        let limit = victory_shader::emit::loop_limit();
        if limit != victory_shader::emit::DEFAULT_LOOP_LIMIT {
            tracing::warn!(
                limit,
                "VICTORY_SHADER_LOOP_LIMIT: translated loops stop sooner"
            );
        }
        let mut bytes = if keyed_by_source() {
            SOURCE.as_bytes().to_vec()
        } else {
            format!("version {}", victory_shader::TRANSLATOR_VERSION).into_bytes()
        };
        bytes.extend_from_slice(&FORMAT.to_le_bytes());
        bytes.extend_from_slice(&limit.to_le_bytes());
        if victory_shader::emit::loop_limit_override().is_some() {
            bytes.extend_from_slice(b"explicit loop cap");
        }
        // Which group barriers wait is part of the translation too.
        bytes.push(victory_shader::emit::group_sync());
        // So are the switches that pick the form of a wave64 program:
        // without them here a run with `VICTORY_WAVE_WIDE=0` was
        // served the translations an earlier run of the same
        // executable had cached with the switch unset.
        for name in [
            "VICTORY_WAVE_WIDE",
            "VICTORY_WAVE_DEEP_SKIPS",
            "VICTORY_WAVE_OWN_BITS",
            "VICTORY_WAVE_LOCAL",
            "VICTORY_LDS_FLAGS",
            "VICTORY_F16_NEAREST",
            "VICTORY_HELPER_OWN",
            "VICTORY_FIRSTLANE_OWN",
            "VICTORY_FIRST_HALF",
            "VICTORY_STRICT_JOINS",
            "VICTORY_MESH_OUTPUT_STRICT",
            "VICTORY_BVH_FENCES",
            "VICTORY_BVH_BOUNDS",
        ] {
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(b'=');
            if let Some(value) = std::env::var_os(name) {
                bytes.extend_from_slice(value.as_encoded_bytes());
            }
            bytes.push(0);
        }
        // And which clean-up passes ran over the translated code.
        bytes.extend_from_slice(&victory_shader::emit::shader_opt().to_le_bytes());
        // Renderer port 5 (`VICTORY_PORT_CBV=1`): constant buffers read as raw
        // bytes. Only when on, so the translations cached without it keep
        // their key.
        if victory_shader::emit::constants_raw() {
            bytes.extend_from_slice(b"VICTORY_PORT_CBV=1");
        }
        bytes
    });
    let mut hash = blake3::Hasher::new();
    hash.update(fixed);
    for value in victory_shader::emit::host_settings() {
        hash.update(&value.to_le_bytes());
    }
    hash.update(&victory_shader::emit::host_wave_range().0.to_le_bytes());
    *hash.finalize().as_bytes()
}

/// What tells one set of translations from another ([`build`]).
pub(crate) fn translation() -> [u8; 32] {
    build()
}

fn compute_translation(container: &[u8]) -> Option<victory_shader::emit::ComputeTranslation> {
    use victory_shader::emit::ComputeTranslation;
    let digest = blake3::hash(container);
    match &digest.to_hex()[..16] {
        // One workgroup per tile. The title can dispatch over 65,535 in X.
        "2dc364df5d3278b8" | "940c2635dc1312f4" => Some(ComputeTranslation::DispatchBase),
        // Original CS programs use 32x1x1 groups and whole-quad execution.
        "e79dd4bd900b4200" | "d6dba265fd10bc2b" => Some(ComputeTranslation::Derivatives),
        _ => None,
    }
}

fn translate(container: &[u8]) -> Result<HostShader, String> {
    let translated = match compute_translation(container) {
        Some(mode) => victory_shader::emit::translate_compute(container, mode),
        None => victory_shader::emit::translate(container),
    };
    translated.map_err(|e| e.to_string())
}

fn stale_compute_translation(shader: &HostShader, container: &[u8]) -> bool {
    use victory_shader::emit::{ComputeTranslation, USER_DISPATCH_GROUP_BASE_X};
    if shader.stage != HwStage::Cs { return false; }
    match compute_translation(container) {
        Some(ComputeTranslation::DispatchBase) => !shader.user_data.iter()
            .any(|u| u.root == RootType::Other(USER_DISPATCH_GROUP_BASE_X)),
        Some(ComputeTranslation::Derivatives) => true,
        None => false,
    }
}

/// Puts the translation of `container` into the cache before any title asks
/// for it (`victory shaders`): whether it was there already, or why it does
/// not translate. The host is opened first, as a run does: what the card
/// can do decides the form of some programs.
pub fn precompile(container: &[u8]) -> Result<bool, String> {
    let _ = crate::host::host();
    let misses = MISSES.load(Ordering::Relaxed);
    match CACHE.get() {
        Some(cache) => cached(cache, &build(), container, translate, true)?,
        None => translate(container)?,
    };
    // (Counts other threads' misses too: the caller only adds them up.)
    Ok(MISSES.load(Ordering::Relaxed) == misses)
}

/// The translation of a console shader container, from the cache when
/// there is one.
pub(crate) fn host_shader(container: &[u8]) -> Result<HostShader, String> {
    match CACHE.get() {
        Some(cache) => cached(cache, &build(), container, translate, refresh_in_runs()),
        None => translate(container),
    }
}

/// An older cooperative translation is still a usable live pipeline. The
/// offline worker may try a replacement, but only publish it after the new
/// device-native pipeline has been persisted.
pub(crate) struct WarmReplacement {
    cache: Cache,
    key: CacheKey,
    previous: blake3::Hash,
}

impl WarmReplacement {
    pub(crate) fn publish(self, shader: &HostShader) -> anyhow::Result<()> {
        let current = std::fs::read(self.cache.path_for(&self.key))?;
        let replacement = encode(shader, SOURCE);
        if current == replacement { return Ok(()); }
        anyhow::ensure!(blake3::hash(&current) == self.previous,
            "translation changed during native pipeline compilation");
        self.cache.replace(&self.key, &replacement)
    }
}

fn legacy_scheduler(bytes: &[u8]) -> bool {
    let current = [MAGIC, PACKED_MAGIC, FIXED_MAGIC, SCALAR_RANGE_MAGIC]
        .iter().any(|magic| bytes.starts_with(*magic));
    !current && decode(bytes).is_some_and(|(shader, source)| {
        source != SOURCE.as_bytes() && shader.stage == HwStage::Cs && cooperative::scheduler(&shader.dxil)
    })
}

// This material family predates Wave32 structured waterfall reflection.
// Re-translate matching older entries in place, leaving the native keys
// available when the revised reflection emits the same DXIL.
fn stale_material_reflection(shader: &HostShader) -> bool {
    let access = &shader.indexed_forms;
    if shader.stage != HwStage::Ps || access.opaque != 8 || access.texture_kinds != [1 << 2, 0]
        || access.buffer_indices.len() != 4
    {
        return false;
    }
    let column = |field| victory_shader::resource::BufferIndex {
        table_slot: 0,
        table_dword: 216,
        word: victory_shader::resource::HeapOffset { bytes: field, constant: None },
        word_step: 512,
        bytes: 0,
        guard: None,
        records: None,
        value_mask: 0x00ff_ffff,
    };
    [(column(48), 2), (column(76), 2), (column(48), 5), (column(76), 5)]
        .iter().all(|entry| access.buffer_indices.contains(entry))
}

// These skinning variants were cached before their integer write selector
// was reflected. Refresh their containers under the existing cache keys.
fn stale_skinning_reflection(shader: &HostShader, container: &[u8]) -> bool {
    let access = &shader.indexed_forms;
    shader.stage == HwStage::Cs && access.opaque == 71 && access.offsets.is_empty()
        && access.buffer_uav_indices.is_empty()
        && matches!(blake3::hash(container).to_hex().as_str(),
            "90a0692af43201294c2af6879636a6206ec3d8dd85996d8343df6f92a729acfd"
                | "3cf39f4495b908d07377a85ce80cae27c86f3bf5701679de0f4d0cdb2b922ecb"
                | "e06a21f3cea46973e418744f49e10c92f0018c68e4f29215ae63ee39b177e942"
                | "4518151afe8c6c093a95c362ea19d5fc73d0055668be55405268ea16657f91f0"
                | "904e7a51e3fab26cc037487b4811021b9370de1fedc7dc2ae7f52fcd651df432"
                | "b5a672a450b321e2128f8c8ccdd7fc44ee6b5cdc51b6dfa05d2a9e08a615e9ed"
                | "8f9eb68d0c6dd5cf710e360408e6f93afc67110b1c0b6c95cc995310e7848dd1"
                | "75bd00da58a1615c716f66c150115f5bef919ae1c4222f1f306b66c4f2530b45"
                | "dd24ff46af3ae6eef3da7ef126337c2efd3d378b6a1556b888a7c27a3ff30a67")
}

// Older vehicle entries scan whole columns of the target table. Re-translate
// the variants that can use the bounded packed parent records throughout.
fn stale_vehicle_reflection(shader: &HostShader, container: &[u8]) -> bool {
    let access = &shader.indexed_forms;
    shader.stage == HwStage::Ps && access.packed_material.is_empty()
        && access.buffer_indices.iter().any(|(index, _)| {
            index.table_slot == 0 && index.table_dword == 368 && index.word_step == 64
        })
        && matches!(blake3::hash(container).to_hex().as_str(),
            "19acd330dacc8f325d8385f839ef8697ba8c8d8da5d7a69b6ced20bdc71d3837"
                | "166fb96b4a4584e0cd11df981f7e91864d45f9a73e0e40cd68af043e4ca4fc6a"
                | "212379c7f4ae453fcdb4c803af28ede0b7c9b29171abe553bd70adfb16e177e5"
                | "2af15279e972b9487423a669c75e131d84dc524df72422b3049a730f7f0c0c5b"
                | "34e0f5531559039df421a747875ed80f2777c14366a9707354ee5417e768d070"
                | "5588031a7d0dce3d21389f47970e5e9f306c0a800f19ad4e1e5ffc1c5dd2d711"
                | "5862598309ecead43de02a920f9857470bc4ea09911b8a05a139aa79ccd896b7"
                | "7333603e0e1b4e905112308f532a7a7afea2d5734ba88d0e65d759a57124c514"
                | "7778fb128a65511e46c55ac36e0d24f7745d9b0a217df8468934d0115e6441ad"
                | "81f23cdc52f59a699fe3c17b2b59c263795fe85c142f1b51dc42882bafcbfd16"
                | "8866d7d19733564c54eb06ca993d514ede63b0613ba619b8878932ba3855adfd"
                | "8b796bb872f4252a2f592124756cc187cae5ed3a852b35813f0761b61e6b8a52"
                | "99bdcd7c5640452d1d9295ae9634c81ee3ec4e3451910fc1e99f8ec20a0a4560"
                | "a686a374bbf863c13c39afaec4c6c235b70295cbe791b3c0e6e2a0c4690dffdc"
                | "b3d05023ed08e15e25a3ef171ccab97c4aef9d08a121470fbf436952bdb77b07"
                | "b3e18290c534283df7f461c932caaba488f39ece4d13e454dddd154cc9a73131"
                | "c100f3c699648196ea57c6d57751bdabee42e5a87bcd79f75efe421e89906c7c")
}

fn stale_pedestrian_reflection(shader: &HostShader, container: &[u8]) -> bool {
    let access = &shader.indexed_forms;
    shader.stage == HwStage::Ps && access.fixed_record_columns.is_empty()
        && access.buffer_indices.iter().any(|(index, _)| {
            index.table_slot == 0 && index.table_dword == 0 && index.word_step == 1
        })
        && matches!(blake3::hash(container).to_hex().as_str(),
            "3212e1d185104746b05822f904a3ae33f5b618908468d96a204b846d2f411d2d"
                | "5392b01466c6d1cfba81829025280b6bdc0a1c0c474a198fd4a164b9d1afe2c3"
                | "6a3a8d3fe52b4eabe5fe50808556cb5ed23d97d17974a2f27563d52c16dc60f7"
                | "73644c6a0b7b5b352af29bfdeda729bd7bf537011df84b4c7235dc2dc2bc9464"
                | "7978fc1bf5d076afa268772de86d4c409b44c5d6ad3ecf02328d4f6ade05c58c"
                | "e7bdd8b081286a92e43a11c988e9242e03f4780598408fb512753607cda87380")
}

// These alpha prepasses select their handles through bounded packed material
// records. The earlier translations scanned the target table's whole columns.
fn stale_alpha_material_reflection(shader: &HostShader, container: &[u8]) -> bool {
    shader.stage == HwStage::Ps && shader.indexed_forms.packed_material.is_empty()
        && shader.indexed_forms.buffer_indices.iter().any(|(index, _)| {
            index.table_slot == 0 && index.table_dword == 344 && index.word_step == 64
        })
        && matches!(blake3::hash(container).to_hex().as_str(),
            "3f1c61d954b46f31739d51c5d0dcccd983074cab6be4a4808ba925d7cd5b3c23"
                | "4cde906781d9b043433a69dd12f34cab546ddcf3e009d85f851826162886ec38"
                | "9a326171ff144fc9a928b8a6815992e0cf4e3731becc31f3bc6a24b27d3f24cb"
                | "c5d4ff74b695d1f2be12820f4d1b2d2876a74024f9f520957958716e9afa2891")
}

fn stale_livery_reflection(shader: &HostShader, container: &[u8]) -> bool {
    let access = &shader.indexed_forms;
    shader.stage == HwStage::Ps && access.scalar_range_columns.is_empty()
        && access.buffer_indices.iter().any(|(index, _)| {
            index.table_slot == 0 && index.table_dword == 368 && index.word_step == 64
        })
        && blake3::hash(container).to_hex().as_str()
            == "4b3d2c8f7a15e7a116d03cd21483ad8d288aab6ce2270aa5d76463d421b8f119"
}

// The older corona vertex shader describes both IA-sourced texture handles
// as unconstrained, even when their input elements are available.
fn stale_vertex_input_reflection(shader: &HostShader, container: &[u8]) -> bool {
    shader.stage == HwStage::Vs && shader.indexed_forms.vertex_inputs.is_empty()
        && shader.indexed_forms.opaque & 8 != 0
        && blake3::hash(container).to_hex().as_str()
            == "9453de2c1f865e27dfe1c7f7ea312c71edee25dd453af654f0e2dadd807018f3"
}

fn stale_vehicle_offset_reflection(shader: &HostShader, container: &[u8]) -> bool {
    shader.stage == HwStage::Ps
        && shader.indexed_forms.buffer_indices.iter().any(|(index, _)| {
            index.table_slot == 0 && index.table_dword == 688 && index.word_step == 1
        })
        && blake3::hash(container).to_hex().as_str()
            == "5588031a7d0dce3d21389f47970e5e9f306c0a800f19ad4e1e5ffc1c5dd2d711"
}

// These linked pixel stages use bounded mesh input and scalar-clamped indices
// for the material handle fields that older entries also scan byte by byte.
fn stale_linked_material_reflection(shader: &HostShader, container: &[u8]) -> bool {
    shader.stage == HwStage::Ps
        && shader.indexed_forms.buffer_indices.iter().any(|(index, _)| {
            index.table_slot == 0 && index.table_dword == 160 && index.word_step == 1
        })
        && matches!(blake3::hash(container).to_hex().as_str(),
            "3847458f4cebb0ba79820cc9c52fedd8439a25aba443e9d15aec796d7a37ae12"
                | "ea90eea5044117c0baaf893278b50f4c7ac163bf4f3a5a8aff604db14f3362e2")
}

// Cascade shadow pixel stages need the bound from their matching mesh outputs
// before the precise descriptor fields can replace the older table scan.
fn stale_cascade_shadow_reflection(shader: &HostShader, container: &[u8]) -> bool {
    let access = &shader.indexed_forms;
    match shader.stage {
        HwStage::Ps => access.buffer_indices.iter().any(|(index, _)| {
            index.table_slot == 0 && index.table_dword == 96 && index.word_step == 1
        }) && matches!(blake3::hash(container).to_hex().as_str(),
            "16400b2f082a200651c7cd1c72451690f98f6188522d4d12b193ec53259f52f0"
                | "b3c8d5bdb2d6f4cd88d46e4c84f06d510732756c76657ebb2e9375a42352d4ae"),
        HwStage::Ms => !access.output_limits.iter().any(|&(row, component, max)| {
            row == 15 && component == 0 && max <= 7
        }) && matches!(blake3::hash(container).to_hex().as_str(),
            "4c1c0bc01e07c903da288efe36920b46baf9edbb43fd48d432547a07dda789de"
                | "adad836e0cab90b107390d2ed883f6f28de9574ca02864cb20a38bc30fad7442"),
        _ => false,
    }
}

fn stale_clustered_compute_reflection(shader: &HostShader, container: &[u8]) -> bool {
    let access = &shader.indexed_forms;
    shader.stage == HwStage::Cs && access.opaque & 8 != 0 && access.texture_kinds[0] == 1 << 2
        && blake3::hash(container).to_hex().as_str()
            == "c4183d3cbee28e8e1aee1806f6b2cfcb4732e8fbe9386910e723ce0a32f73205"
}

pub(crate) fn warm_replacement(container: &[u8]) -> Result<Option<(WarmReplacement, HostShader)>, String> {
    let Some(cache) = CACHE.get() else { return Ok(None) };
    let _ = crate::host::host();
    let converter = Translate {
        source: SOURCE, ran: AtomicBool::new(false),
        translate,
    };
    let input = [&build()[..], container].concat();
    let key = CacheKey::new(&converter, &input);
    let Ok(old) = std::fs::read(cache.path_for(&key)) else { return Ok(None) };
    if !legacy_scheduler(&old) { return Ok(None); }
    tracing::info!(path = %cache.path_for(&key).display(), "warming the current cooperative wave scheduler");
    let candidate = (converter.translate)(container)?;
    Ok(Some((WarmReplacement { cache: cache.clone(), key, previous: blake3::hash(&old) }, candidate)))
}

/// The cache entry's converter: the build and the container in, the
/// encoded translation out. `ran` tells a miss from a hit.
struct Translate<'a, F> {
    translate: F,
    /// The build of the translator, written into the entry.
    source: &'a str,
    ran: AtomicBool,
}

impl<F: Fn(&[u8]) -> Result<HostShader, String> + Send + Sync> Converter for Translate<'_, F> {
    fn id(&self) -> &'static str {
        "shader.dxil"
    }
    fn version(&self) -> u32 {
        // The build fingerprint in the input does the versioning.
        1
    }
    fn convert(&self, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.ran.store(true, Ordering::Relaxed);
        let shader = (self.translate)(&input[32..]).map_err(anyhow::Error::msg)?;
        Ok(encode(&shader, self.source))
    }
}

/// `refresh`: an entry an earlier build of the translator made is
/// translated again (and replaced) rather than served.
fn cached(
    cache: &Cache,
    build: &[u8; 32],
    container: &[u8],
    translate: impl Fn(&[u8]) -> Result<HostShader, String> + Send + Sync,
    refresh: bool,
) -> Result<HostShader, String> {
    cached_by(cache, build, SOURCE, container, translate, refresh)
}

/// [`cached`] with the build of the translator given (`source`).
fn cached_by(
    cache: &Cache,
    build: &[u8; 32],
    source: &str,
    container: &[u8],
    translate: impl Fn(&[u8]) -> Result<HostShader, String> + Send + Sync,
    refresh: bool,
) -> Result<HostShader, String> {
    let input = [&build[..], container].concat();
    let converter = Translate {
        translate,
        source,
        ran: AtomicBool::new(false),
    };
    let key = CacheKey::new(&converter, &input);
    for _ in 0..3 {
        let path = cache
            .get_or_convert(&converter, &input)
            .map_err(|e| format!("{e:#}"))?;
        if let Some((shader, made_by)) = std::fs::read(&path).ok().and_then(|b| decode(&b)) {
            let ran = converter.ran.load(Ordering::Relaxed);
            if !shader.supports_wave_range(victory_shader::emit::host_wave_range()) {
                if ran {
                    return Err(
                        "translated compute shader requires an unsupported wave width".to_owned(),
                    );
                }
                let _ = std::fs::remove_file(&path);
                continue;
            }
            if !ran && made_by != source.as_bytes() {
                STALE.fetch_add(1, Ordering::Relaxed);
                if refresh || stale_material_reflection(&shader)
                    || stale_skinning_reflection(&shader, container)
                    || stale_vehicle_reflection(&shader, container)
                    || stale_alpha_material_reflection(&shader, container)
                    || stale_pedestrian_reflection(&shader, container)
                    || stale_livery_reflection(&shader, container)
                    || stale_vehicle_offset_reflection(&shader, container)
                    || stale_linked_material_reflection(&shader, container)
                    || stale_cascade_shadow_reflection(&shader, container)
                    || stale_clustered_compute_reflection(&shader, container)
                    || stale_vertex_input_reflection(&shader, container)
                    || stale_compute_translation(&shader, container)
                {
                    tracing::debug!(path = %path.display(), "shader cache entry of an earlier translator; translating again");
                    let replacement = (converter.translate)(container).and_then(|candidate| {
                        if candidate.supports_wave_range(victory_shader::emit::host_wave_range()) {
                            Ok(candidate)
                        } else {
                            Err("translated compute shader requires an unsupported wave width".to_owned())
                        }
                    });
                    let replacement = match replacement {
                        Ok(candidate) => candidate,
                        Err(error) if refresh => return Err(error),
                        Err(error) => {
                            tracing::warn!(%error, path = %path.display(), "automatic shader refresh failed; keeping previous translation");
                            HITS.fetch_add(1, Ordering::Relaxed);
                            return Ok(shader);
                        }
                    };
                    if let Err(error) = cache.replace(&key, &encode(&replacement, source)) {
                        // A write failure leaves the old entry intact. This caller
                        // can still use the compatible translation in memory.
                        tracing::warn!(%error, path = %path.display(), "could not publish refreshed shader");
                    }
                    REFRESHED.fetch_add(1, Ordering::Relaxed);
                    if shader != replacement {
                        CHANGED.fetch_add(1, Ordering::Relaxed);
                    }
                    MISSES.fetch_add(1, Ordering::Relaxed);
                    return Ok(replacement);
                }
                static SAID: AtomicBool = AtomicBool::new(false);
                if !SAID.swap(true, Ordering::Relaxed) {
                    tracing::info!("shader translations of an earlier build of the translator (same version) are served; `victory shaders` translates them again");
                }
            }
            let (counter, what) = if ran {
                (&MISSES, "translated and cached")
            } else {
                (&HITS, "from the cache")
            };
            counter.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(path = %path.display(), "shader {what}");
            return Ok(shader);
        }
        tracing::debug!(path = %path.display(), "unreadable shader cache entry; translating again");
        let _ = std::fs::remove_file(&path);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    (converter.translate)(container)
}

// Access metadata changed without changing native DXIL, root signatures or
// device PSO keys. Existing translations that need new reflection refresh
// individually in their original cache entries.
// K marks translations made with liveness across cooperative wave sweeps.
// K also accepts an indexed buffer-write tag in the existing selector list.
// Ordinary older entries stay usable.
const MAGIC: &[u8; 4] = b"VSHK";
// Only shaders with packed parent selectors need the additional section.
const PACKED_MAGIC: &[u8; 4] = b"VSHL";
const FIXED_MAGIC: &[u8; 4] = b"VSHM";
const SCALAR_RANGE_MAGIC: &[u8; 4] = b"VSHN";
const VERTEX_INPUT_MAGIC: &[u8; 4] = b"VSHO";
const INDEXED_BUFFER_WRITE: u32 = 4;
const STAGES: [HwStage; 8] = [
    HwStage::Ls,
    HwStage::Hs,
    HwStage::Es,
    HwStage::Gs,
    HwStage::Vs,
    HwStage::Ps,
    HwStage::Ms,
    HwStage::Cs,
];

fn root_code(r: RootType) -> u32 {
    match r {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 2,
        RootType::ImmediateSampler => 3,
        RootType::ResourceHeap => 4,
        RootType::SamplerHeap => 5,
        RootType::Other(x) => 6 + x,
    }
}

fn root_type(code: u32) -> RootType {
    match code {
        0 => RootType::Constants,
        1 => RootType::Descriptor,
        2 => RootType::Table,
        3 => RootType::ImmediateSampler,
        4 => RootType::ResourceHeap,
        5 => RootType::SamplerHeap,
        x => RootType::Other(x - 6),
    }
}

fn write_scalar_program(out: &mut Vec<u8>, program: &[victory_shader::resource::ScalarWordOp]) {
    use victory_shader::resource::ScalarWordOp::*;
    out.extend((program.len() as u32).to_le_bytes());
    for &op in program {
        let (tag, a, b): (u32, u32, u32) = match op {
            Constant(k) => (0, k, 0),
            Read { table: (slot, dword), address } => (1, u32::from(slot) | u32::from(address) << 8, dword),
            Add(a, b) => (2, a.into(), b.into()),
            Multiply(a, b) => (3, a.into(), b.into()),
            And(a, b) => (4, a.into(), b.into()),
            Or(a, b) => (5, a.into(), b.into()),
            ShiftLeft(a, b) => (6, a.into(), b.into()),
            ShiftRight(a, b) => (7, a.into(), b.into()),
        };
        for word in [tag, a, b] { out.extend(word.to_le_bytes()); }
    }
}

/// Little-endian words and length-prefixed lists, after the build of the
/// translator that made the entry (`source`, length-prefixed).
fn encode(s: &HostShader, source: &str) -> Vec<u8> {
    let packed = &s.indexed_forms.packed_material;
    let fixed = &s.indexed_forms.fixed_record_columns;
    let ranges = &s.indexed_forms.scalar_range_columns;
    let vertex_inputs = &s.indexed_forms.vertex_inputs;
    let magic = if !vertex_inputs.is_empty() { VERTEX_INPUT_MAGIC }
        else if !ranges.is_empty() { SCALAR_RANGE_MAGIC } else if !fixed.is_empty() { FIXED_MAGIC }
        else if !packed.is_empty() { PACKED_MAGIC } else { MAGIC };
    let mut out = magic.to_vec();
    let word = |out: &mut Vec<u8>, v: u32| out.extend(v.to_le_bytes());
    word(&mut out, source.len() as u32);
    out.extend(source.as_bytes());
    let stage = STAGES.iter().position(|&x| x == s.stage).unwrap_or(0) as u32;
    word(&mut out, stage);
    word(&mut out, s.dxil.len() as u32);
    out.extend(&s.dxil);
    word(&mut out, s.user_data.len() as u32);
    for u in &s.user_data {
        for v in [u.index, u32::from(u.slot), root_code(u.root), u.dword] {
            word(&mut out, v);
        }
    }
    word(&mut out, s.samplers.len() as u32);
    for sampler in &s.samplers {
        for &v in sampler {
            word(&mut out, v);
        }
    }
    word(&mut out, s.static_samplers.len() as u32);
    for t in &s.static_samplers {
        let [u, v, w] = t.address;
        let words = [
            t.filter,
            u,
            v,
            w,
            t.mip_lod_bias.to_bits(),
            t.max_anisotropy,
            t.comparison,
            t.border,
            t.min_lod.to_bits(),
            t.max_lod.to_bits(),
        ];
        for v in words {
            word(&mut out, v);
        }
    }
    for slots in [&s.root_cbvs, &s.root_srvs, &s.root_uavs] {
        word(&mut out, slots.len() as u32);
        out.extend(slots);
    }
    word(&mut out, u32::from(s.indexed_forms.opaque));
    for kinds in s.indexed_forms.texture_kinds {
        word(&mut out, kinds);
    }
    word(&mut out, s.indexed_forms.offsets.len() as u32);
    for (offset, form) in &s.indexed_forms.offsets {
        word(&mut out, offset.bytes);
        word(&mut out, u32::from(*form));
        match offset.constant {
            None => word(&mut out, u32::MAX),
            Some((slot, dword, shift)) => {
                word(&mut out, u32::from(slot));
                word(&mut out, dword);
                word(&mut out, u32::from(shift));
            }
        }
    }
    word(&mut out, s.root_indexed_views.len() as u32);
    for view in &s.root_indexed_views {
        for value in [
            u32::from(view.table_slot),
            u32::from(view.constant_slot),
            view.constant_dword,
            view.byte_offset,
            u32::from(view.shift),
        ] {
            word(&mut out, value);
        }
    }
    word(&mut out, s.indexed_forms.chains.len() as u32);
    for &(chain, kind) in &s.indexed_forms.chains {
        for (slot, dword) in [chain.groups, chain.records] {
            word(&mut out, u32::from(slot));
            word(&mut out, dword);
        }
        for value in [chain.parent, chain.handle, u32::from(kind)] { word(&mut out, value); }
    }
    word(&mut out, s.indexed_forms.scalar_records.len() as u32);
    for &(c, kind) in &s.indexed_forms.scalar_records {
        for (slot, dword) in [c.index, c.instances, c.records] {
            word(&mut out, u32::from(slot));
            word(&mut out, dword);
        }
        for value in [c.index_offset, c.instance_stride, c.packed_offset, u32::from(c.record_shift),
            c.record_stride, c.count_offset, c.handle_offset, c.handle_stride, u32::from(kind)] { word(&mut out, value); }
    }
    word(&mut out,s.indexed_forms.pointer_fields.len() as u32);
    for &(f,kind) in &s.indexed_forms.pointer_fields {
        for (s,d) in [f.pointer,f.records] { word(&mut out,u32::from(s)); word(&mut out,d); }
        for x in [f.pointer_offset,f.field,u32::from(kind)] { word(&mut out,x); }
    }
    for limits in [&s.indexed_forms.input_limits,&s.indexed_forms.output_limits] {
        word(&mut out,limits.len() as u32);
        for &(row,col,limit) in limits { word(&mut out,row.into()); word(&mut out,col.into()); word(&mut out,limit); }
    }
    word(&mut out, s.indexed_forms.scalar_programs.len() as u32);
    for (program, kind) in &s.indexed_forms.scalar_programs {
        word(&mut out, u32::from(*kind));
        write_scalar_program(&mut out, program);
    }
    let sampled=s.indexed_forms.buffer_indices.iter().map(|(i,k)|(i,u32::from(*k)));
    let storage=s.indexed_forms.uav_indices.iter().map(|(i,k,w)|(i,u32::from(*k) | 256 | (u32::from(*w)<<9)));
    let buffer_writes = s.indexed_forms.buffer_uav_indices.iter()
        .map(|(index, form)| (index, u32::from(*form) | (INDEXED_BUFFER_WRITE << 8)));
    word(&mut out,(sampled.len()+storage.len()+buffer_writes.len()) as u32);
    for (index, kind) in sampled.chain(storage).chain(buffer_writes) {
        let (slot, dword, shift) = index.word.constant
            .map_or((u32::MAX, 0, 0), |(s, d, k)| (u32::from(s), d, u32::from(k)));
        for v in [u32::from(index.table_slot), index.table_dword, index.word.bytes,
                  slot, dword, shift, index.word_step, index.bytes, kind] {
            word(&mut out, v);
        }
        word(&mut out, index.guard.map_or(u32::MAX, |g| u32::from(g.table_slot)));
        if let Some(guard) = index.guard {
            word(&mut out, guard.table_dword);
            word(&mut out, guard.offset);
        }
        word(&mut out, index.records.map_or(0, |r| r.stride));
        if let Some(records) = index.records {
            for (slot,dword) in [records.first, records.count] {
                word(&mut out, u32::from(slot));
                word(&mut out, dword);
            }
        }
        word(&mut out, index.value_mask);
    }
    if magic != MAGIC {
        word(&mut out, packed.len() as u32);
        for &(column, kind) in packed {
            for (slot, dword) in [column.parent, column.records] {
                word(&mut out, u32::from(slot));
                word(&mut out, dword);
            }
            for value in [column.parent_count, column.parent_stride, column.packed_field,
                u32::from(column.shift), column.index_mask, column.record_stride,
                column.handle_field, column.bytes, u32::from(kind)]
            {
                word(&mut out, value);
            }
        }
    }
    if [FIXED_MAGIC, SCALAR_RANGE_MAGIC, VERTEX_INPUT_MAGIC].contains(&magic) {
        word(&mut out, fixed.len() as u32);
        for &(column, kind) in fixed {
            for value in [u32::from(column.table.0), column.table.1, column.field,
                column.stride, column.count, column.bytes, column.value_mask, u32::from(kind)]
            {
                word(&mut out, value);
            }
        }
    }
    if magic == SCALAR_RANGE_MAGIC || magic == VERTEX_INPUT_MAGIC {
        word(&mut out, ranges.len() as u32);
        for (column, kind) in ranges {
            for value in [u32::from(column.records.0), column.records.1, column.count,
                column.record_stride, column.handle_field, column.bytes, u32::from(column.zero_record), u32::from(*kind)]
            {
                word(&mut out, value);
            }
            write_scalar_program(&mut out, &column.base);
        }
    }
    if magic == VERTEX_INPUT_MAGIC {
        word(&mut out, vertex_inputs.len() as u32);
        for &(index, kind) in vertex_inputs {
            for value in [u32::from(index.row), u32::from(index.channel), index.bytes,
                index.value_mask, u32::from(kind)]
            {
                word(&mut out, value);
            }
        }
    }
    word(&mut out, s.halfword_aligned_roots.len() as u32);
    for &(slot, dword) in &s.halfword_aligned_roots {
        word(&mut out, u32::from(slot));
        word(&mut out, dword);
    }
    out
}

/// The shader and the build of the translator that made it.
fn decode(bytes: &[u8]) -> Option<(HostShader, Vec<u8>)> {
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let (head, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            Some(head)
        }
        fn word(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn list(&mut self) -> Option<Vec<u8>> {
            let n = self.word()? as usize;
            Some(self.take(n)?.to_vec())
        }
        fn scalar_program(&mut self) -> Option<Vec<victory_shader::resource::ScalarWordOp>> {
            use victory_shader::resource::ScalarWordOp::*;
            let count = self.word()?;
            if !(1..=32).contains(&count) { return None; }
            let mut program = Vec::with_capacity(count as usize);
            for i in 0..count {
                let (tag, a, b) = (self.word()?, self.word()?, self.word()?);
                let op = match tag {
                    0 if b == 0 => Constant(a),
                    1 if a >> 8 < i => Read { table: (a as u8, b), address: (a >> 8) as u8 },
                    2..=7 if a < i && b < i => [Add, Multiply, And, Or, ShiftLeft, ShiftRight][tag as usize - 2](a as u8, b as u8),
                    _ => return None,
                };
                program.push(op);
            }
            matches!(program.last(), Some(Read { .. })).then_some(program)
        }
    }
    let mut r = Reader(bytes);
    let magic = r.take(4)?;
    let vertex_access = magic == VERTEX_INPUT_MAGIC;
    let range_access = magic == SCALAR_RANGE_MAGIC || vertex_access;
    let fixed_access = magic == FIXED_MAGIC || range_access;
    let packed_access = magic == PACKED_MAGIC || fixed_access;
    let typed_access = packed_access || magic == MAGIC || magic == b"VSHJ";
    let linked_access = typed_access || matches!(magic,b"VSHG"|b"VSHH"|b"VSHI");
    let record_access = linked_access || magic == b"VSHF";
    let masked_access = record_access || matches!(magic, b"VSHC" | b"VSHD" | b"VSHE");
    let current_access = masked_access || magic == b"VSHB";
    let legacy_access = match magic {
        b"VSH8" | b"VSH9" => true,
        b"VSHA" | b"VSHB" | b"VSHC" | b"VSHD" | b"VSHE" | b"VSHF" | b"VSHG" | b"VSHH" | b"VSHI" => false,
        _ if typed_access => false,
        _ => return None,
    };
    let made_by = r.list()?;
    let stage = *STAGES.get(r.word()? as usize)?;
    let dxil = r.list()?;
    let n = r.word()? as usize;
    let mut user_data = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        user_data.push(UserData {
            index: r.word()?,
            slot: u8::try_from(r.word()?).ok()?,
            root: root_type(r.word()?),
            dword: r.word()?,
        });
    }
    let n = r.word()? as usize;
    let mut samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        samplers.push([r.word()?, r.word()?, r.word()?, r.word()?]);
    }
    let n = r.word()? as usize;
    let mut static_samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        static_samplers.push(StaticSampler {
            filter: r.word()?,
            address: [r.word()?, r.word()?, r.word()?],
            mip_lod_bias: f32::from_bits(r.word()?),
            max_anisotropy: r.word()?,
            comparison: r.word()?,
            border: r.word()?,
            min_lod: f32::from_bits(r.word()?),
            max_lod: f32::from_bits(r.word()?),
        });
    }
    let root_cbvs = r.list()?;
    let root_srvs = r.list()?;
    let root_uavs = r.list()?;
    let opaque = u8::try_from(r.word()?).ok()?;
    let texture_kinds = [r.word()?, r.word()?];
    let count = r.word()? as usize;
    let mut offsets = Vec::with_capacity(count.min(256));
    for _ in 0..count {
        let bytes = r.word()?;
        let form = u8::try_from(r.word()?).ok()?;
        let slot = r.word()?;
        let constant = if slot == u32::MAX { None } else {
            Some((u8::try_from(slot).ok()?, r.word()?, u8::try_from(r.word()?).ok().filter(|s| *s < 32)?))
        };
        offsets.push((victory_shader::resource::HeapOffset { bytes, constant }, form));
    }
    let n = r.word()? as usize;
    let mut root_indexed_views = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        root_indexed_views.push(RootIndexedView {
            table_slot: u8::try_from(r.word()?).ok()?,
            constant_slot: u8::try_from(r.word()?).ok()?,
            constant_dword: r.word()?,
            byte_offset: r.word()?,
            shift: u8::try_from(r.word()?).ok().filter(|&s| s < 32)?,
        });
    }
    let mut chains = Vec::new();
    if current_access {
        for _ in 0..r.word()? {
            let groups = (u8::try_from(r.word()?).ok()?, r.word()?);
            let records = (u8::try_from(r.word()?).ok()?, r.word()?);
            let parent = r.word()?;
            let handle = r.word()?;
            let kind = u8::try_from(r.word()?).ok().filter(|k| (1..=9).contains(k))?;
            chains.push((victory_shader::resource::MaterialChain { groups, records, parent, handle }, kind));
        }
    }
    let mut scalar_records = Vec::new();
    if record_access {
        for _ in 0..r.word()? {
            let index = (u8::try_from(r.word()?).ok()?, r.word()?);
            let instances = (u8::try_from(r.word()?).ok()?, r.word()?);
            let records = (u8::try_from(r.word()?).ok()?, r.word()?);
            let c = victory_shader::resource::ScalarRecordChain { index, instances, records,
                index_offset:r.word()?, instance_stride:r.word()?, packed_offset:r.word()?,
                record_shift:u8::try_from(r.word()?).ok().filter(|k| *k < 32)?, record_stride:r.word()?,
                count_offset:r.word()?, handle_offset:r.word()?, handle_stride:r.word()? };
            if !c.handle_stride.is_power_of_two() { return None; }
            let kind = u8::try_from(r.word()?).ok().filter(|k| (1..=9).contains(k))?;
            scalar_records.push((c,kind));
        }
    }
    let mut pointer_fields = Vec::new();
    let (mut input_limits,mut output_limits) = (Vec::new(),Vec::new());
    if linked_access {
        for _ in 0..r.word()? {
            let pointer = (u8::try_from(r.word()?).ok()?, r.word()?);
            let records = (u8::try_from(r.word()?).ok()?, r.word()?);
            let f = victory_shader::resource::PointerField { pointer,records,pointer_offset:r.word()?,field:r.word()? };
            let kind = u8::try_from(r.word()?).ok().filter(|k| (1..=9).contains(k))?;
            pointer_fields.push((f,kind));
        }
        for limits in [&mut input_limits,&mut output_limits] {
            for _ in 0..r.word()? {
                limits.push((u8::try_from(r.word()?).ok()?,u8::try_from(r.word()?).ok().filter(|k| *k<4)?,r.word()?));
            }
        }
    }
    let mut scalar_programs = Vec::new();
    if linked_access {
        for _ in 0..r.word()? {
            let kind = u8::try_from(r.word()?).ok().filter(|k| (1..=9).contains(k))?;
            scalar_programs.push((r.scalar_program()?, kind));
        }
    }
    let count = r.word()? as usize;
    let mut buffer_indices = Vec::with_capacity(count.min(256));
    let mut uav_indices = Vec::new();
    let mut buffer_uav_indices = Vec::new();
    for _ in 0..count {
        let table_slot = u8::try_from(r.word()?).ok()?;
        let table_dword = r.word()?;
        let word_bytes = r.word()?;
        let slot = r.word()?;
        let dword = r.word()?;
        let shift = u8::try_from(r.word()?).ok().filter(|&k| k < 32)?;
        let word_step = r.word()?;
        if word_step != 0 && !word_step.is_power_of_two() { return None; }
        if word_step != 0 && (slot != u32::MAX || word_bytes >= word_step) {
            return None;
        }
        let bytes = r.word()?;
        let typed_kind=r.word()?;
        let kind=(typed_kind & 255) as u8;
        let access=typed_kind >> 8;
        let buffer_write = access == INDEXED_BUFFER_WRITE;
        if buffer_write {
            if !(magic == MAGIC || packed_access) || kind != (4 | victory_shader::emit::BUFFER_WRITE_ACCESS) { return None; }
        } else if !(1..=9).contains(&kind) || !matches!(access,0|1|3) || (access!=0 && !typed_access) {
            return None;
        }
        let constant = if slot == u32::MAX { None } else { Some((u8::try_from(slot).ok()?, dword, shift)) };
        let word = victory_shader::resource::HeapOffset { bytes: word_bytes, constant };
        let guard_slot = if legacy_access { u32::MAX } else { r.word()? };
        let guard = if guard_slot == u32::MAX { None } else {
            let table_slot = u8::try_from(guard_slot).ok()?;
            let table_dword = r.word()?;
            let offset = r.word()?;
            offset.checked_add(7)?;
            Some(victory_shader::resource::BufferRange { table_slot, table_dword, offset })
        };
        let stride = if current_access { r.word()? } else { 0 };
        let records = if stride == 0 { None } else {
            if stride < 4 || !stride.is_multiple_of(4) || word_step != 0 || word.constant.is_some()
                || !word_bytes.is_multiple_of(4) || word_bytes.checked_add(4)? > stride { return None; }
            let mut root = || Some((u8::try_from(r.word()?).ok()?, r.word()?));
            Some(victory_shader::resource::IndexRecords { first: root()?, count: root()?, stride })
        };
        let value_mask = if masked_access { r.word()? } else { u32::MAX };
        let index=victory_shader::resource::BufferIndex {table_slot,table_dword,word,word_step,bytes,guard,records,value_mask};
        if buffer_write {
            if word_step != 0 || guard.is_some() || records.is_none() { return None; }
            buffer_uav_indices.push((index,kind));
        } else if access==0 {buffer_indices.push((index,kind));} else {
            if word_step!=0 || guard.is_some() || records.is_some(){return None;}
            uav_indices.push((index,kind,access==3));
        }
    }
    if !current_access && (opaque & 8 != 0 || buffer_indices.iter().any(|(index, _)| index.word_step != 0)) { return None; }
    if magic == b"VSHG" && buffer_indices.iter().any(|(index,_)| index.word_step != 0) { return None; }
    if !typed_access && ((magic!=b"VSHI" && opaque & 8 != 0) || opaque & 16 != 0) { return None; }
    if !linked_access && (stage == HwStage::Ms || opaque & 8 != 0
        || buffer_indices.iter().any(|(index,_)| index.word_step == 1)) { return None; }
    if magic == b"VSHC" && buffer_indices.iter().any(|(i, _)| i.value_mask != u32::MAX && i.word_step != 0) { return None; }
    if legacy_access && (opaque & 16 != 0 || offsets.iter().any(|(_, form)| form & 16 != 0)
        || buffer_indices.iter().any(|(index, _)| index.word_step != 0))
    {
        return None;
    }
    let mut packed_material = Vec::new();
    if packed_access {
        for _ in 0..r.word()? {
            let parent = (u8::try_from(r.word()?).ok()?, r.word()?);
            let records = (u8::try_from(r.word()?).ok()?, r.word()?);
            let column = victory_shader::resource::PackedMaterialColumn {
                parent,
                records,
                parent_count: r.word()?,
                parent_stride: r.word()?,
                packed_field: r.word()?,
                shift: u8::try_from(r.word()?).ok()?,
                index_mask: r.word()?,
                record_stride: r.word()?,
                handle_field: r.word()?,
                bytes: r.word()?,
            };
            if column.parent_count == 0 || column.parent_count.checked_mul(column.parent_stride)? > 4096
                || !column.parent_stride.is_multiple_of(4) || !column.packed_field.is_multiple_of(4)
                || column.packed_field.checked_add(4)? > column.parent_stride
                || !matches!(column.shift, 0 | 16) || column.index_mask != 0xffff
                || !column.record_stride.is_multiple_of(4) || !column.handle_field.is_multiple_of(4)
                || column.handle_field.checked_add(4)? > column.record_stride
                || column.index_mask.checked_mul(column.record_stride)?.checked_add(column.handle_field).is_none()
            {
                return None;
            }
            let kind = u8::try_from(r.word()?).ok().filter(|kind| (1..=9).contains(kind))?;
            packed_material.push((column, kind));
        }
    }
    let mut fixed_record_columns = Vec::new();
    if fixed_access {
        for _ in 0..r.word()? {
            let column = victory_shader::resource::FixedRecordColumn {
                table: (u8::try_from(r.word()?).ok()?, r.word()?),
                field: r.word()?, stride: r.word()?, count: r.word()?, bytes: r.word()?, value_mask: r.word()?,
            };
            if !(1..=512).contains(&column.count) || !column.stride.is_multiple_of(4)
                || !column.field.is_multiple_of(4) || column.field.checked_add(4)? > column.stride
                || column.count.checked_mul(column.stride).is_none()
            {
                return None;
            }
            let kind = u8::try_from(r.word()?).ok().filter(|kind| (1..=9).contains(kind))?;
            fixed_record_columns.push((column, kind));
        }
    }
    let mut scalar_range_columns = Vec::new();
    if range_access {
        for _ in 0..r.word()? {
            let records = (u8::try_from(r.word()?).ok()?, r.word()?);
            let (count, record_stride, handle_field, bytes) = (r.word()?, r.word()?, r.word()?, r.word()?);
            let zero_record = match r.word()? { 0 => false, 1 => true, _ => return None };
            let kind = u8::try_from(r.word()?).ok().filter(|kind| (1..=9).contains(kind))?;
            if !(1..=256).contains(&count) || !record_stride.is_power_of_two() || record_stride > 64
                || !handle_field.is_multiple_of(4) || handle_field.checked_add(4)? > record_stride
            {
                return None;
            }
            scalar_range_columns.push((victory_shader::resource::ScalarRangeColumn {
                base: r.scalar_program()?, records, count, record_stride, handle_field, bytes, zero_record,
            }, kind));
        }
    }
    let mut vertex_inputs = Vec::new();
    if vertex_access {
        let count = r.word()?;
        if stage != HwStage::Vs || count == 0 || count as usize > r.0.len() / 20 {
            return None;
        }
        for _ in 0..count {
            let row = u8::try_from(r.word()?).ok()?;
            let channel = u8::try_from(r.word()?).ok().filter(|&channel| channel < 4)?;
            let bytes = r.word()?;
            let value_mask = r.word()?;
            let kind = u8::try_from(r.word()?).ok().filter(|kind| (1..=9).contains(kind))?;
            vertex_inputs.push((victory_shader::resource::VertexInputIndex { row, channel, bytes, value_mask }, kind));
        }
    }
    let indexed_forms = victory_shader::emit::IndexedAccess { opaque, texture_kinds, offsets, buffer_indices, uav_indices, buffer_uav_indices, packed_material, fixed_record_columns, scalar_range_columns, vertex_inputs, chains, scalar_records, scalar_programs, pointer_fields,input_limits,output_limits };
    let count = r.word()? as usize;
    if r.0.len() != count.checked_mul(8)? {
        return None;
    }
    let mut halfword_aligned_roots = Vec::with_capacity(count);
    for _ in 0..count {
        halfword_aligned_roots.push((u8::try_from(r.word()?).ok()?, r.word()?));
    }
    r.0.is_empty().then_some((
        HostShader {
        stage,
        dxil,
        user_data,
        samplers,
        static_samplers,
        root_cbvs,
        root_srvs,
        root_uavs,
        indexed_forms,
        root_indexed_views,
        halfword_aligned_roots,
        },
        made_by,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shader() -> HostShader {
        HostShader {
            stage: HwStage::Ps,
            dxil: b"DXBC and the rest".to_vec(),
            user_data: vec![UserData {
                index: 8,
                slot: 1,
                root: RootType::Table,
                dword: 0,
            }],
            samplers: vec![[0x400, 0xc0_0000, 0x18f0_0000, 0]],
            static_samplers: vec![StaticSampler::from_words([
                0x400,
                0xc0_0000,
                0x18f0_0000,
                0,
            ])],
            root_cbvs: vec![2],
            root_srvs: vec![0, 3],
            root_uavs: vec![1],
            indexed_forms: victory_shader::emit::IndexedAccess {
                opaque: 8 | 16,
                texture_kinds: [1 << 2 | 1 << 5, 1 << 4],
                offsets: Vec::new(),
                uav_indices: vec![(victory_shader::resource::BufferIndex {
                    table_slot:1,table_dword:80,word:victory_shader::resource::HeapOffset {bytes:16,constant:None},
                    word_step:0,bytes:0,guard:None,records:None,value_mask:u32::MAX,
                },2,true)],
                buffer_uav_indices: Vec::new(),
                packed_material: Vec::new(),
                fixed_record_columns: Vec::new(),
                scalar_range_columns: Vec::new(),
                vertex_inputs: Vec::new(),
                pointer_fields:vec![(victory_shader::resource::PointerField {pointer:(1,56),records:(0,160),pointer_offset:28,field:4},2)],
                scalar_programs: vec![(vec![victory_shader::resource::ScalarWordOp::Constant(20),
                    victory_shader::resource::ScalarWordOp::Read { table:(1,8),address:0 }],2)],
                input_limits:vec![(14,0,7)], output_limits:vec![],
                scalar_records: vec![(victory_shader::resource::ScalarRecordChain {
                    index:(1,8), instances:(0,712), records:(0,480), index_offset:20, instance_stride:48,
                    packed_offset:24, record_shift:16, record_stride:912, count_offset:136,
                    handle_offset:148, handle_stride:16,
                },2)],
                chains: vec![(victory_shader::resource::MaterialChain {
                    groups: (1, 0), records: (0, 688), parent: 4, handle: 924,
                }, 2)],
                buffer_indices: vec![(victory_shader::resource::BufferIndex {
                    table_slot: 0, table_dword: 688,
                    word: victory_shader::resource::HeapOffset { bytes: 4, constant: Some((4, 1, 0)) },
                    word_step: 0, bytes: 0, guard: None, records: None, value_mask: u32::MAX,
                }, 2)],
            },
            root_indexed_views: vec![RootIndexedView {
                table_slot: 1,
                constant_slot: 2,
                constant_dword: 3,
                byte_offset: 32,
                shift: 5,
            }],
            halfword_aligned_roots: vec![(0, 3), (2, u32::MAX)],
        }
    }

    #[test]
    fn translations_come_back_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [7u8; 32];
        let container = b"a console container".as_slice();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        assert_eq!(cached(&cache, &build, container, translate, false), Ok(shader()));
        assert_eq!(cached(&cache, &build, container, translate, false), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1, "the second came from disk");

        // Another build translates again.
        assert_eq!(cached(&cache, &[8; 32], container, translate, false), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);

        // A damaged entry is a miss, translated and stored again.
        for e in walk(dir.path()) {
            std::fs::write(e, b"VSH8 broken").unwrap();
        }
        assert_eq!(cached(&cache, &build, container, translate, false), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);
        assert_eq!(cached(&cache, &build, container, translate, false), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);

        // Failures are not stored.
        let fail = |_: &[u8]| Err::<HostShader, _>("unsupported".to_owned());
        assert!(cached(&cache, &build, b"other", fail, false).is_err());
    }

    #[test]
    fn stale_material_family_refreshes_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [7u8; 32];
        let mut previous = shader();
        previous.stage = HwStage::Ps;
        let mut column = previous.indexed_forms.buffer_indices[0].0;
        column.table_dword = 216;
        column.word.constant = None;
        column.word_step = 512;
        column.value_mask = 0x00ff_ffff;
        previous.indexed_forms = victory_shader::emit::IndexedAccess {
            opaque: 8,
            texture_kinds: [1 << 2, 0],
            ..Default::default()
        };
        for (field, kind) in [(48, 2), (76, 2), (48, 5), (76, 5)] {
            column.word.bytes = field;
            previous.indexed_forms.buffer_indices.push((column, kind));
        }
        let mut unrelated = previous.clone();
        unrelated.indexed_forms.buffer_indices[0].0.value_mask = u32::MAX;
        let mut refreshed = previous.clone();
        refreshed.indexed_forms.opaque = 0;
        refreshed.indexed_forms.texture_kinds = [0; 2];

        for (container, old) in [(b"material".as_slice(), &previous), (b"other".as_slice(), &unrelated)] {
            cached_by(&cache, &build, "old source", container, |_| Ok(old.clone()), false).unwrap();
        }
        let paths = walk(dir.path());
        let convert = |_: &[u8]| Ok(refreshed.clone());
        assert_eq!(cached_by(&cache, &build, "new source", b"material", convert, false), Ok(refreshed));
        assert_eq!(cached_by(&cache, &build, "new source", b"other", |_| panic!("unrelated shader refreshed"), false), Ok(unrelated));
        assert_eq!(walk(dir.path()).len(), paths.len());
        assert!(paths.iter().all(|path| path.is_file()));
    }

    fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() {
                files.extend(walk(&e.path()));
            } else {
                files.push(e.path());
            }
        }
        files
    }

    #[test]
    fn entries_round_trip_and_reject_damage() {
        let mut host = shader();
        let (mut scan, kind) = host.indexed_forms.buffer_indices[0];
        for (step, residue) in [(1, 0), (4, 3), (16, 8), (128, 87)] {
            scan.word_step = step;
            scan.word = victory_shader::resource::HeapOffset { bytes: residue, constant: None };
            scan.guard = Some(victory_shader::resource::BufferRange { table_slot: 1, table_dword: 24, offset: 120 });
            host.indexed_forms.buffer_indices.push((scan, kind));
        }
        scan.word_step = 0;
        scan.word.bytes = 80;
        scan.guard = None;
        scan.records = Some(victory_shader::resource::IndexRecords { first: (2,0), count: (2,1), stride: 176 });
        host.indexed_forms.buffer_indices.push((scan, kind));
        let bytes = encode(&host, "made by");
        let mut previous_layout = bytes.clone();
        previous_layout[..4].copy_from_slice(b"VSHJ");
        assert_eq!(decode(&previous_layout), Some((host.clone(), b"made by".to_vec())));
        scan.table_dword = 112;
        scan.word.bytes = 60;
        scan.value_mask = 0x00ff_ffff;
        host.indexed_forms.buffer_uav_indices.push((scan, 4 | victory_shader::emit::BUFFER_WRITE_ACCESS));
        let bytes = encode(&host, "made by");
        previous_layout = bytes.clone();
        previous_layout[..4].copy_from_slice(b"VSHJ");
        assert!(decode(&previous_layout).is_none());
        let mut packed = host.clone();
        packed.indexed_forms.packed_material.push((victory_shader::resource::PackedMaterialColumn {
            parent: (1, 24), records: (0, 368), parent_count: 256, parent_stride: 16,
            packed_field: 8, shift: 16, index_mask: 0xffff, record_stride: 64,
            handle_field: 36, bytes: 0,
        }, 2));
        let packed_bytes = encode(&packed, "made by");
        assert!(packed_bytes.starts_with(PACKED_MAGIC));
        assert_eq!(decode(&packed_bytes), Some((packed.clone(), b"made by".to_vec())));
        packed.indexed_forms.packed_material[0].0.parent_count += 1;
        assert!(decode(&encode(&packed, "made by")).is_none());
        let mut fixed = host.clone();
        fixed.indexed_forms.fixed_record_columns.push((victory_shader::resource::FixedRecordColumn {
            table: (0, 0), field: 4, stride: 136, count: 512, bytes: 0, value_mask: u32::MAX,
        }, 2));
        let fixed_bytes = encode(&fixed, "made by");
        assert!(fixed_bytes.starts_with(FIXED_MAGIC));
        assert_eq!(decode(&fixed_bytes), Some((fixed.clone(), b"made by".to_vec())));
        fixed.indexed_forms.fixed_record_columns[0].0.count += 1;
        assert!(decode(&encode(&fixed, "made by")).is_none());
        let mut range = host.clone();
        range.indexed_forms.scalar_range_columns.push((victory_shader::resource::ScalarRangeColumn {
            base: range.indexed_forms.scalar_programs[0].0.clone(), records: (0, 368),
            count: 256, record_stride: 64, handle_field: 44, bytes: 0, zero_record: true,
        }, 2));
        let range_bytes = encode(&range, "made by");
        assert!(range_bytes.starts_with(SCALAR_RANGE_MAGIC));
        assert_eq!(decode(&range_bytes), Some((range.clone(), b"made by".to_vec())));
        range.indexed_forms.scalar_range_columns[0].0.count += 1;
        assert!(decode(&encode(&range, "made by")).is_none());
        let mut vertex = host.clone();
        vertex.stage = HwStage::Vs;
        vertex.indexed_forms.vertex_inputs.push((victory_shader::resource::VertexInputIndex {
            row: 2, channel: 1, bytes: 0, value_mask: 0x07ff_ffff,
        }, 2));
        let vertex_bytes = encode(&vertex, "made by");
        assert!(vertex_bytes.starts_with(VERTEX_INPUT_MAGIC));
        assert_eq!(decode(&vertex_bytes), Some((vertex.clone(), b"made by".to_vec())));
        vertex.indexed_forms.vertex_inputs[0].0.channel = 4;
        assert!(decode(&encode(&vertex, "made by")).is_none());
        let mut legacy = bytes.clone();
        legacy[..4].copy_from_slice(b"VSH8");
        assert!(decode(&legacy).is_none());
        let mut unaffected = shader();
        unaffected.indexed_forms = Default::default();
        let mut legacy = encode(&unaffected, "earlier");
        // VSH8 through VSHA have neither chain count preceding the empty
        // index list and trailing halfword-alignment metadata.
        let chains = legacy.len() - (8 * 4 + unaffected.halfword_aligned_roots.len() * 8);
        legacy.drain(chains..chains + 24);
        legacy[..4].copy_from_slice(b"VSH8");
        assert_eq!(decode(&legacy), Some((unaffected, b"earlier".to_vec())));
        assert_eq!(decode(&bytes), Some((host, b"made by".to_vec())));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode(&[bytes.as_slice(), &[0]].concat()), None);
        assert_eq!(decode(b"VSH0"), None);
        // Older entries omit shader binding requirements or the translator build.
        for version in b'1'..=b'7' {
            let mut old = bytes.clone();
            old[3] = version;
            assert_eq!(decode(&old), None);
        }
    }

    #[test]
    fn live_cache_keeps_legacy_code_until_the_native_warmer_publishes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [21; 32];
        let guest = b"a warmable shader";
        let make = |_: &[u8]| Ok(shader());
        let converter = Translate { translate: make, source: SOURCE, ran: AtomicBool::new(false) };
        let input = [&build[..], &guest[..]].concat();
        let key = CacheKey::new(&converter, &input);
        let _ = cached(&cache, &build, guest, make, false).unwrap();
        let mut old = shader();
        old.dxil = b"earlier native bytecode".to_vec();
        let mut bytes = encode(&old, "earlier translator");
        bytes[..4].copy_from_slice(b"VSHJ");
        cache.replace(&key, &bytes).unwrap();
        let pending = WarmReplacement { cache: cache.clone(), key, previous: blake3::hash(&bytes) };
        let live = || cached(&cache, &build, guest, make, false).unwrap();
        assert_eq!(live(), old);
        assert_eq!(std::fs::read(cache.path_for(&key)).unwrap(), bytes);
        pending.publish(&shader()).unwrap();
        assert_eq!(live(), shader());
    }

    /// An entry an earlier build of the translator made, under the same
    /// version: served as it is; when refreshing translated again in its
    /// place, and a different result counted.
    #[test]
    fn entries_of_an_earlier_translator_are_served_or_refreshed() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [9u8; 32];
        let container = b"a container of an earlier build".as_slice();
        let runs = AtomicU64::new(0);
        let old = || {
            let mut s = shader();
            s.dxil = b"the old translation".to_vec();
            s
        };
        let first = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(old())
        };
        assert_eq!(cached_by(&cache, &build, "build a", container, first, false), Ok(old()));
        let now = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        let (stale, refreshed, changed) = stale_counts();
        // The next build, same version: the old translation, not translated.
        assert_eq!(cached_by(&cache, &build, "build b", container, now, false), Ok(old()));
        assert_eq!(runs.load(Ordering::Relaxed), 1);
        // Refreshing: translated again and kept.
        assert_eq!(cached_by(&cache, &build, "build b", container, now, true), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);
        // Now the entry is this build's: served when refreshing too.
        assert_eq!(cached_by(&cache, &build, "build b", container, now, true), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);
        // (Other tests count too; these at least.)
        let (s, r, c) = stale_counts();
        assert!(s >= stale + 2 && r > refreshed && c > changed, "{s} {r} {c}");
    }
}
