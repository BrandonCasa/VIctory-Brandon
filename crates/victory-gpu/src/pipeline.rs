//! Pipeline states. Both descriptions start as on PC: the root signature,
//! then the shader stages (five for graphics, one for compute); streams
//! carry the rest of the graphics state as subobjects of the standard
//! types.
//!
//! Host pipelines are made on a pipeline's first draw, not at creation:
//! The title creates some 60,000 pipelines in its first 20
//! seconds and draws with a few dozen (decision 0007, step 3). Shader
//! containers are shared by content hash and translated once, on first
//! use (decision 0003).
//!
//! shortcut: only vertex plus pixel pipelines from streams and compute
//! pipelines get host pipelines; mesh, tessellation and geometry stages,
//! and the plain graphics description, keep their containers only. The 32-byte
//! packet after the header stays zero: titles only copy it inline, which
//! our null command-list limits keep from running.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::mem::offset_of;
use std::sync::{Arc, Mutex, OnceLock};

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, PIPELINE, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

const PIPELINE_IIDS: [Iid; 4] = [
    guid(
        0x765a30f3,
        0xf624,
        0x4c6f,
        [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
pub(crate) struct Bytecode {
    code: *const u8,
    size: usize,
}

#[repr(C)]
pub(crate) struct GraphicsHead {
    root: This,
    /// Vertex, pixel, domain, hull, geometry.
    stages: [Bytecode; 5],
}

#[repr(C)]
pub(crate) struct ComputeHead {
    root: This,
    stage: Bytecode,
}

/// A shader container a title passed, shared by every pipeline with the
/// same bytes.
pub(crate) struct Shader {
    pub(crate) stage: &'static str,
    /// The first 8 bytes of its BLAKE3, as logged.
    pub(crate) hash: String,
    container: Vec<u8>,
    /// Its part in the stand-in for the indirect light.
    #[cfg(windows)]
    pub(crate) ambient: Option<crate::ambient::Role>,
    /// Its part in the light probe (`crate::lightprobe`).
    #[cfg(windows)]
    pub(crate) probe: Option<crate::lightprobe::Role>,
    #[cfg(windows)]
    translated: OnceLock<Option<Translated>>,
}

impl std::fmt::Debug for Shader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.stage, self.hash)
    }
}

/// Every container seen, by full content hash.
static SHADERS: Mutex<Option<HashMap<[u8; 32], Arc<Shader>>>> = Mutex::new(None);

/// For the memory lines: containers kept and their bytes, translations
/// kept and the bytes of their host code. None of them is ever let go.
static KEPT: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// What the shader table and the pipeline library keep in memory:
/// containers and their bytes, translations and their bytes, the bytes of
/// the library's file (kept whole while the library lives) and what its
/// last save wrote.
#[cfg(windows)]
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
pub(crate) fn kept() -> [u64; 7] {
    use std::sync::atomic::Ordering::Relaxed;
    [
        KEPT[0].load(Relaxed),
        KEPT[1].load(Relaxed),
        KEPT[2].load(Relaxed),
        KEPT[3].load(Relaxed),
        library::BLOB.load(Relaxed),
        library::SAVED.load(Relaxed),
        library::TRIMMED_BYTES.load(Relaxed),
    ]
}

/// The shared container for `bytes`.
fn shader(stage: &'static str, bytes: &[u8]) -> Arc<Shader> {
    let full = blake3::hash(bytes);
    let mut shaders = SHADERS.lock().unwrap_or_else(|e| e.into_inner());
    shaders
        .get_or_insert_with(HashMap::new)
        .entry(*full.as_bytes())
        .or_insert_with(|| {
            KEPT[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            KEPT[1].fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
            Arc::new(Shader {
                stage,
                hash: full.to_hex()[..16].to_owned(),
                container: bytes.to_vec(),
                #[cfg(windows)]
                ambient: (stage == "compute" && crate::ambient::on()).then(|| crate::ambient::role(bytes)).flatten(),
                #[cfg(windows)]
                probe: crate::lightprobe::on().then(|| crate::lightprobe::role(bytes)).flatten(),
                #[cfg(windows)]
                translated: OnceLock::new(),
            })
        })
        .clone()
}

/// A translated shader and the console resources its code reads.
#[cfg(windows)]
pub(crate) struct Translated {
    pub(crate) host: victory_shader::emit::HostShader,
    /// A compute program's threads in a group.
    pub(crate) threads: Option<[u32; 3]>,
    pub(crate) used: Vec<victory_shader::UsedResource>,
    pub(crate) tables: crate::draw::TablePlan,
    pub(crate) layout: Arc<[u32]>,
    pub(crate) address_keys: Vec<(u8, u32)>,
    pub(crate) address_ordinals: Vec<Option<usize>>,
    /// Whether the shader's code can store to buffer memory ([`stores`]);
    /// true when its code is not known.
    pub(crate) stores: bool,
    /// Whether it can store through raw addresses ([`stores_by_address`]).
    pub(crate) stores_by_address: bool,
    /// Whether it stores through a view it picks from the heap by an index,
    /// once asked (`crate::picked`).
    pub(crate) picks: std::sync::atomic::AtomicU8,
}

#[cfg(windows)]
impl Shader {
    /// The translation, made on first use; a shader that does not
    /// translate is logged once.
    pub(crate) fn translated(&self) -> Option<&Translated> {
        self.translated
            .get_or_init(|| {
                let done = {
                    let _translation = crate::timeline::span("pipeline: translate a shader");
                    translate(&self.container)
                };
                // `VICTORY_SHADER_SAVE=<dir>`: each shader as the title gave it (`.cdxil`) and as
                // translated (`.dxil`), by the hash crash reports name it with
                if let Some(dir) = std::env::var_os("VICTORY_SHADER_SAVE") {
                    let dir = std::path::PathBuf::from(dir);
                    let _ = std::fs::create_dir_all(&dir);
                    let stem = format!("{}.{}", self.hash, self.stage);
                    let _ = std::fs::write(dir.join(format!("{stem}.cdxil")), &self.container);
                    if let Ok(t) = &done {
                        let _ = std::fs::write(dir.join(format!("{stem}.dxil")), &t.host.dxil);
                    }
                }
                if let Ok(t) = &done {
                    KEPT[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    KEPT[3].fetch_add(t.host.dxil.len() as u64, std::sync::atomic::Ordering::Relaxed);
                }
                match &done {
                    Ok(t) => tracing::info!(
                        stage = self.stage,
                        hash = %self.hash,
                        user_data = t.host.user_data.len(),
                        samplers = t.host.samplers.len(),
                        "shader translated"
                    ),
                    Err(e) => tracing::warn!(
                        stage = self.stage,
                        hash = %self.hash,
                        error = %e,
                        "shader does not translate; its pipelines' draws are dropped"
                    ),
                }
                done.ok()
            })
            .as_ref()
    }
}

#[cfg(windows)]
impl Shader {
    /// The console programs of the container; `None` if they are not known.
    pub(crate) fn programs(&self) -> Option<Vec<victory_shader::NativeProgram>> {
        use victory_shader::{Container, FourCC};
        let c = Container::parse(&self.container).ok()?;
        let part = c.part(FourCC::DXIL).or(c.part(FourCC::ACCC))?;
        victory_shader::native_programs(part.data).ok()
    }
}

#[cfg(windows)]
fn translate(container: &[u8]) -> Result<Translated, String> {
    use victory_shader::{Container, FourCC, HwStage};
    // The translation depends on it (a view index past a capped heap).
    crate::descriptor::host::probe_view_limit();
    let c = Container::parse(container).map_err(|e| e.to_string())?;
    let programs = match c.part(FourCC::DXIL).or(c.part(FourCC::ACCC)) {
        Some(part) => victory_shader::native_programs(part.data).map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    let host = crate::shader_cache::host_shader(container)?;
    let stores = programs.is_empty() || programs.iter().any(stores);
    let threads = programs.iter().find_map(|p| match &p.io {
        victory_shader::native::StageIo::Cs(cs) if cs.threads.iter().all(|&n| n > 0) => Some(cs.threads),
        _ => None,
    });
    let stores_by_address = programs.is_empty() || programs.iter().any(stores_by_address);
    let used = programs
        .into_iter()
        .find(|p| {
            matches!(
                p.stage,
                HwStage::Vs | HwStage::Ps | HwStage::Ms | HwStage::Cs
            )
        })
        .map(|p| p.used)
        .unwrap_or_default();
    let tables = crate::draw::TablePlan::new(&used, &host.root_indexed_views);
    let mut address_keys = Vec::new();
    let address_ordinals = host
        .user_data
        .iter()
        .map(|u| {
            let victory_shader::RootType::Other(kind) = u.root else {
                return None;
            };
            let low = victory_shader::emit::USER_VIEW_ADDRESS;
            if kind != low && kind != low + 1 {
                return None;
            }
            let key = (u.slot, u.dword);
            Some(
                address_keys
                    .iter()
                    .position(|k| *k == key)
                    .unwrap_or_else(|| {
                        address_keys.push(key);
                        address_keys.len() - 1
                    }),
            )
        })
        .collect();
    let mut layout = vec![host.user_data_dwords()];
    for slots in [&host.root_cbvs, &host.root_srvs, &host.root_uavs] {
        layout.push(slots.len() as u32);
        layout.extend(slots.iter().copied().map(u32::from));
    }
    for s in &host.static_samplers {
        layout.extend([
            s.filter,
            s.address[0],
            s.address[1],
            s.address[2],
            s.mip_lod_bias.to_bits(),
            s.max_anisotropy,
            s.comparison,
            s.border,
            s.min_lod.to_bits(),
            s.max_lod.to_bits(),
        ]);
    }
    Ok(Translated {
        layout: layout.into(),
        address_keys,
        address_ordinals,
        host,
        threads,
        used,
        tables,
        stores,
        stores_by_address,
        picks: Default::default(),
    })
}

/// Whether `shaders` are a pipeline of the structure builder.
#[cfg(windows)]
fn of_builder(shaders: &[Arc<Shader>]) -> bool {
    use victory_shader::{Container, FourCC};
    let [shader] = shaders else { return false };
    if shader.stage != "compute" {
        return false;
    }
    let Ok(c) = Container::parse(&shader.container) else {
        return false;
    };
    c.part(FourCC::DXIL)
        .and_then(|part| victory_shader::native_programs(part.data).ok())
        .is_some_and(|programs| programs.iter().any(victory_shader::spill::is_spilled))
}

/// `VICTORY_BVH_QUARANTINE=1`: what the structure builder's stages store
/// stays in the host's copies and is not copied back into title memory. A
/// check for a first run of the whole builder: a stage that stores where
/// it should not can then not reach the title's own objects (run 98), and
/// the programs that walk the structures read the host's copies anyway.
/// shortcut: not a way to run: the title also reads results back (sizes,
/// counters) and gets none, and a copy's pages are overwritten from title
/// memory when the title writes them.
#[cfg(windows)]
pub(crate) fn builder_quarantined(this: This) -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    is_builder(this) && *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_QUARANTINE").is_some_and(|v| v == "1"))
}

/// Whether the title's pipeline `this` is one of the structure builder's.
#[cfg(windows)]
pub(crate) fn is_builder(this: This) -> bool {
    !this.is_null() && pipeline(this).builder
}

#[cfg(windows)]
pub(crate) fn builder_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("VICTORY_BVH_BUILDER").is_none_or(|v| v != "0"))
}

/// Whether a console program can store to buffer memory: it has a buffer,
/// typed-buffer, scalar-memory, flat or global store or atomic, or code
/// that is not all known (an instruction of those kinds outside our table,
/// an indirect jump, code that does not decode). Stores to textures
/// (image instructions) and to the program's own scratch memory do not
/// count: neither reaches a buffer's host copy. After upstream #413,
/// which leaves scalar-memory stores out and counts image stores.
#[cfg(windows)]
fn stores(program: &victory_shader::NativeProgram) -> bool {
    use victory_shader::gfx10::{Encoding, decode};
    let Ok(code) = decode(&program.code) else {
        return true;
    };
    code.iter().any(|i| {
        let name = i.name();
        match i.encoding {
            Encoding::Mubuf
            | Encoding::Mtbuf
            | Encoding::Smem
            | Encoding::Flat
            | Encoding::Global => name.is_none_or(|n| {
                (n.contains("store") || n.contains("atomic")) && !n.contains("scratch")
            }),
            Encoding::Sop1 => name.is_none_or(|n| matches!(n, "s_setpc_b64" | "s_swappc_b64")),
            _ => false,
        }
    })
}

/// Whether a console program can store to memory through raw addresses
/// (`crate::memory`): a flat or global store or atomic, or code that does
/// not decode. The programs that only walk ray-tracing structures do not,
/// whatever they store through views.
#[cfg(windows)]
fn stores_by_address(program: &victory_shader::NativeProgram) -> bool {
    use victory_shader::gfx10::{Encoding, decode};
    let Ok(code) = decode(&program.code) else {
        return true;
    };
    code.iter().any(|i| {
        matches!(i.encoding, Encoding::Flat | Encoding::Global)
            && i.name()
                .is_none_or(|n| (n.contains("store") || n.contains("atomic")) && !n.contains("scratch"))
    })
}

#[repr(C)]
struct Pipeline {
    object: Object,
    packet: [u32; 8],
    /// Each used stage's container.
    shaders: Vec<Arc<Shader>>,
    /// Whether a list has bound it yet (its shaders are logged then).
    bound: std::sync::atomic::AtomicBool,
    /// What its host pipeline is made from, and the host pipeline once made
    /// (by a worker: the title may release the pipeline meanwhile).
    #[cfg(windows)]
    host: Arc<make::Pending>,
    #[cfg(windows)]
    topology_variant: OnceLock<Option<Arc<make::Pending>>>,
    /// One of the ray-tracing structure builder's pipelines.
    #[cfg(windows)]
    builder: bool,
}
const _: () = assert!(offset_of!(Pipeline, packet) == 0x10);

/// Stage names, in description order.
const GRAPHICS_STAGES: [&str; 5] = ["vertex", "pixel", "domain", "hull", "geometry"];

fn pipeline<'a>(this: This) -> &'a Pipeline {
    // SAFETY: the title passes one of our pipelines.
    unsafe { &*this.cast::<Pipeline>() }
}

/// List method: logs each pipeline's shaders the first time a list binds
/// it, so a run lists the shaders its frames need first.
pub(crate) fn bound(this: This) {
    if this.is_null() {
        return;
    }
    let p = pipeline(this);
    if p.bound.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let frame = crate::frame::current();
    for shader in &p.shaders {
        tracing::info!(pipeline = ?this, frame, stage = shader.stage, size = shader.container.len(), hash = %shader.hash, "first bind");
    }
}

/// The host pipeline of a title pipeline; `None` if one of its shaders does
/// not translate, the host refuses it, or it is still being made (its first
/// use starts that, see [`make`]).
#[cfg(windows)]
pub(crate) fn host_pipeline<'a>(this: This) -> Option<&'a host::HostPipeline> {
    if this.is_null() {
        return None;
    }
    make::ready(&pipeline(this).host)
}

/// Select the PSO for the topology actually recorded on this list. The other
/// variant owns an independent input layout and its own driver library key.
#[cfg(windows)]
pub(crate) fn draw_pipeline<'a>(this: This, rectangle: bool) -> Option<&'a host::HostPipeline> {
    make::ready(draw_recipe(this, rectangle)?)
}

#[cfg(windows)]
fn draw_recipe<'a>(this: This, rectangle: bool) -> Option<&'a Arc<make::Pending>> {
    if this.is_null() {
        return None;
    }
    let p = pipeline(this);
    let Some(g) = &p.host.graphics else {
        return Some(&p.host);
    };
    const RECTANGLE_TYPE: i32 = 19;
    if (g.topology.0 == RECTANGLE_TYPE) == rectangle {
        return Some(&p.host);
    }
    let alternate = p.topology_variant.get_or_init(|| {
        let mut bytes = Vec::new();
        g.put(&mut bytes);
        let mut g = host::GraphicsState::get(&bytes)?;
        g.topology.0 = if rectangle { RECTANGLE_TYPE } else { 3 };
        Some(canonical_pending(&p.shaders, Some(Box::new(g))))
    });
    alternate.as_ref()
}

#[cfg(windows)]
pub(crate) fn dependency(this: This, rectangle: Option<bool>) -> Option<Arc<make::Pending>> {
    if this.is_null() {
        return None;
    }
    let p = match rectangle {
        Some(r) => draw_recipe(this, r)?,
        None => &pipeline(this).host,
    };
    make::requested(p).then(|| p.clone())
}

#[cfg(windows)]
pub(crate) fn prefetch(this: This) {
    if !this.is_null() {
        make::prefetch(&pipeline(this).host)
    }
}

#[cfg(windows)]
fn canonical_pending(
    shaders: &[Arc<Shader>],
    graphics: Option<Box<host::GraphicsState>>,
) -> Arc<make::Pending> {
    type Recipe = (Vec<usize>, Option<Vec<u8>>);
    #[derive(Default)]
    struct Cache {
        live: HashMap<Recipe, std::sync::Weak<make::Pending>>,
        retained: std::collections::VecDeque<Arc<make::Pending>>,
    }
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let key = (
        shaders.iter().map(|s| Arc::as_ptr(s) as usize).collect(),
        graphics.as_ref().map(|g| {
            let mut bytes = Vec::new();
            g.put(&mut bytes);
            bytes
        }),
    );
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(p) = cache.live.get(&key).and_then(|p| p.upgrade()) {
        return p;
    }
    let pending = Arc::new(make::Pending::new(shaders.to_vec(), graphics));
    if cache.retained.len() == 512 {
        cache.retained.pop_front();
    }
    if cache.live.len() > 2 * cache.retained.len().max(512) && cache.live.len() % 256 == 0 {
        cache.live.retain(|_, p| p.strong_count() != 0);
    }
    cache.live.insert(key, Arc::downgrade(&pending));
    cache.retained.push_back(pending.clone());
    pending
}

/// Host pipelines queued and not made yet.
#[cfg(windows)]
pub(crate) fn making() -> u64 {
    make::waiting()
}

/// Draws and dispatches dropped since the last [`take_drops`], by reason
/// (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
static DROPS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// Shader hashes as printed in the pipeline diagnostics.
#[cfg(all(windows, feature = "diagnostics"))]
fn parse_skip_hashes(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
        .filter_map(|hash| {
            if hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                Some(hash.to_ascii_lowercase())
            } else {
                tracing::warn!(
                    hash,
                    "VICTORY_SKIP_PIPELINES expects 16-digit shader hashes"
                );
                None
            }
        })
        .collect()
}

#[cfg(all(windows, feature = "diagnostics"))]
fn skipped_shader(p: &Pipeline) -> Option<&str> {
    static HASHES: OnceLock<Vec<String>> = OnceLock::new();
    let hashes = HASHES.get_or_init(|| {
        let value = std::env::var("VICTORY_SKIP_PIPELINES").unwrap_or_default();
        parse_skip_hashes(&value)
    });
    if hashes.is_empty() {
        return None;
    }
    p.shaders
        .iter()
        .find(|shader| hashes.contains(&shader.hash))
        .map(|shader| shader.hash.as_str())
}

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn is_skipped(this: This) -> bool {
    !this.is_null() && skipped_shader(pipeline(this)).is_some()
}

/// Counts a draw or dispatch of `this` intentionally skipped or dropped
/// for want of a host pipeline (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn note_drop(this: This) {
    let why = if this.is_null() {
        Some("no pipeline set".to_owned())
    } else {
        let p = pipeline(this);
        skipped_shader(p)
            .map(|hash| format!("skipped shader {hash}"))
            .or_else(|| make::missing(&p.host))
    };
    if let Some(why) = why {
        note_why(why);
    }
}

/// Counts a draw or dispatch dropped for `why` (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn note_why(why: String) {
    crate::capture::op(0, || format!("DROPPED {why}"));
    let mut drops = DROPS.lock().unwrap_or_else(|e| e.into_inner());
    *drops.get_or_insert_with(HashMap::new).entry(why).or_default() += 1;
}

/// The drops counted since the last call, most first (diagnostics).
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn take_drops() -> Vec<(String, u64)> {
    let drops = DROPS.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut drops: Vec<(String, u64)> = drops.into_iter().flatten().collect();
    // Frame reports only show the first 40 reasons. Keep ties ordered so
    // a HashMap iteration order change cannot change which reasons appear.
    drops.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    drops
}

/// The stage's container, if it has one.
fn copy(stage: &'static str, code: &Bytecode) -> Option<Arc<Shader>> {
    if code.code.is_null() || code.size == 0 {
        return None;
    }
    // SAFETY: the title passes `size` bytes of shader code.
    Some(shader(stage, unsafe {
        std::slice::from_raw_parts(code.code, code.size)
    }))
}

fn hand_out(
    shaders: Vec<Arc<Shader>>,
    #[cfg(windows)] graphics: Option<Box<host::GraphicsState>>,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    tracing::trace!(?shaders, "pipeline state");
    #[cfg(windows)]
    let host = canonical_pending(&shaders, graphics);
    #[cfg(windows)]
    if known::on() {
        known::note(host.key(), &shaders, host.graphics.as_deref());
    }
    #[cfg(windows)]
    make::early(&host);
    #[cfg(windows)]
    let builder = of_builder(&shaders);
    let pipeline = Box::new(Pipeline {
        object: Object::new(&FUNCTIONS),
        packet: [0; 8],
        #[cfg(windows)]
        host,
        #[cfg(windows)]
        topology_variant: OnceLock::new(),
        #[cfg(windows)]
        builder,
        shaders,
        bound: std::sync::atomic::AtomicBool::new(false),
    });
    object::hand_out(PIPELINE, pipeline, iid, &PIPELINE_IIDS, out)
}

/// Device method.
pub(crate) extern "system" fn create_graphics(
    _: This,
    desc: *const GraphicsHead,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    let stages = GRAPHICS_STAGES.iter().zip(&desc.stages);
    let shaders = stages.filter_map(|(s, c)| copy(s, c)).collect();
    hand_out(
        shaders,
        #[cfg(windows)]
        None,
        iid,
        out,
    )
}

/// Device method.
pub(crate) extern "system" fn create_compute(
    _: This,
    desc: *const ComputeHead,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    hand_out(
        copy("compute", &desc.stage).into_iter().collect(),
        #[cfg(windows)]
        None,
        iid,
        out,
    )
}

#[cfg(windows)]
thread_local! {
    static COMPUTE_LOOKUP: std::cell::Cell<Option<(u64, *const ComputeHead)>> = const { std::cell::Cell::new(None) };
}

#[cfg(windows)]
static TITLE_LOOKUP: OnceLock<usize> = OnceLock::new();

/// Bridge for the identified retail compute wrapper, installed before title execution.
#[cfg(windows)]
pub fn cached_compute_wrapper(original: usize) -> Option<usize> {
    (*TITLE_LOOKUP.get_or_init(|| original) == original)
        .then_some(scoped_compute_lookup as *const () as usize)
}

#[cfg(windows)]
extern "system" fn scoped_compute_lookup(
    cache: usize,
    key: u64,
    root: This,
    desc: *const ComputeHead,
) -> This {
    let lookup: extern "system" fn(usize, u64, This) -> This =
        unsafe { std::mem::transmute(*TITLE_LOOKUP.get().unwrap()) };
    COMPUTE_LOOKUP.with(|scope| {
        let previous = scope.replace(Some((key, desc)));
        let result = lookup(cache, key, root);
        scope.set(previous);
        result
    })
}

pub(crate) extern "system" fn create_cached_compute_x(
    this: This,
    packed: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    if out.is_null() {
        return E_INVALIDARG;
    }
    unsafe { *out = std::ptr::null() };
    #[cfg(windows)]
    if !packed.is_null() && !iid.is_null() && PIPELINE_IIDS.contains(unsafe { &*iid })
        && let Some((key, desc)) = COMPUTE_LOOKUP.get()
        && let Some(desc) = unsafe { desc.as_ref() }
        && !desc.stage.code.is_null() && desc.stage.size > 0
        // The lookup compares node+10h to its key and passes node+18h to this method.
        && unsafe { packed.sub(8).cast::<u64>().read_unaligned() } == key
        && unsafe { packed.cast::<This>().read_unaligned() } == desc.root
    {
        return create_compute(this, desc, iid, out);
    }
    0x80004001u32 as HRESULT
}

/// Device method 0x1f0: the plain compute description plus console
/// extensions `(desc, extension count, extensions, iid, out)`.
/// shortcut: the extensions (console shader settings) are ignored, as is
/// the rest of the description.
pub(crate) extern "system" fn create_compute_x(
    this: This,
    desc: *const ComputeHead,
    _extensions: u32,
    _extended: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    create_compute(this, desc, iid, out)
}

/// A pipeline described as a stream: its size and the subobjects, each a
/// type then its data, padded to 8 bytes.
#[repr(C)]
pub(crate) struct StreamDesc {
    size: usize,
    stream: *const u8,
}

/// A subobject type's size (type included), and the stage it is if it is a
/// shader. The console numbers amplification and mesh shaders 23 and 24
/// (PC: 24 and 25); its own types from 25 on are not sized yet.
fn subobject(kind: u32) -> Option<(usize, Option<&'static str>)> {
    let stage = |name| Some((24, Some(name)));
    match kind {
        1 => stage("vertex"),
        2 => stage("pixel"),
        3 => stage("domain"),
        4 => stage("hull"),
        5 => stage("geometry"),
        6 => stage("compute"),
        23 => stage("amplification"),
        24 => stage("mesh"),
        0 => Some((16, None)),
        12 | 19 => Some((24, None)),
        7 | 15 => Some((40, None)),
        8 => Some((336, None)),
        10 => Some((48, None)),
        11 => Some((56, None)),
        17 => Some((16, None)),
        21 => Some((64, None)),
        22 => Some((32, None)),
        9 | 13 | 14 | 16 | 18 | 20 => Some((8, None)),
        _ => None,
    }
}

/// Device method: a pipeline from a stream; the shaders are kept, and the
/// graphics state for the host pipeline.
pub(crate) extern "system" fn create_stream(
    _: This,
    desc: *const StreamDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    let Some(recipe) = stream_recipe(desc) else {
        return E_INVALIDARG;
    };
    recipe.create(iid, out)
}

struct StreamRecipe {
    shaders: Vec<Arc<Shader>>,
    #[cfg(windows)]
    graphics: Box<host::GraphicsState>,
    #[cfg(windows)]
    cache_root: Option<blake3::Hash>,
}

impl StreamRecipe {
    fn create(self, iid: *const Iid, out: *mut This) -> HRESULT {
        hand_out(
            self.shaders,
            #[cfg(windows)]
            Some(self.graphics),
            iid,
            out,
        )
    }
}

fn stream_recipe(desc: *const StreamDesc) -> Option<StreamRecipe> {
    let desc = unsafe { desc.as_ref()? };
    if desc.stream.is_null() || desc.size > 1 << 20 {
        return None;
    }
    #[cfg(windows)]
    let bytes = {
        let mut bytes = vec![0; desc.size];
        if !crate::host::read_into(desc.stream, &mut bytes) {
            return None;
        }
        bytes
    };
    #[cfg(not(windows))]
    let bytes = unsafe { std::slice::from_raw_parts(desc.stream, desc.size) };
    let mut shaders = Vec::new();
    #[cfg(windows)]
    let mut graphics = Box::new(host::GraphicsState::default());
    #[cfg(windows)]
    let (mut root, mut represented) = (None, true);
    let mut at = 0;
    while at < bytes.len() {
        let kind = u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?);
        let Some((size, stage)) = subobject(kind) else {
            // Preserve ordinary creation's prefix fallback. It cannot be used
            // to certify equivalence with a packed seed.
            #[cfg(windows)]
            {
                represented = false;
            }
            break;
        };
        let sub = bytes.get(at..at + size)?;
        if let Some(stage) = stage {
            let code = unsafe { sub.as_ptr().add(8).cast::<Bytecode>().read_unaligned() };
            if code.size != 0 {
                if code.code.is_null() || code.size > 64 << 20 {
                    return None;
                }
                #[cfg(windows)]
                {
                    let mut container = vec![0; code.size];
                    if !crate::host::read_into(code.code, &mut container) {
                        return None;
                    }
                    shaders.push(shader(stage, &container));
                }
                #[cfg(not(windows))]
                shaders.extend(copy(stage, &code));
            }
        }
        #[cfg(windows)]
        {
            if kind == 0 {
                root = crate::root::digest(unsafe {
                    sub.as_ptr().add(8).cast::<This>().read_unaligned()
                });
            }
            if matches!(kind, 7 | 22) || matches!(kind, 18 | 20) && sub[4..8] != [0; 4] {
                represented = false;
            }
            if !unsafe { graphics.read(kind, sub.as_ptr()) } {
                return None;
            }
        }
        at += size;
    }
    Some(StreamRecipe {
        shaders,
        #[cfg(windows)]
        graphics,
        #[cfg(windows)]
        cache_root: if represented { root } else { None },
    })
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &PIPELINE_IIDS).then_some(this);
    object::answer(PIPELINE, iid, found, out)
}
pub(crate) extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Pipeline>(this)
}

/// IUnknown 3, object 4, device child 1, pipeline 3.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<PIPELINE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    Table(f)
};

#[cfg(windows)]
pub(crate) mod host;
#[cfg(windows)]
pub mod known;
#[cfg(windows)]
mod library;
#[cfg(windows)]
pub(crate) mod make;
#[cfg(windows)]
pub mod title_cache;

/// Lets the pipeline library's mapped file go from memory when no pipeline
/// has loaded for a while (`library::trim`); called by the host's sweep.
#[cfg(windows)]
pub(crate) fn trim_library() {
    library::trim();
}

/// The pipeline library's part of the report when the host device is
/// removed (`library::removal_note`).
#[cfg(windows)]
pub(crate) fn library_removal_note() -> String {
    library::removal_note()
}

/// Keeps compiled host pipelines, and the list of title pipelines to start
/// early, in `dir` (a folder of the cache) across runs.
#[cfg(windows)]
pub fn use_pipeline_cache(dir: std::path::PathBuf) {
    library::use_pipeline_cache(dir);
}

/// Saves the pipeline cache now if anything was added (at the end of a run).
#[cfg(windows)]
pub fn save_pipeline_cache() {
    library::save(true);
    known::flush();
}

#[cfg(test)]
mod tests {
    #[cfg(all(windows, feature = "diagnostics"))]
    #[test]
    fn skip_hashes_accepts_full_hashes_only() {
        assert!(super::parse_skip_hashes(" , ").is_empty());
        assert_eq!(
            super::parse_skip_hashes(" 0123456789ABCDEF, 0123456789abcde, xyz, 0011223344556677,"),
            ["0123456789abcdef", "0011223344556677"]
        );
    }

    use super::*;
    use crate::object::{S_OK, slot};

    fn shaders(p: This) -> Vec<(&'static str, Vec<u8>)> {
        pipeline(p)
            .shaders
            .iter()
            .map(|s| (s.stage, s.container.clone()))
            .collect()
    }

    #[test]
    fn pipelines_keep_their_shaders() {
        let (vs, ps) = ([1u8, 2, 3], [4u8; 5]);
        let none = || Bytecode {
            code: std::ptr::null(),
            size: 0,
        };
        let desc = GraphicsHead {
            root: std::ptr::null(),
            stages: [
                Bytecode {
                    code: vs.as_ptr(),
                    size: vs.len(),
                },
                Bytecode {
                    code: ps.as_ptr(),
                    size: ps.len(),
                },
                none(),
                none(),
                none(),
            ],
        };
        let mut p = std::ptr::null();
        assert_eq!(
            create_graphics(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p),
            S_OK
        );
        assert_eq!(
            shaders(p),
            [("vertex", vs.to_vec()), ("pixel", ps.to_vec())]
        );
        // A second pipeline with the same pixel shader shares its container.
        let mut q = std::ptr::null();
        create_graphics(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut q);
        assert!(Arc::ptr_eq(
            &pipeline(p).shaders[1],
            &pipeline(q).shaders[1]
        ));
        assert_eq!(slot::<extern "system" fn(This) -> u32>(p, 0x10)(p), 0);
    }

    /// The stream layout the title builds (title+0x86f090): root
    /// signature, five shader slots (the first a mesh shader), blend, sample
    /// mask, rasterizer, depth-stencil 1, sample description, input layout,
    /// topology, render target and depth formats: 0x2b0 bytes.
    #[test]
    fn streams_yield_their_shaders() {
        let (ms, ps) = ([9u8; 7], [4u8; 5]);
        let mut stream = vec![0u8; 0x2b0];
        let mut put = |at: usize, v: u64| stream[at..at + 8].copy_from_slice(&v.to_le_bytes());
        for (at, kind) in [
            (0x10, 24),
            (0x28, 2),
            (0x40, 3),
            (0x58, 4),
            (0x70, 5),
            (0x88, 8),
            (0x1d8, 9),
            (0x1e0, 10),
            (0x210, 21),
            (0x250, 17),
            (0x260, 12),
            (0x278, 14),
            (0x280, 15),
            (0x2a8, 16),
        ] {
            put(at, kind);
        }
        put(0x18, ms.as_ptr() as u64);
        put(0x20, ms.len() as u64);
        put(0x30, ps.as_ptr() as u64);
        put(0x38, ps.len() as u64);
        // Topology type 3 (triangles), one render target of format 28.
        put(0x278, 14 | 3 << 32);
        put(0x280, 15 | 28 << 32);
        put(0x2a0, 1 << 32);
        let desc = StreamDesc {
            size: stream.len(),
            stream: stream.as_ptr(),
        };
        let mut p = std::ptr::null();
        let hr = create_stream(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p);
        assert_eq!(hr, S_OK);
        assert_eq!(shaders(p), [("mesh", ms.to_vec()), ("pixel", ps.to_vec())]);
        #[cfg(windows)]
        {
            let g = pipeline(p).host.graphics.as_deref().unwrap();
            assert_eq!(g.topology.0, 3);
            assert_eq!(
                (g.targets.NumRenderTargets, g.targets.RTFormats[0].0),
                (1, 28)
            );
        }
    }
}
