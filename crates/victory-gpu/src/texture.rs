//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory was written since the last detiling
//! ([`crate::watch`]), in write-watched memory; in physical-page views,
//! which cannot be watched, a content hash of the whole memory at every
//! submission that samples it.
//!
//! shortcut: a change detiles the whole texture, not the pages written.
//! Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); plane 0 is uploaded
//! (every mip and array slice), depth and stencil planes are not.

pub(crate) mod ahead;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    // Ahead jobs submit this buffer directly. Ordinary submissions retain immutable
    // upload versions, so keep their detiled source in CPU-cacheable memory.
    storage: Mutex<StageStorage>,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Layout,
    /// Mips per array slice, of the title's texture.
    mips: u32,
    /// The first of them that is read and uploaded
    /// (`crate::resource::held_only`), the one the host copy begins at, and
    /// the bytes of title memory read.
    first: u32,
    level0: u32,
    read: usize,
    /// Which host copy it uploads into, and what that is told to hold.
    serial: u64,
    holds: Arc<crate::resource::Holds>,
    /// The staging buffer's bytes (counted in `STAGED` while it lives).
    bytes: u64,
    /// The title texture's shape, for `crate::texstore`.
    shape: crate::texstore::Shape,
    memory: *const u8,
    last: Mutex<Last>,
    parts: Mutex<Vec<StagedPart>>,
    /// Whether the staging buffer was detiled since a list's prelude last
    /// uploaded it, and whether one has uploaded it at all: a list's prelude
    /// records `CopyTextureRegion` at submission time (after `refresh()`)
    /// only when `!uploaded || dirty`.
    dirty: std::sync::atomic::AtomicBool,
    uploaded: std::sync::atomic::AtomicBool,
    /// When a list last recorded a use of it (`crate::host::moment`).
    used: std::sync::atomic::AtomicU64,
    /// Levels on demand (`crate::tiles`): the texture's host copy was made
    /// without memory of its own when the staging buffer was made.
    on_demand: bool,
    sparse_image: Option<std::sync::Weak<crate::resource::HostTexture>>,
    linear_null: bool,
    native_null: std::sync::atomic::AtomicBool,
    null_levels: std::sync::atomic::AtomicU32,
    /// For such a texture: the levels that were the title's at the last
    /// detiling (a bit a level; the staging buffer holds zeros for the
    /// others), and what that was judged by ([`Staging::judgement`]).
    detiled: std::sync::atomic::AtomicU32,
    judged: [std::sync::atomic::AtomicU64; 2],
    /// For such a texture: when an upload last left a held level without
    /// tiles (`crate::host::moment`, never 0; 0: none did). The upload is
    /// recorded again about a second later if the card has room by then
    /// ([`upload`]).
    waiting: std::sync::atomic::AtomicU64,
}

enum StageStorage {
    Cpu(Vec<Vec<u8>>),
    Upload {
        buffer: ID3D12Resource,
        base: u64,
        data: *mut u8,
        piece: Option<crate::pool::StagingPiece>,
    },
}

impl StageStorage {
    fn allocated(&self, footprint: u64) -> u64 {
        match self {
            Self::Cpu(parts) => parts.iter().map(|p| p.len() as u64).sum(),
            Self::Upload { .. } => footprint,
        }
    }
}

#[derive(Clone, Default)]
struct StagedPart {
    content: Option<(Digest, bool)>,
    dirty: bool,
    /// Version holding the latest detile pending queue submission, or zero.
    pending: u64,
    /// Immutable native bytes for this exact content, retained across a
    /// rejected occurrence or a later tile mapping until the copy lands.
    captured: Option<UploadSlice>,
}

impl StagedPart {
    fn resolve_upload(&mut self, version: u64, sent: bool) {
        if self.pending == version {
            self.pending = 0;
            if !sent { self.dirty = self.content.is_some(); }
        }
    }
}

struct UploadMemory {
    buffer: ID3D12Resource,
    _piece: Option<crate::pool::StagingPiece>,
}

impl UploadMemory {
    fn allocate(size: u64) -> Option<(Arc<Self>, u64, *mut u8)> {
        let host = crate::host::host()?;
        let (piece, buffer, base, data) = match crate::pool::staging(host, size, crate::pool::StagingUse::Submission) {
            Some((piece, buffer, base, data)) => (Some(piece), buffer, base, data),
            None => match host.upload_buffer(size) {
                Some((buffer, data)) => (None, buffer, 0, data),
                None => {
                    crate::host::refuse_frame(format_args!("could not retain {size} texture upload bytes"));
                    return None;
                }
            },
        };
        Some((Arc::new(Self { buffer, _piece: piece }), base, data))
    }
}

#[derive(Clone)]
struct UploadSlice {
    memory: Arc<UploadMemory>,
    offset: u64,
}

struct UploadVersion {
    id: u64,
    staging: std::sync::Weak<Staging>,
    sent: std::sync::atomic::AtomicBool,
    parts: Vec<(usize, UploadSlice)>,
}

impl Drop for UploadVersion {
    fn drop(&mut self) {
        if self.sent.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        if let Some(staging) = self.staging.upgrade() {
            let mut parts = staging.parts.lock().unwrap_or_else(|e| e.into_inner());
            for &(i, _) in &self.parts {
                if let Some(p) = parts.get_mut(i) {
                    p.resolve_upload(self.id, false);
                }
            }
            staging
                .dirty
                .store(parts.iter().any(|p| p.dirty), std::sync::atomic::Ordering::Release);
        }
    }
}

/// What a texture's staging buffer holds: the hash of the memory detiled
/// (memory not watched), or the write-watch asking it was detiled after.
#[derive(Default)]
struct Last {
    hash: Option<Digest>,
    previous: Previous,
    backing: Vec<(usize, usize, usize, u64, u64)>,
    mapping_epoch: Option<u64>,
    guarded_epoch: Option<u64>,
    mapping_busy: bool,
    asked: u64,
    /// The frame a partly mapped texture was last read in.
    partial: Option<u64>,
}

impl Last {
    fn release_previous(&mut self) {
        #[cfg(feature = "diagnostics")]
        let owner = self.previous.owner;
        self.previous = Previous::default();
        #[cfg(feature = "diagnostics")]
        { self.previous.owner = owner; }
        self.hash = None;
    }
}

#[derive(Default)]
struct Previous {
    bytes: Vec<u8>,
    /// Guest ranges and their offsets in the compact payload. Empty for a
    /// complete contiguous capture (or no bytes).
    ranges: Vec<(std::ops::Range<usize>, usize)>,
    #[cfg(feature = "diagnostics")]
    owner: SnapshotOwner,
    #[cfg(feature = "diagnostics")]
    omitted: usize,
}

impl Previous {
    fn get(&self, guest: std::ops::Range<usize>) -> Option<&[u8]> {
        if self.ranges.is_empty() {
            return self.bytes.get(guest);
        }
        let i = self.ranges.partition_point(|(r, _)| r.end <= guest.start);
        let (r, at) = self.ranges.get(i)?;
        if guest.start < r.start || guest.end > r.end { return None; }
        self.bytes.get(at + (guest.start - r.start)..at + (guest.end - r.start))
    }

    fn capture(&mut self, tiled: &[u8], mut runs: Option<Vec<std::ops::Range<usize>>>) {
        self.ranges.clear();
        let mut len = 0;
        if let Some(runs) = &mut runs {
            runs.sort_unstable_by_key(|r| r.start);
            for r in runs.drain(..) {
                if let Some((last, _)) = self.ranges.last_mut()
                    && last.end >= r.start
                {
                    let end = last.end.max(r.end);
                    len += end - last.end;
                    last.end = end;
                } else {
                    self.ranges.push((r.clone(), len));
                    len += r.len();
                }
            }
        } else {
            len = tiled.len();
        }
        #[cfg(feature = "diagnostics")]
        let before = self.bytes.capacity();
        if self.bytes.capacity() != len {
            self.bytes = Vec::new();
            self.bytes.reserve_exact(len);
        }
        self.bytes.clear();
        if runs.is_some() {
            for (r, _) in &self.ranges {
                self.bytes.extend_from_slice(&tiled[r.clone()]);
            }
        } else {
            self.bytes.extend_from_slice(tiled);
        }
        #[cfg(feature = "diagnostics")]
        {
            let omitted = tiled.len() - len;
            if self.omitted != omitted {
                SPARSE_OMITTED_BYTES.fetch_add((omitted as u64).wrapping_sub(self.omitted as u64), std::sync::atomic::Ordering::Relaxed);
                self.omitted = omitted;
            }
        }
        #[cfg(feature = "diagnostics")]
        if self.bytes.capacity() != before {
            use std::sync::atomic::Ordering::Relaxed;
            let change = (self.bytes.capacity() as u64).wrapping_sub(before as u64);
            PREVIOUS_BYTES.fetch_add(change, Relaxed);
            SNAPSHOT_OWNERS[self.owner as usize].fetch_add(change, Relaxed);
        }
    }
}

#[cfg(feature = "diagnostics")]
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SnapshotOwner {
    #[default]
    NoPhysicalView,
    Movie,
    Partial,
    Unique,
    Shared,
    ManyViews,
}

#[cfg(feature = "diagnostics")]
pub(crate) static PREVIOUS_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "diagnostics")]
pub(crate) static SPARSE_OMITTED_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "diagnostics")]
pub(crate) static SNAPSHOT_OWNERS: [std::sync::atomic::AtomicU64; 6] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 6];

#[cfg(feature = "diagnostics")]
impl Previous {
    fn set_owner(&mut self, owner: SnapshotOwner) {
        use std::sync::atomic::Ordering::Relaxed;
        if self.owner != owner {
            let bytes = self.bytes.capacity() as u64;
            SNAPSHOT_OWNERS[self.owner as usize].fetch_sub(bytes, Relaxed);
            SNAPSHOT_OWNERS[owner as usize].fetch_add(bytes, Relaxed);
            self.owner = owner;
        }
    }
}

#[cfg(feature = "diagnostics")]
impl Drop for Previous {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        let bytes = self.bytes.capacity() as u64;
        PREVIOUS_BYTES.fetch_sub(bytes, Relaxed);
        SNAPSHOT_OWNERS[self.owner as usize].fetch_sub(bytes, Relaxed);
        SPARSE_OMITTED_BYTES.fetch_sub(self.omitted as u64, Relaxed);
    }
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

/// The buffer a texture the host writes is copied through into the texture
/// of a view the title placed in its memory in another format
/// ([`alias`]): every level of every layer as each of the two formats lays
/// it out for a copy (the same bytes: the texels are as wide), the written
/// texture's layout first.
struct Through {
    serials: [u64; 2],
    buffer: ID3D12Resource,
    parts: Vec<(
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    )>,
}




// SAFETY: a device-local buffer only lists record copies with.
unsafe impl Send for Through {}
// SAFETY: as above.
unsafe impl Sync for Through {}

/// The copy buffers of [`alias`], by the placed view's texture.
static THROUGH: Mutex<Option<HashMap<usize, Arc<Through>>>> = Mutex::new(None);

/// Brings `placed`, the texture of a view the title placed in the memory
/// of `source` in another format (`crate::resource::alias_of`), up to date
/// with what the host wrote to `source`: the texels of every level and
/// layer are copied as they are, through a buffer (the host copies between
/// two formats of one texel width in no other way). Recorded the first time
/// the list reads the view
/// and again whenever the list used `source` in another way since (wrote
/// it, or read it itself). Leaves `placed` in the copy destination state,
/// as [`upload`] does. False if the copy cannot be made: the caller fills
/// the texture from title memory, as before.
pub(crate) fn alias(list: &HostList, placed: This, source: This) -> bool {
    const TARGET_OR_UNORDERED: u32 = 0x4 | 0x8;
    const COPY_DEST: u32 = 0x400;
    const COPY_SOURCE: u32 = 0x800;
    // (All of its levels: the copy is level by level.)
    crate::resource::want_level(placed, 0);
    let (Some(reader), Some(writer)) = (
        crate::resource::host_copy(placed),
        crate::resource::host_copy(source),
    ) else {
        return false;
    };
    if list.alias_current(writer.serial, reader.serial) {
        return true;
    }
    let Some(through) = alias_layout(placed, source, &reader, &writer) else { return false };
    let to = &reader.texture;
    list.first_use(placed as usize);
    // The source's first use in the list records nothing (its prelude
    // brings it to the state); a use in another way since the last copy
    // shows as a change of state.
    let before = list.transition(source, COPY_SOURCE).map(|(texture, before)| {
        let state = crate::host::legal_state(texture, COPY_SOURCE);
        let barrier = crate::host::transition(
            texture,
            D3D12_RESOURCE_STATES(before as i32),
            D3D12_RESOURCE_STATES(state as i32),
        );
        // SAFETY: an open list and a live texture.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
        before
    });
    card_writes(list, placed, to, None);
    crate::list::ensure_state_access(list, placed, COPY_DEST, false);
    list.native_alias_changed(reader.serial);
    #[cfg(feature = "diagnostics")]
    list.note_op(|| format!("alias copy of {source:?} into {placed:?}"));
    through.record(&list.list, &writer.texture, to);
    list.keep(&through.buffer);
    list.alias_copied(writer.serial, reader.serial);
    // A draw that also writes the source (a view of it handled before
    // this one) needs it in that state again; the next read of the alias
    // then copies once more, as it must after a write.
    if let Some(before) = before
        && before & TARGET_OR_UNORDERED != 0
    {
        crate::list::ensure_state_access(list, source, before, false);
        list.native_alias_changed(writer.serial);
    }
    true
}

pub(crate) struct SampleAlias {
    pub(crate) writer: Arc<crate::resource::HostTexture>,
    pub(crate) reader: Arc<crate::resource::HostTexture>,
    through: Arc<Through>,
}

impl SampleAlias {
    pub(crate) fn new(placed: This, source: This, reader: &Arc<crate::resource::HostTexture>) -> Option<Self> {
        let writer = crate::resource::host_copy(source)?;
        if reader.base != 0 || reader.tiled.is_some() || writer.serial == reader.serial
            || crate::resource::twin(source).is_some() || crate::resource::twin(placed).is_some() { return None; }
        let through = alias_layout(placed, source, reader, &writer)?;
        Some(Self { writer, reader: reader.clone(), through })
    }

    /// Temporarily move both images from their original queue states, perform
    /// the format reinterpretation, and restore them for the ordinary prelude.
    pub(crate) fn record(&self, list: &ID3D12GraphicsCommandList, states: [u32; 2]) {
        let mut before = Vec::new();
        let mut after = Vec::new();
        if self.reader.pooled {
            // The whole reader is overwritten. Activate its placed allocation
            // before transitioning it out of the state tracked by the queue.
            before.push(D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                        pResourceBefore: std::mem::ManuallyDrop::new(None),
                        pResourceAfter: unsafe { std::mem::transmute_copy(&self.reader.texture) },
                    }),
                },
            });
        }
        for ((image, copy_state), old) in [(&self.writer, D3D12_RESOURCE_STATE_COPY_SOURCE),
            (&self.reader, D3D12_RESOURCE_STATE_COPY_DEST)].into_iter().zip(states) {
            let real = D3D12_RESOURCE_STATES(old as i32);
            if real != copy_state {
                before.push(crate::host::transition(&image.texture, real, copy_state));
                after.push(crate::host::transition(&image.texture, copy_state, real));
            }
        }
        unsafe { if !before.is_empty() { list.ResourceBarrier(&before); } }
        self.through.record(list, &self.writer.texture, &self.reader.texture);
        unsafe { if !after.is_empty() { list.ResourceBarrier(&after); } }
    }
}

fn alias_layout(placed: This, source: This, reader: &crate::resource::HostTexture,
                writer: &crate::resource::HostTexture) -> Option<Arc<Through>> {
    let (to, from) = (&reader.texture, &writer.texture);
    let Some(host) = host() else { return None };
    // A linear host copy is exact only if guest addressing assigns every
    // corresponding element the same bytes, including packed tails. BC works
    // here when the writer stores one encoded block per integer element.
    let Some((a, _, _)) = crate::resource::title_texture(source) else { return None };
    let placed_layout = crate::resource::placed_view_layout(
        crate::resource::texture_memory(source)?, crate::resource::texture_memory(placed)?, true,
    )?;
    let [plane] = placed_layout.planes.as_slice() else { return None };
    let b = &plane.layout;
    if a.desc.swizzle != b.desc.swizzle || a.desc.element_bytes != b.desc.element_bytes
        || a.desc.samples != 1 || b.desc.samples != 1 || a.desc.dimension != b.desc.dimension
        || a.mips != b.mips || a.block != b.block || a.slice_size != b.slice_size || a.slices != b.slices
        || crate::resource::stands_in(source) || crate::resource::stands_in(placed)
    {
        return None;
    }
    let serials = [writer.serial, reader.serial];
    let through = {
        let mut map = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
        let map = map.get_or_insert_with(HashMap::new);
        match map.get(&(placed as usize)).filter(|t| t.serials == serials) {
            Some(through) => through.clone(),
            None => {
                // SAFETY: live textures.
                let (of_from, of_to) = unsafe { (from.GetDesc(), to.GetDesc()) };
                // Levels times layers (a volume's slices are one part).
                let count = |d: &D3D12_RESOURCE_DESC| {
                    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
                        1
                    } else {
                        u32::from(d.DepthOrArraySize)
                    };
                    u32::from(d.MipLevels) * layers
                };
                let parts = count(&of_from);
                if parts == 0 || parts != count(&of_to) {
                    return None;
                }
                let mut rows = [(); 2]
                    .map(|()| vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); parts as usize]);
                let mut bytes = [0u64; 2];
                let mut row_counts = [(); 2].map(|()| vec![0u32; parts as usize]);
                let mut row_sizes = [(); 2].map(|()| vec![0u64; parts as usize]);
                // SAFETY: plain queries; the out pointers are ours and
                // hold `parts` layouts each.
                unsafe {
                    host.device.GetCopyableFootprints(
                        &of_from,
                        0,
                        parts,
                        0,
                        Some(rows[0].as_mut_ptr()),
                        Some(row_counts[0].as_mut_ptr()),
                        Some(row_sizes[0].as_mut_ptr()),
                        Some(&mut bytes[0]),
                    );
                    host.device.GetCopyableFootprints(
                        &of_to,
                        0,
                        parts,
                        0,
                        Some(rows[1].as_mut_ptr()),
                        Some(row_counts[1].as_mut_ptr()),
                        Some(row_sizes[1].as_mut_ptr()),
                        Some(&mut bytes[1]),
                    );
                }
                let alike = rows[0].iter().zip(&rows[1]).all(|(a, b)| {
                    let (fa, fb) = (a.Footprint, b.Footprint);
                    a.Offset == b.Offset
                        && (fa.Depth, fa.RowPitch) == (fb.Depth, fb.RowPitch)
                });
                if bytes[0] == 0 || bytes[0] != bytes[1] || row_counts[0] != row_counts[1] || row_sizes[0] != row_sizes[1] || !alike {
                    return None;
                }
                let Some(buffer) = host.uav_buffer(bytes[0]) else {
                    return None;
                };
                crate::host::name(&buffer, || {
                    format!(
                        "alias copy {}x{} f{} to f{}",
                        of_from.Width, of_from.Height, of_from.Format.0, of_to.Format.0
                    )
                });
                tracing::info!(
                    view = ?placed,
                    texture = ?source,
                    width = of_from.Width,
                    height = of_from.Height,
                    parts,
                    written_as = of_from.Format.0,
                    read_as = of_to.Format.0,
                    "a view placed in the memory of a texture the host writes, in another format: read from that texture by a copy on the host"
                );
                let [of_written, of_read] = rows;
                let through = Arc::new(Through {
                    serials,
                    buffer,
                    parts: of_written.into_iter().zip(of_read).collect(),
                });
                map.insert(placed as usize, through.clone());
                through
            }
        }
    };
    Some(through)
}

impl Through {
    fn record(&self, list: &ID3D12GraphicsCommandList, from: &ID3D12Resource, to: &ID3D12Resource) {
        let part = |texture: &ID3D12Resource, index: usize| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: index as u32,
            },
        };
        let rows = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&self.buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *row,
            },
        };
        let (common, read, written) = (
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        // SAFETY: an open list and live resources; the buffer is as big as
        // the footprints and rests in the common state between copies.
        unsafe {
            list
                .ResourceBarrier(&[crate::host::transition(&self.buffer, common, written)]);
            for (index, (of_written, _)) in self.parts.iter().enumerate() {
                list
                    .CopyTextureRegion(&rows(of_written), 0, 0, 0, &part(from, index), None);
            }
            list
                .ResourceBarrier(&[crate::host::transition(&self.buffer, written, read)]);
            for (index, (_, of_read)) in self.parts.iter().enumerate() {
                list
                    .CopyTextureRegion(&part(to, index), 0, 0, 0, &rows(of_read), None);
            }
            list
                .ResourceBarrier(&[crate::host::transition(&self.buffer, read, common)]);
        }
    }
}

/// Whether a texture refresh that finds the runtime remapping title memory
/// goes on without waiting for it (`VICTORY_TEXTURE_NO_WAIT=1`; off by
/// default).
///
/// A texture whose memory is all mapped is read in place, under a lock
/// the runtime takes alone while it changes a mapping
/// (`crate::watch::title_memory_reading`), so that no page goes away under
/// the read. While the world streams in the runtime remaps all the time,
/// and each of the frame's 150-360 refreshes waited for it: runs 99-101
/// have 4, 15 and 16 frame lines with over 300 ms of "textures", up to
/// 3,708 ms, and on every one of them the wait for this lock is the whole
/// of it (1,345 of 1,381 ms; 3,630 of 3,708; the write-watch askings 3-12
/// ms, the texture's own lock 0).
///
/// With the switch the refresh asks for the lock without waiting. If the
/// runtime has it, the texture is read through the kernel instead, which
/// needs no lock: it fails if a page is not there, and then the texture is
/// left as it is and read at its next refresh, a frame later. A read
/// through the kernel costs about 0.6 ms a megabyte against 0.3 in place.
///
/// What changes for the picture: a texture that arrives while the runtime
/// is remapping may show a frame later. A staging buffer that holds nothing
/// yet is filled with what is there all the same ([`fill_unlocked`]): a list
/// has recorded the upload from it by then.
fn no_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_NO_WAIT").is_some_and(|v| v == "1"))
}

/// Since the last frame-rate line, of texture refreshes on every thread, in
/// microseconds: waiting for the texture's own lock (another thread is
/// refreshing the same texture), asking the write watch and the guards,
/// waiting for title memory to hold still (the lock the runtime takes
/// alone while it remaps, `crate::watch::title_memory_reading`), and the
/// refreshes in all.
///
/// While the world streams in, the frame line's "textures" is seconds a
/// frame in some runs and tenths in others for the same megabytes (2,566
/// ms for 110 MB in run 93, 314 ms for 132 MB in run 91; before these
/// counters, runs 81, 83, 85 and 94 each had lines of 1.5-2.9 s), and the
/// submitting thread is not running while it lasts (run 93: 240-310 ms of
/// its own time a frame beside 1,100-1,800 ms of "textures"): it waits.
/// These say for what.
pub(crate) static REFRESH_WAITS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// `f`'s result, its time added to [`REFRESH_WAITS`]' place `slot`.
fn waited<T>(slot: usize, f: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let out = f();
    REFRESH_WAITS[slot].fetch_add(started.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
    out
}

struct ReadScratch {
    bytes: Vec<u8>,
    next_trim: u64,
}

/// Total allocated capacity across live texture readers, including idle workers.
#[cfg(feature = "diagnostics")]
pub(crate) static READ_SCRATCH_CAPACITY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "diagnostics")]
impl Drop for ReadScratch {
    fn drop(&mut self) {
        if self.bytes.capacity() != 0 && crate::copystat::memory_categories() {
            READ_SCRATCH_CAPACITY.fetch_sub(self.bytes.capacity() as u64, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

thread_local! {
    /// Room to read a texture's memory into: one buffer per submitting
    /// thread. Each texture kept its own, a second copy in memory of every
    /// write-watched or partly mapped texture ever sampled, for the run.
    static READ: std::cell::RefCell<ReadScratch> = const {
        std::cell::RefCell::new(ReadScratch { bytes: Vec::new(), next_trim: 0 })
    };
}

/// Release an idle worker's largest read buffer under host memory pressure.
/// The read callback has already filled the owned staging and snapshot bytes.
pub(crate) fn trim_idle_read_scratch() {
    const MIN_CAPACITY: usize = 8 << 20;
    const TRIM_INTERVAL_MS: u64 = 10_000;
    if !crate::residency::pressured() {
        return;
    }
    READ.with(|read| {
        let mut scratch = read.borrow_mut();
        let released = scratch.bytes.capacity();
        if released < MIN_CAPACITY {
            return;
        }
        let now = crate::host::coarse_ms();
        if now < scratch.next_trim {
            return;
        }
        scratch.next_trim = now.saturating_add(TRIM_INTERVAL_MS);
        drop(std::mem::take(&mut scratch.bytes));
        #[cfg(feature = "diagnostics")]
        if crate::copystat::memory_categories() {
            use std::sync::atomic::Ordering::Relaxed;
            let remaining = READ_SCRATCH_CAPACITY.fetch_sub(released as u64, Relaxed) - released as u64;
            tracing::info!(released_bytes = released, remaining_bytes = remaining, "released idle texture read scratch");
        }
    });
}

/// Calls `f` with `size` bytes of this thread's read buffer (their
/// contents are whatever the last texture left).
fn with_read<R>(size: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    READ.with(|read| {
        let mut scratch = read.borrow_mut();
        let read = &mut scratch.bytes;
        if read.len() < size {
            #[cfg(feature = "diagnostics")]
            let before = read.capacity();
            read.resize(size, 0);
            #[cfg(feature = "diagnostics")]
            if read.capacity() != before && crate::copystat::memory_categories() {
                READ_SCRATCH_CAPACITY.fetch_add((read.capacity() - before) as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }
        f(&mut read[..size])
    })
}

/// Calls `f` with the `size` bytes of title memory at `memory`, read
/// through the kernel into this thread's read buffer; pages that are not
/// there read as zeros. The bytes are those a read in place would see.
#[cfg(test)]
fn with_title<R>(memory: *const u8, size: usize, f: impl FnOnce(&[u8]) -> R) -> R {
    with_read(size, |read| {
        crate::host::copy_mapped(read.as_mut_ptr(), memory, size, false);
        f(read)
    })
}

/// Staging buffers alive (also those only a list still holds) and their
/// bytes.
static STAGED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Claimed footprint bytes covered by recorded views, by late views only,
/// outside both, or without a known view, followed by bytes sent to the queue.
#[cfg(feature = "diagnostics")]
static UPLOAD_VIEW_BYTES: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

#[cfg(feature = "diagnostics")]
pub(crate) fn upload_view_bytes() -> [u64; 5] {
    UPLOAD_VIEW_BYTES.each_ref().map(|bytes| bytes.load(std::sync::atomic::Ordering::Relaxed))
}

/// The bytes of staging buffers alive ([`crate::residency`]).
pub(crate) fn live_staging_bytes() -> u64 {
    STAGED[1].load(std::sync::atomic::Ordering::Relaxed)
}

impl Drop for Staging {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        STAGED[0].fetch_sub(1, Relaxed);
        let storage = self.storage.get_mut().unwrap_or_else(|e| e.into_inner());
        STAGED[1].fetch_sub(storage.allocated(self.bytes), Relaxed);
    }
}

/// For the memory line: staging buffers alive, their bytes, and how many
/// of them their texture still has (the others wait for a list's reset).
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 4] {
    use std::sync::atomic::Ordering::Relaxed;
    let kept = STAGING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |map| map.len() as u64);
    [STAGED[0].load(Relaxed), STAGED[1].load(Relaxed), kept, PREVIOUS_BYTES.load(Relaxed)]
}

/// Staging buffers let go as idle since the start, and their bytes.
static LET_GO: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// For the memory line: staging buffers let go as idle, and their bytes.
#[cfg(feature = "diagnostics")]
pub(crate) fn let_go() -> [u64; 2] {
    use std::sync::atomic::Ordering::Relaxed;
    [LET_GO[0].load(Relaxed), LET_GO[1].load(Relaxed)]
}

/// Lets go of the staging buffers no list recorded a use of for `IDLE_MS`
/// and `IDLE_FRAMES` frames and no list holds: a staging buffer is system memory the size of its
/// texture, kept so that an unchanged texture is not uploaded again, and
/// those of textures no longer sampled were kept for the run (2,246 of
/// them, 2.1 GB, at the end of run 24). The next list that samples such a
/// texture makes a staging buffer anew, detiles and uploads once more.
/// Called before a submission starts, by the thread that submits.
pub(crate) fn let_go_idle() {
    use std::sync::atomic::Ordering::Relaxed;
    const IDLE_FRAMES: u64 = 4;
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = map.as_mut() else {
        return;
    };
    if map.is_empty() {
        return;
    }
    let idle_ms = crate::host::ram_idle_ms(crate::host::available_physical_ram());
    // A list that recorded an upload or a check holds the staging until it
    // is reset: one only the table holds is named by no list, recorded or
    // running.
    let (mut went, mut bytes) = (0u64, 0u64);
    map.retain(|_, s| {
        let keep = Arc::strong_count(s) > 1
            || !crate::host::long_ago(s.used.load(Relaxed), idle_ms, IDLE_FRAMES);
        if !keep {
            LET_GO[0].fetch_add(1, Relaxed);
            let allocated = s.storage.lock().unwrap_or_else(|e| e.into_inner()).allocated(s.bytes);
            LET_GO[1].fetch_add(allocated, Relaxed);
            went += 1;
            bytes += allocated;
        }
        keep
    });
    if went > 0 {
        crate::host::trail(|| format!("sweep: {went} staging buffers of textures let go ({} KiB), held by no list", bytes >> 10));
    }
}

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    let new = {
        let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        set.get_or_insert_with(Default::default).insert(resource as usize)
    };
    // Another placed view can cover the same allocation through a different
    // Xbox VA. Keep every live owner, including equal-base, different-size views.
    if new && let Some((at, desc)) = crate::resource::texture_memory(resource) {
        // SAFETY: 56 bytes in the description's layout.
        let words = unsafe { std::ptr::from_ref(&desc).cast::<[u32; 14]>().read_unaligned() };
        let bytes = crate::resource::placed_bytes(&words).unwrap_or(0);
        if !at.is_null() && bytes > 0 {
            let mut writers = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
            MAX_WRITTEN_BYTES.fetch_max(bytes, std::sync::atomic::Ordering::Relaxed);
            writers.insert((at as u64, resource as usize), (bytes, words[4], words[6], words[7], words[8]));
            crate::resource::aliases_changed();
        }
    }
}

/// The title memory of textures the host copied into, by address: bytes
/// and description words 4, 6, 7 and 8 (width, height, layers and levels,
/// format).
static HOST_WRITTEN_AT: Mutex<std::collections::BTreeMap<(u64, usize), (u64, u32, u32, u32, u32)>> =
    Mutex::new(std::collections::BTreeMap::new());
static MAX_WRITTEN_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Physical ranges of a placed texture may overlap an ordinary texture
/// filled by a GPU copy or resolve, including textures without target flags.
pub(crate) fn copy_writer_overlaps(ranges: &[(usize, usize)]) -> bool {
    visit_copy_writers(ranges, |_, _, _| true)
}

pub(crate) fn copy_writer_intersections(ranges: &[(usize, usize)]) -> Vec<(usize, u64, u64)> {
    let mut intersections = Vec::new();
    visit_copy_writers(ranges, |owner, lo, hi| {
        intersections.push((owner, lo, hi));
        false
    });
    intersections
}

fn visit_copy_writers(ranges: &[(usize, usize)], mut visit: impl FnMut(usize, u64, u64) -> bool) -> bool {
    let writers = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
    let max = MAX_WRITTEN_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    ranges.iter().any(|&(lo, hi)| {
        let (lo, hi) = (lo as u64, hi as u64);
        writers.range((lo.saturating_sub(max), 0)..(hi, 0))
            .any(|(&(base, owner), &(bytes, ..))| {
                let end = base.saturating_add(bytes).min(hi);
                end > lo && visit(owner, base.max(lo), end)
            })
    })
}

/// A texture the title placed at `address` (description words `desc`) was
/// made: says so once if it lies in the memory of a texture the host
/// copied into ([`host_written`]). Its own host copy is filled from title
/// memory, which the copy never reached: the copied texels are on the
/// card in the other texture, where no shader that reads this view finds
/// them.
pub(crate) fn placed_in_host_written(address: u64, desc: &[u32; 14]) {
    let hit = {
        let map = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
        map.range(..=(address, usize::MAX))
            .next_back()
            .filter(|(key, d)| address < key.0.saturating_add(d.0))
            .map(|(key, d)| (key.0, *d))
    };
    if let Some((at, (bytes, width, height, layers_levels, format))) = hit {
        static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            tracing::warn!(
                view_address = format_args!("{address:#x}"),
                view_offset = address - at,
                view_width = desc[4],
                view_height = desc[6],
                view_layers_levels = format_args!("{:#x}", desc[7]),
                view_format = desc[8],
                copied_address = format_args!("{at:#x}"),
                copied_bytes = bytes,
                copied_width = width,
                copied_height = height,
                copied_layers_levels = format_args!("{layers_levels:#x}"),
                copied_format = format,
                "HERO LEAD: a placed texture view lies in the memory of a texture the host copied into: it is filled from title memory, which the copy never reached"
            );
        }
    }
}

/// Whether the host GPU wrote `resource` by a copy or resolve
/// ([`host_written`]): its host copy cannot be made again from title
/// memory.
pub(crate) fn host_wrote(resource: usize) -> bool {
    HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&resource))
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = set.as_mut() {
        set.remove(&resource);
    }
    // Its memory may be given to other textures now (run 177: most of the
    // "HERO LEAD" lines were views in memory a released texture had held).
    let mut writers = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
    let before = writers.len();
    writers.retain(|&(_, owner), _| owner != resource);
    if writers.len() != before { crate::resource::aliases_changed(); }
    drop(writers);
    let mut through = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(through) = through.as_mut() {
        through.remove(&resource);
    }
}

/// Registers, once per list, `resource`'s staging buffer so that at submission
/// time its memory is refreshed and, if changed (or never uploaded), copied
/// into its host texture on the list's prelude command list.
pub(crate) fn upload(list: &HostList, resource: This, _texture: &ID3D12Resource) {
    upload_from(list, resource, 0);
}

thread_local! {
    /// The thread records uploads for a draw that bakes from them.
    pub(crate) static BAKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The thread records the upload of a texture picked by index, of which
    /// that many bytes were found mapped just now (`u64::MAX`: not asked).
    pub(crate) static PICKING: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// A 128-bit digest of `bytes`, to tell a texture's memory changed: four
/// lanes of folded multiplications over 64 bytes at a time. (Not a
/// cryptographic one, which took 0.27 ms a megabyte, four times the detiling
/// it was to spare; nobody makes up textures to collide.)
pub(crate) fn digest(bytes: &[u8]) -> Digest {
    PrefixDigest::new().prefix(bytes)
}

/// State shared by successively longer prefixes of one immutable byte slice.
pub(crate) struct PrefixDigest {
    lanes: [u64; 4],
    consumed: usize,
}

impl PrefixDigest {
    const K: [u64; 8] = [
        0xa076_1d64_78bd_642f,
        0xe703_7ed1_a0b4_28db,
        0x8ebc_6af0_9c88_c6e3,
        0x5899_65cc_7537_4cc3,
        0x1d8e_4e27_c47d_124f,
        0xeb44_acca_b455_d165,
        0x9e37_79b9_7f4a_7c15,
        0xc2b2_ae3d_27d4_eb4f,
    ];
    pub(crate) fn new() -> Self {
        Self {
            lanes: [Self::K[0], Self::K[1], Self::K[2], Self::K[3]],
            consumed: 0,
        }
    }

    fn fold(a: u64, b: u64) -> u64 {
        let p = u128::from(a) * u128::from(b);
        p as u64 ^ (p >> 64) as u64
    }

    fn step(lanes: &mut [u64; 4], block: &[u8; 64]) {
    let word = |b: &[u8]| {
        let mut w = [0; 8];
        w[..b.len()].copy_from_slice(b);
        u64::from_le_bytes(w)
    };
        for (i, lane) in lanes.iter_mut().enumerate() {
            let a = word(&block[16 * i..16 * i + 8]);
            let b = word(&block[16 * i + 8..16 * i + 16]);
            // (What went in is added again: a product with a factor of zero
            // forgets neither the lane nor the block.)
            *lane = Self::fold(a ^ Self::K[4 + i], b ^ *lane)
                .wrapping_add(a.rotate_left(23) ^ lane.rotate_left(41))
                ^ b;
        }
    }

    pub(crate) fn prefix(&mut self, bytes: &[u8]) -> Digest {
        let mut blocks = bytes[self.consumed..].chunks_exact(64);
        for block in &mut blocks {
            Self::step(&mut self.lanes, block.try_into().unwrap());
            self.consumed += 64;
        }
        let rest = blocks.remainder();
        let mut tail = [0u8; 64];
        tail[..rest.len()].copy_from_slice(rest);
        let mut lanes = self.lanes;
        Self::step(&mut lanes, &tail);
        let len = bytes.len() as u64;
        let low = Self::fold(lanes[0] ^ Self::K[1], lanes[1] ^ len)
            ^ Self::fold(lanes[2] ^ Self::K[3], lanes[3] ^ Self::K[5]);
        let high = Self::fold(lanes[0].rotate_left(32) ^ Self::K[6], lanes[2] ^ len)
            ^ Self::fold(lanes[1].rotate_left(32) ^ Self::K[7], lanes[3] ^ Self::K[2]);
        Digest(u128::from(low) << 64 | u128::from(high))
    }
}

/// See [`digest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Digest(pub(crate) u128);

/// [`upload`] of the levels from `first` on, the ones a view shows: of a
/// texture the title placed there may be no memory of the larger ones, and
/// what lies there is another texture's. (After victory-amd 4436f8f1.)
pub(crate) fn upload_from(list: &HostList, resource: This, first: u32) {
    upload_view(list, resource, first, None);
}

pub(crate) fn upload_view(list: &HostList, resource: This, first: u32, view: Option<&crate::descriptor::View>) -> bool {
    if host_wrote(resource as usize) { return false; }
    let required = view.filter(|_| PICKING.get().is_none());
    let Some(copy) = crate::resource::host_copy(resource) else {
        if required.is_some() { list.refused_command(0, "required texture has no native image"); }
        return false;
    };
    let mut prepared = false;
    let upload = sampled_upload(resource, first, &copy, |staging| {
        // This precise mip range already has an upload, or is about to acquire one.
        prepared = true;
        if !list.first_use((1usize << 63) | Arc::as_ptr(staging) as usize) { return false; }
        list.first_use(resource as usize);
        true
    });
    if let Some(upload) = upload { list.defer_texture(upload); }
    if let Some(view) = required { list.require_texture(resource as usize, copy.serial, view); }
    prepared
}

/// Prepare the sampled upload for the precise native image retained by a binding.
pub(crate) fn sampled_upload(
    resource: This,
    first: u32,
    copy: &Arc<crate::resource::HostTexture>,
    take: impl FnOnce(&Arc<Staging>) -> bool,
) -> Option<PreludeUpload> {
    sampled_upload_from(resource, first, copy, false, take)
}

/// Keep this upload within the view's console mip prefix when an adjacent
/// placement owns the excluded large mips. Existing wider recordings retain
/// their original staging and immutable uploads.
pub(crate) fn sampled_upload_from(
    resource: This, first: u32, copy: &Arc<crate::resource::HostTexture>, narrow: bool,
    take: impl FnOnce(&Arc<Staging>) -> bool,
) -> Option<PreludeUpload> {
    let first = if narrow || crate::resource::held_only() { first } else { 0 };
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_wrote(resource as usize) { return None; }
    ahead::listed(copy);
    let staging = staging_for(resource, copy, first, false, narrow)?;
    // Each allocation and mip range has its own retained staging object.
    // A speculative attempt that failed above never claims a required upload.
    if !take(&staging) { return None; }
    use std::sync::atomic::Ordering::Relaxed;
    staging.used.store(crate::host::moment(), Relaxed);
    // Levels on demand: the last upload left a held level without tiles
    // (the card had no room, or no heap could be had), and nothing else
    // would record an upload again until the title writes the texture: the
    // level stayed empty, the texture blurred or black. Asked again about
    // once a second, and only when the card has room; the staging buffer
    // still holds the level's bytes.
    if staging.on_demand {
        let since = staging.waiting.load(Relaxed);
        if since != 0 && crate::host::long_ago(since, 1000, 2) {
            if crate::resource::room_lately() {
                staging.waiting.store(0, Relaxed);
                staging.dirty.store(true, Relaxed);
                crate::tiles::STATS[10].fetch_add(1, Relaxed);
            } else {
                staging.waiting.store(crate::host::moment().max(1), Relaxed);
            }
        }
    }
    // At submission time, `staging.refresh()` runs first (in parallel via
    // `Batch::refresh_front` or `Batch::refresh_textures`), and only then does
    // `PreludeUpload::finish_refresh` decide whether the list's prelude must
    // record `CopyTextureRegion`. This guarantees zero frames of latency when
    // title memory changes (including 3-buffer Bink video rings) and eliminates
    // the race with `VICTORY_JOIN_PARTS` / `VICTORY_THIN`.
    let on_demand = crate::resource::OnDemand::for_copy(copy);
    Some(PreludeUpload {
        staging,
        copy: copy.clone(),
        partial_timer: crate::resource::partial_timer(resource),
        resource: resource as usize,
        version: None,
        on_demand,
    })
}

/// Pure claim helper for [`PreludeUpload::finish_refresh`]: returns
/// `Some(first_upload)` when the host texture has never been uploaded or the
/// staging buffer was detiled since the last upload, and `None` when the host
/// texture already holds the staging buffer's current contents.
fn claim_upload(
    uploaded: &std::sync::atomic::AtomicBool,
    dirty: &std::sync::atomic::AtomicBool,
) -> Option<bool> {
    use std::sync::atomic::Ordering::AcqRel;
    dirty
        .swap(false, AcqRel)
        .then(|| !uploaded.swap(true, AcqRel))
}

/// Logical subresources this particular shader view can address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ViewRange {
    mip: u32,
    levels: u32,
    slice: u32,
    slices: u32,
}

impl ViewRange {
    pub(crate) fn mip(&self) -> u32 { self.mip }

    /// Console storage the shader may address. Packed-tail mips share their
    /// tile, and a volume mip can occupy several non-adjacent storage slabs.
    pub(crate) fn storage_runs(&self, layout: &victory_formats::texture::Layout) -> Option<Vec<std::ops::Range<usize>>> {
        let mut runs = Vec::new();
        let mips = u32::try_from(layout.mips.len()).ok()?;
        for slice in self.slice..layout.slices.min(self.slice.saturating_add(self.slices)) {
            for mip in self.mip..mips.min(self.mip.saturating_add(self.levels)) {
                runs.extend(layout.subresource_runs(mip, slice)?);
            }
        }
        if runs.is_empty() { return None; }
        runs.sort_unstable_by_key(|r| r.start);
        let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
        for run in runs {
            if let Some(last) = merged.last_mut() && run.start <= last.end {
                last.end = last.end.max(run.end);
            } else { merged.push(run); }
        }
        Some(merged)
    }

    fn first_missing(&self, mips: u32, layers: u32, mut ready: impl FnMut(Self) -> bool) -> Option<Self> {
        if self.mip >= mips || self.slice >= layers || self.levels == 0 || self.slices == 0 {
            return Some(*self);
        }
        for slice in self.slice..layers.min(self.slice.saturating_add(self.slices)) {
            for mip in self.mip..mips.min(self.mip.saturating_add(self.levels)) {
                let part = Self { mip, levels: 1, slice, slices: 1 };
                if !ready(part) { return Some(part); }
            }
        }
        None
    }

    pub(crate) fn includes(&self, mip: u32, slice: u32) -> bool {
        (self.mip..self.mip.saturating_add(self.levels)).contains(&mip)
            && (self.slice..self.slice.saturating_add(self.slices)).contains(&slice)
    }

    pub(crate) fn for_view(view: &crate::descriptor::View) -> Option<Self> {
        use crate::descriptor::View;
        let whole = Self {
            mip: 0,
            levels: u32::MAX,
            slice: 0,
            slices: u32::MAX,
        };
        let (d, writable) = match view {
            View::Shader(_, Some(d)) => (d, false),
            View::Unordered(_, _, Some(d)) => (d, true),
            View::Shader(_, None) | View::Unordered(_, _, None) => return Some(whole),
            _ => return None,
        };
        if writable {
            let (slice, slices) = match d[1] {
                2 | 4 | 8 => (0, 1),
                3 | 5 => (d[3], d[4]),
                _ => return None,
            };
            return Some(Self {
                mip: d[2],
                levels: 1,
                slice,
                slices,
            });
        }
        let (mip, levels) = if matches!(d[1], 6 | 7) {
            (0, 1)
        } else {
            (d[4], d[5])
        };
        let (slice, slices) = match d[1] {
            2 | 4 | 6 | 8 => (0, 1),
            3 | 5 => (d[6], d[7]),
            7 => (d[4], d[5]),
            9 => (0, 6),
            10 => (d[6], d[7].saturating_mul(6)),
            _ => return None,
        };
        Some(Self {
            mip,
            levels,
            slice,
            slices,
        })
    }
}

/// Several recorded bindings can bring different mip ranges to the same
/// native image. A view is ready when every addressed mip and slice is ready.
pub(crate) fn missing_view(uploads: &[&PreludeUpload], view: ViewRange) -> Option<ViewRange> {
    if uploads.iter().any(|u| u.covers(view)) { return None; }
    let Some(first) = uploads.first() else { return Some(view) };
    let s = &first.staging;
    let layers = s.footprints.len() as u32 / (s.mips - s.first);
    let missing = view.first_missing(s.mips, layers, |part| uploads.iter().any(|u| u.covers(part)));
    if let Some(part) = missing {
        for upload in uploads {
            let s = &upload.staging;
            let source = s.memory as usize;
            let captured = part.mip.checked_sub(s.first).and_then(|mip| {
                let i = (part.slice * (s.mips - s.first) + mip) as usize;
                s.parts.lock().unwrap_or_else(|e| e.into_inner()).get(i).and_then(|p| p.content)
            });
            #[cfg(feature = "diagnostics")]
            if crate::switch::on("VICTORY_TRACE_TEXTURE_OWNER") {
                let mapping = crate::watch::title_memory_reading_now();
                let mapping_stable = mapping.is_some();
                let spans = s.layout.subresource_runs(part.mip, part.slice).map(|runs| {
                    runs.map(|r| required_backing_span(source + r.start, r.len(), mapping_stable))
                        .collect::<Vec<_>>()
                });
                drop(mapping);
                tracing::warn!(image = s.serial, format = s.shape[2], memory = format_args!("{source:#x}"), ?part, first = s.first,
                    base = s.level0, ?captured, mapping_stable, ?spans, desc = ?s.layout.desc, "required texture backing");
                continue;
            }
            let spans = s.layout.subresource_runs(part.mip, part.slice).map(|runs| runs.map(|r| {
                let start = source + r.start;
                (format!("{start:#x}"), r.len(), mapped_bytes(start as *const u8, r.len()))
            }).collect::<Vec<_>>());
            tracing::warn!(image = s.serial, format = s.shape[2], memory = format_args!("{source:#x}"), ?part, first = s.first,
                base = s.level0, ?captured, ?spans, desc = ?s.layout.desc, "required texture backing");
        }
    }
    missing
}

#[cfg(feature = "diagnostics")]
fn required_backing_span(start: usize, len: usize, mapping_stable: bool) -> String {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};

    let shown = crate::inplace::shown_bytes(start, len);
    let backing = crate::inplace::backing(start, len);
    let sparse = crate::inplace::sparse_range(start, len);
    let sparse_null = mapping_stable.then(|| crate::inplace::sparse_null_range(start, len));
    let mapped = mapped_bytes(start as *const u8, len);
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the query only fills `info`, even if the title address is inaccessible.
    let queried = unsafe { VirtualQuery(start as *const _, &mut info, size_of_val(&info)) } != 0;
    let region = queried.then(|| format!("state={:#x} protect={:#x} allocation={:#x} base={:#x} size={:#x}",
        info.State, info.Protect, info.AllocationBase as usize, info.BaseAddress as usize, info.RegionSize));
    format!("address={start:#x} bytes={len} mapped={mapped} shown={shown:?} sparse={sparse} sparse_null={sparse_null:?} region={region:?} backing={backing:?}")
}

/// A title texture deferred to submission time so `staging.refresh()` runs
/// before deciding whether `HostList::prelude` must record `CopyTextureRegion`.
#[derive(Clone)]
pub(crate) struct PreludeUpload {
    pub(crate) staging: Arc<Staging>,
    pub(crate) copy: Arc<crate::resource::HostTexture>,
    partial_timer: Option<Arc<std::sync::atomic::AtomicU64>>,
    resource: usize,
    version: Option<Arc<UploadVersion>>,
    on_demand: Option<crate::resource::OnDemand>,
}

impl PreludeUpload {
    pub(crate) fn sent(&self) {
        if let Some(v) = &self.version {
            v.sent.store(true, std::sync::atomic::Ordering::Release);
            #[cfg(feature = "diagnostics")]
            if crate::copystat::memory_categories() {
                let bytes = v.parts.iter().map(|(i, _)| self.staging.part_bytes(*i)).sum();
                UPLOAD_VIEW_BYTES[4].fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
            }
            let mut parts = self.staging.parts.lock().unwrap_or_else(|e| e.into_inner());
            for &(i, _) in &v.parts {
                if let Some(part) = parts.get_mut(i) {
                    part.resolve_upload(v.id, true);
                }
            }
            // The queue retains its immutable upload through the fence. Once
            // all contents land, the staging cache no longer needs a retry copy.
            let landed_sparse = self.staging.sparse_image.is_some() && self.copy.tiled.as_ref().is_some_and(|tiles|
                tiles.without(self.staging.detiled.load(std::sync::atomic::Ordering::Acquire)) == 0);
            if (!self.staging.on_demand || landed_sparse) && parts.iter().all(|p| !p.dirty && p.pending == 0) {
                for part in &mut *parts { part.captured = None; }
                let mut storage = self.staging.storage.lock().unwrap_or_else(|e| e.into_inner());
                if let StageStorage::Cpu(bytes) = &mut *storage {
                    STAGED[1].fetch_sub(bytes.iter().map(|p| p.len() as u64).sum(), std::sync::atomic::Ordering::Relaxed);
                    *bytes = Vec::new();
                }
            }
        }
    }

    /// The title memory the texture is read from: address and bytes.
    pub(crate) fn memory(&self) -> (usize, usize) {
        self.staging.memory()
    }

    pub(crate) fn covers(&self, needed: ViewRange) -> bool {
        let stages = &self.staging;
        // The lookahead refresh may yield to the streaming thread. A required
        // draw must capture the bytes once the pending remap has completed.
        if stages.last.lock().unwrap_or_else(|e| e.into_inner()).mapping_busy {
            stages.refresh_timed(true);
        }
        let mips = stages.mips;
        let layers = stages.footprints.len() as u32 / (mips - stages.first);
        if needed.mip < stages.first
            || needed.mip >= mips
            || needed.slice >= layers
            || needed.levels == 0
            || needed.slices == 0
        {
            return false;
        }
        if stages.native_null.load(std::sync::atomic::Ordering::Acquire)
            && self.copy.tiled.as_ref().is_some_and(|tiles| tiles.given_bytes() == 0) {
            return true;
        }
        let mut parts = stages.parts.lock().unwrap_or_else(|e| e.into_inner());
        let null_levels = self.copy.tiled.as_ref().map_or(0, |tiles|
            tiles.without(stages.null_levels.load(std::sync::atomic::Ordering::Acquire)));
        let ready = (needed.slice..layers.min(needed.slice.saturating_add(needed.slices))).all(|slice| {
            (needed.mip..mips.min(needed.mip.saturating_add(needed.levels))).all(|mip| {
                let i = (slice * (mips - stages.first) + mip - stages.first) as usize;
                null_levels & (1 << mip) != 0 || parts
                    .get(i)
                    .is_some_and(|p| matches!(p.content, Some((_, true))))
            })
        });
        if !ready { return false; }
        if let Some(tiled) = self.on_demand.as_ref().and_then(|entry| entry.tiled()) {
            let mask = (needed.mip..mips.min(needed.mip.saturating_add(needed.levels)))
                .filter(|&mip| null_levels & (1 << mip) == 0)
                .fold(0, |mask, mip| mask | tiled.with_level(mip - stages.level0));
            let missing = tiled.without(mask);
            if missing != 0 {
                let held = stages.detiled.load(std::sync::atomic::Ordering::Relaxed);
                tiled.uploaded(&self.copy.texture, held | mask, || true);
                if tiled.without(mask) != 0 { return false; }
                for (i, part) in parts.iter_mut().enumerate() {
                    let mip = stages.first + i as u32 % (mips - stages.first) - stages.level0;
                    if missing & (1u32 << mip) != 0 && part.content.is_some_and(|(_, held)| held) {
                        part.dirty = true;
                    }
                }
                stages.dirty.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        true
    }

    pub(crate) fn resource(&self) -> usize {
        self.resource
    }

    /// Compare the claimed native footprint with the views known to this
    /// list. An uncovered part may still be sampled by a different list.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_claim_views(&self, recorded: &[ViewRange], has_late_views: bool, late_contains: impl Fn(u32, u32) -> bool) {
        let Some(version) = &self.version else { return };
        let mut bytes = [0u64; 4];
        for (i, _) in &version.parts {
            let (mip, slice) = self.staging.part(*i);
            let class = if recorded.iter().any(|view| view.includes(mip, slice)) { 0 }
                else if late_contains(mip, slice) { 1 }
                else if !recorded.is_empty() || has_late_views { 2 }
                else { 3 };
            bytes[class] += self.staging.part_bytes(*i);
        }
        for (counter, bytes) in UPLOAD_VIEW_BYTES.iter().zip(bytes) {
            if bytes != 0 { counter.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed); }
        }
    }

    /// Called on the submitting thread after `self.staging.refresh()` has
    /// completed for this submission. Returns `Some(first_upload)` when the
    /// list's prelude must record `CopyTextureRegion` into `self.copy.texture`.
    pub(crate) fn finish_refresh(&self) -> Option<(Self, bool)> {
        use std::sync::atomic::Ordering::Relaxed;
        let mut parts = self.staging.parts.lock().unwrap_or_else(|e| e.into_inner());
        if !self
            .staging
            .dirty
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return None;
        }
        let mut ranges = Vec::new();
        let mut size = 0u64;
        let mut storage = self.staging.storage.lock().unwrap_or_else(|e| e.into_inner());
        for (i, part) in parts.iter().enumerate().filter(|(_, p)| p.dirty) {
            let fp = &self.staging.footprints[i];
            let len = self.staging.part_bytes(i);
            let offset = size.next_multiple_of(512);
            if part.captured.is_none() {
                if let StageStorage::Cpu(blocks) = &*storage
                    && blocks.get(i).is_none_or(|b| b.len() != len as usize) { return None; }
                size = offset.checked_add(len)?;
            }
            ranges.push((i, offset, fp.Offset, len));
        }
        if ranges.is_empty() { return None; }
        // Retries can refer directly to the same native bytes. Allocate once
        // for just the subresources that were detiled into fresh CPU blocks.
        let fresh = if size == 0 { None } else { Some(UploadMemory::allocate(size)?) };
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let first_upload = claim_upload(&self.staging.uploaded, &self.staging.dirty)?;
        let id = NEXT.fetch_add(1, Relaxed);
        let mut claimed = Vec::with_capacity(ranges.len());
        for (i, offset, src, len) in ranges {
            let slice = if let Some(slice) = &parts[i].captured {
                slice.clone()
            } else {
                let (memory, base, data) = fresh.as_ref().expect("allocated uncaptured subresources");
                let source = match &*storage {
                    StageStorage::Cpu(blocks) => blocks[i].as_ptr(),
                    StageStorage::Upload { data, .. } => unsafe { data.add(src as usize).cast_const() },
                };
                // Both locks exclude detiling while this exact version is captured.
                unsafe { std::ptr::copy_nonoverlapping(source, data.add(offset as usize), len as usize); }
                let slice = UploadSlice { memory: memory.clone(), offset: base + offset };
                if let StageStorage::Cpu(blocks) = &mut *storage {
                    STAGED[1].fetch_sub(blocks[i].len() as u64, Relaxed);
                    blocks[i] = Vec::new();
                    parts[i].captured = Some(slice.clone());
                }
                slice
            };
            claimed.push((i, slice));
            parts[i].dirty = false;
            parts[i].pending = id;
        }
        drop(storage);
        let mut upload = self.clone();
        upload.version = Some(Arc::new(UploadVersion {
            id,
            staging: Arc::downgrade(&self.staging),
            sent: Default::default(),
            parts: claimed,
        }));
        drop(parts);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexUploads, 1);
        // A host copy that took the place of one with fewer levels: the slots
        // that show that one are made again once this upload has run.
        if self.staging.first == self.staging.level0 && self.copy.grown.swap(false, Relaxed) {
            crate::descriptor::host::levels_changed_after(self.staging.memory as u64);
        }
        // Levels on demand (`crate::tiles`): after `refresh()` has detiled,
        // give the held levels their memory on the queue before the prelude
        // copies into them; otherwise record what the host copy now holds.
        if let Some(on_demand) = &self.on_demand {
            let waiting = on_demand.uploaded(
                self.staging.detiled.load(Relaxed),
                self.staging.memory as u64,
            );
            self.staging.waiting.store(
                if waiting { crate::host::moment().max(1) } else { 0 },
                Relaxed,
            );
            // A texture picked by index has no staging buffer after this
            // upload: its slots are made due again by the texture pass.
            if waiting {
                crate::descriptor::host::levels_waiting(self.staging.memory as u64);
            }
        } else {
            self.staging.note_held();
        }
        Some((upload, first_upload))
    }

    /// Records `CopyTextureRegion` (and any `texstore` prefill) onto `cmd_list`
    /// (the prelude command list). `self.copy.texture` must already be in
    /// `D3D12_RESOURCE_STATE_COPY_DEST`.
    pub(crate) fn record(&self, host_list: &HostList, cmd_list: &ID3D12GraphicsCommandList) {
        let texture = &self.copy.texture;
        // A host copy made whole from the store (`crate::texstore`): its larger
        // levels, once, before the ones from title memory.
        let prefill = self.copy.prefill.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some((levels, bytes)) = prefill
            && prefill_levels(host_list, cmd_list, &self.copy, levels, &bytes)
        {
            if let Some(timer) = &self.partial_timer { crate::texlate::whole(timer); }
        }
        // The staging buffer lives as long as `self.staging`, which the list's
        // deferred steps hold until the list is reset after GPU completion.
        let version = self.version.as_ref().expect("claimed texture occurrence");
        // The submission owns this immutable version through its completion fence.
        for (i, source) in &version.parts {
            let (i, offset) = (*i, source.offset);
            let footprint = &self.staging.footprints[i];
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: {
                        let (mip, slice) = self.staging.part(i);
                        mip - self.staging.level0
                            + slice * (self.staging.mips - self.staging.level0)
                    },
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(&source.memory.buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: offset,
                        ..*footprint
                    },
                },
            };
            // SAFETY: an open prelude list; both locations name live resources.
            unsafe { cmd_list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }
}

/// Records copies of levels `0..levels` of `copy` (in the copy destination
/// state) from `bytes`, laid out as the host lays out those subresources;
/// its views show them from then on.
fn prefill_levels(
    host_list: &HostList,
    cmd_list: &ID3D12GraphicsCommandList,
    copy: &crate::resource::HostTexture,
    levels: u32,
    bytes: &[u8],
) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let Some(host) = host() else {
        return false;
    };
    let texture = &copy.texture;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    if levels == 0 || levels > u32::from(d.MipLevels) || d.DepthOrArraySize != 1 {
        return false;
    }
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); levels as usize];
    let mut total = 0;
    // SAFETY: outputs for `levels` subresources.
    unsafe {
        host.device.GetCopyableFootprints(&d, 0, levels, 0, Some(footprints.as_mut_ptr()), None, None, Some(&mut total));
    }
    if total as usize > bytes.len() {
        return false;
    }
    let Some((buffer, data)) = host.upload_buffer(total) else {
        return false;
    };
    // SAFETY: the mapping holds `total` bytes.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, total as usize) };
    host_list.keep(&buffer);
    for (i, footprint) in footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { cmd_list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    copy.prefilled.store(levels, Relaxed);
    crate::texstore::PREFILLED.fetch_add(1, Relaxed);
    true
}

/// Levels on demand: the levels a list claimed for its zeros
/// (`crate::tiles::Tiled::claim`), held by the list's submission-time
/// step. A list that is reset without having been submitted gives the
/// claim up, or the next list that writes those levels would send no zeros
/// and what its write does not cover would be what the tiles' last owner
/// left.
struct Claimed {
    on_demand: crate::resource::OnDemand,
    fresh: u32,
    ran: std::sync::atomic::AtomicBool,
}

impl Drop for Claimed {
    fn drop(&mut self) {
        if self.fresh != 0
            && !*self.ran.get_mut()
            && let Some(tiled) = self.on_demand.tiled()
        {
            tiled.unclaim(self.fresh);
        }
    }
}

/// Records copies of zeros into every subresource of `texture`, which is in
/// the copy destination state.
pub(crate) fn zeros(list: &HostList, host: &crate::host::Host, texture: &ID3D12Resource) {
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { u32::from(d.DepthOrArraySize) };
    let count = u32::from(d.MipLevels) * layers;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut bytes = 0u64;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device
            .GetCopyableFootprints(&d, 0, count, 0, Some(footprints.as_mut_ptr()), None, None, Some(&mut bytes));
    }
    if bytes == 0 || bytes == u64::MAX {
        return;
    }
    let Some((buffer, data)) = list.upload(host, bytes) else {
        return;
    };
    // SAFETY: a mapped buffer of `bytes` bytes, just made.
    unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    for (i, footprint) in footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above; the list holds the buffer until it is reset.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
}

/// Levels on demand (`crate::tiles`): the card itself is about to write
/// title texture `resource` in `list` (a copy or a resolve into level
/// `level`, or into every level). If its host copy `texture` was made
/// without memory, the levels written get theirs when the list is
/// submitted, ahead of it on the queue; the ones that have none yet are
/// first written with zeros, here in the same list, so that what the
/// card's write does not cover is not what the tiles' last owner left.
/// To be called before the list is brought to the state of that write.
/// shortcut: two lists that write the same new level, submitted in the
/// other order than recorded, lose the first one's write under the zeros
/// of the other; so does an upload recorded and not yet submitted.
pub(crate) fn card_writes(list: &HostList, resource: This, texture: &ID3D12Resource, level: Option<u32>) {
    let Some(on_demand) = crate::resource::host_tiled(resource) else {
        return;
    };
    let Some(tiled) = on_demand.tiled() else {
        return;
    };
    let levels = match level {
        Some(level) => tiled.with_level(level),
        None => tiled.every_level(),
    };
    if levels == 0 {
        return;
    }
    let fresh = tiled.claim(levels);
    // Of them, the ones whose zeros could not be recorded.
    let mut unzeroed = if host().is_none() { fresh } else { 0 };
    if fresh != 0
        && let Some(host) = host()
    {
        const COPY_DEST: u32 = 0x400;
        crate::list::ensure_state_access(list, resource, COPY_DEST, false);
        list.native_alias_changed(on_demand.serial());
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        for level in (0..u32::from(d.MipLevels).min(32)).filter(|&level| fresh & (1u32 << level) != 0) {
            let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut bytes = 0u64;
            // SAFETY: outputs for one subresource.
            unsafe {
                host.device
                    .GetCopyableFootprints(&d, level, 1, 0, Some(&mut footprint), None, None, Some(&mut bytes));
            }
            let Some((buffer, data)) = list.upload(host, bytes) else {
                // (With the levels that share its tiles, if it is one of
                // the small ones.)
                unzeroed |= tiled.with_level(level);
                continue;
            };
            // SAFETY: a mapped buffer of `bytes` bytes, just made.
            unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: level },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above; the list holds the buffer until it is reset.
                pResource: unsafe { std::mem::transmute_copy(&buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
            };
            // SAFETY: an open list; both locations name live resources.
            unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }
    // A new level without its zeros gets no tiles from this list: the
    // card's write into it is lost (the level reads zeros, as before the
    // write), which is less wrong than a part write over what the tiles'
    // last owner left. The next list that writes it claims it again.
    unzeroed &= fresh;
    if unzeroed != 0 {
        tiled.unclaim(unzeroed);
    }
    let levels = levels & !unzeroed;
    if levels == 0 {
        return;
    }
    let memory = crate::resource::texture_memory(resource).map_or(0, |(memory, _)| memory as u64);
    let step = Claimed {
        on_demand,
        fresh: fresh & !unzeroed,
        ran: std::sync::atomic::AtomicBool::new(false),
    };
    list.defer_texture_note(move || {
        step.ran.store(true, std::sync::atomic::Ordering::Relaxed);
        step.on_demand.card_wrote(levels, memory);
    });
}

// Encoded blocks whose decoded channels have the value of an unmapped tile.
// BC1 requires transparent index 3. BC7 mode 6 encodes zero RGBA endpoints.
fn null_sample_block(format: u32) -> Option<&'static [u8]> {
    match format {
        // R16_TYPELESS and R16_UNORM use the same zero bits. Every typed
        // interpretation of the R16 backing, including FLOAT, decodes those
        // bits as zero. The view supplies any missing channels.
        53 | 56 => Some(&[0; 2]),
        70..=72 => Some(&[0, 0, 0, 0, 255, 255, 255, 255]),
        79..=81 => Some(&[0; 8]),
        73..=78 | 82..=84 | 94..=96 => Some(&[0; 16]),
        97..=99 => Some(&[64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        _ => None,
    }
}

fn linear_1d_null_sample(layout: &victory_formats::texture::Layout, format: u32, native_depth: bool) -> bool {
    use victory_formats::texture::{Dimension, Swizzle};
    let desc = &layout.desc;
    // Zero bits encode numeric zero in the RGBA8, RG16 and R32 color
    // families. Keep the native view's channel defaults and component mapping.
    !native_depth && matches!(format, 27..=39 | 41..=43)
        && desc.dimension == Dimension::Tex1d && desc.swizzle == Swizzle::LINEAR
        && desc.mips == 1 && layout.slices == 1 && desc.samples == 1
        && desc.element_bytes == 4 && desc.block == 1
}

fn fill_null_blocks(bytes: &mut [u8], block: &[u8]) {
    if bytes.is_empty() { return; }
    if block.iter().all(|&v| v == 0) { bytes.fill(0); return; }
    let mut filled = block.len().min(bytes.len());
    bytes[..filled].copy_from_slice(&block[..filled]);
    while filled < bytes.len() {
        let count = filled.min(bytes.len() - filled);
        bytes.copy_within(..count, filled);
        filled += count;
    }
}

/// Xbox page mappings address encoded texels or blocks before detiling.
/// Preserve live pages even when part of a subresource is explicitly NULL.
fn capture_sparse_blocks(start: usize, at: usize, out: &mut [u8], block: &[u8]) -> Option<usize> {
    if !crate::inplace::sparse_range(at, out.len()) { return None; }
    if (at - start) % block.len() == 0 && crate::inplace::sparse_null_range(at, out.len()) {
        fill_null_blocks(out, block);
        return Some(0);
    }
    let mut copied = 0;
    let mut offset = 0;
    while offset < out.len() {
        let address = at + offset;
        let count = (0x10000 - (address & 0xffff)).min(out.len() - offset);
        let part = &mut out[offset..offset + count];
        if crate::inplace::sparse_null_range(address, count) {
            let phase = (address - start) % block.len();
            if phase != 0 || part.len() % block.len() != 0 { return None; }
            fill_null_blocks(part, block);
        } else if crate::host::read_into(address as *const u8, part) {
            copied += count;
        } else { return None; }
        offset += count;
    }
    Some(copied)
}

impl Staging {
    /// Levels on demand: what says, beside the mapping, which levels of
    /// the texture are the title's now. First the lowest level a live view
    /// of the texture shows (0 without a view that says): the title's own
    /// word, a texture it holds from level 3 on is viewed from level 3.
    /// Then where the next texture placed in this one's span starts, in
    /// bytes from this one's start (`u64::MAX`: none does): for a texture
    /// viewed from level 0 although a neighbour sits where its large
    /// levels would be.
    fn judgement(&self) -> [u64; 2] {
        let start = self.memory as u64;
        let first = crate::descriptor::host::first_viewed(start).map_or(0, u64::from);
        let limit = crate::descriptor::host::next_start(start, start.saturating_add(self.layout.size))
            .map_or(u64::MAX, |at| at - start);
        [first, limit]
    }

    /// What [`Staging::judgement`] said at the last detiling.
    fn judged(&self) -> [u64; 2] {
        use std::sync::atomic::Ordering::Relaxed;
        [self.judged[0].load(Relaxed), self.judged[1].load(Relaxed)]
    }

    /// Levels on demand: the levels that are the title's now, a bit a
    /// level. A level is held if no view starts above it, its bytes end
    /// before the next texture starts, and all of them are mapped. The
    /// smallest levels share the texture's first block and are held
    /// together. (The console layout: that block first, then the other
    /// levels from the smallest up, level 0 last:
    /// `victory_formats::texture::Layout`.) A texture this cannot tell
    /// the levels of holds them all.
    fn held_levels(&self, [first, limit]: [u64; 2]) -> u32 {
        let mips = &self.layout.mips;
        if self.layout.slices != 1 || mips.is_empty() || mips.len() > 32 {
            return u32::MAX;
        }
        let tail = (self.layout.first_tail_mip as usize).min(mips.len());
        let there = |from: u64, to: u64| {
            to > from && to <= limit && committed(self.memory.wrapping_add(from as usize), (to - from) as usize)
        };
        let mut held = 0u32;
        for (level, m) in mips.iter().enumerate().take(tail) {
            let end = if level == 0 { self.layout.slice_size } else { mips[level - 1].offset };
            if level as u64 >= first && there(m.offset, end) {
                held |= 1 << level;
            }
        }
        if tail < mips.len() && there(0, self.layout.alignment.min(self.layout.slice_size)) {
            for level in tail..mips.len() {
                if level as u64 >= first {
                    held |= 1 << level;
                }
            }
        }
        held
    }

    /// The title memory the texture is read from: address and bytes.
    pub(crate) fn memory(&self) -> (usize, usize) {
        (self.memory as usize, self.read)
    }

    /// The title's level and the array slice of the staging buffer's part
    /// `i`.
    fn part(&self, i: usize) -> (u32, u32) {
        let mips = self.mips - self.first;
        (self.first + i as u32 % mips, i as u32 / mips)
    }

    fn part_bytes(&self, i: usize) -> u64 {
        let fp = &self.footprints[i];
        (u64::from(fp.Footprint.RowPitch) * u64::from(self.rows[i]) * u64::from(fp.Footprint.Depth))
            .min(self.bytes - fp.Offset)
    }

    /// The host copy holds what the staging buffer was filled from
    /// ([`unchanged`]).
    fn note_held(&self) {
        let hash = self.last.lock().unwrap_or_else(|e| e.into_inner()).hash;
        *self.holds.lock().unwrap_or_else(|e| e.into_inner()) = hash.map(|hash| (self.first, hash));
    }

    /// This occurrence was rejected before its first texture upload. A later
    /// occurrence observes its own guest bytes and mapping under the usual
    /// refresh locks. Keep every already-claimed or GPU-visible version.
    pub(crate) fn discard_unsubmitted_cpu(&self) -> u64 {
        use std::sync::atomic::Ordering;
        if self.uploaded.load(Ordering::Acquire) { return 0; }
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let mut parts = self.parts.lock().unwrap_or_else(|e| e.into_inner());
        if self.uploaded.load(Ordering::Acquire) || parts.iter().any(|p| p.pending != 0) { return 0; }
        let mut storage = self.storage.lock().unwrap_or_else(|e| e.into_inner());
        let StageStorage::Cpu(blocks) = &mut *storage else { return 0; };
        let released = blocks.iter().map(|b| b.len() as u64).sum();
        if released == 0 { return 0; }
        *blocks = Vec::new();
        parts.clear();
        *last = Last::default();
        self.detiled.store(0, Ordering::Relaxed);
        self.dirty.store(false, Ordering::Release);
        STAGED[1].fetch_sub(released, Ordering::Relaxed);
        released
    }

    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all).
    pub(crate) fn refresh(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let _texture = crate::timeline::span("a texture's refresh");
        let read = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(&self) -> usize {
        let whole = std::time::Instant::now();
        let read = self.refresh_timed(false);
        REFRESH_WAITS[3].fetch_add(whole.elapsed().as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
        read
    }

    fn refresh_timed(&self, required: bool) -> usize {
        let size = self.read;
        let start = self.memory as usize;
        let mut last = waited(0, || self.last.lock().unwrap_or_else(|e| e.into_inner()));
        let last = &mut *last;
        last.mapping_busy = false;
        if let Some((plane, offset)) = crate::bink::plane(start, size) {
            last.mapping_epoch = None;
            last.asked = 0;
            #[cfg(feature = "diagnostics")]
            last.previous.set_owner(SnapshotOwner::Movie);
            return with_read(size, |read| {
                let covered = (plane.len() - offset).min(size);
                if covered < size
                    && !crate::host::read_into((start + covered) as *const u8, &mut read[covered..])
                {
                    self.parts.lock().unwrap_or_else(|e| e.into_inner()).clear();
                    last.hash = None;
                    return 0;
                }
                read[..covered].copy_from_slice(&plane[offset..offset + covered]);
                let hash = digest(read);
                if last.hash != Some(hash) || last.previous.get(0..size) != Some(&*read) {
                    last.hash = self.detile_parts(read, None, Some(&mut last.previous)).then_some(hash);
                }
                size
            });
        }
        // The identity comparison and the captured bytes must describe one
        // mapping. A pending remap invalidates the old ready parts for this read.
        let _mapping = if no_wait() && !required {
            let Some(guard) = crate::watch::title_memory_reading_now() else {
                last.mapping_busy = true;
                last.asked = 0;
                last.hash = None;
                self.parts.lock().unwrap_or_else(|e| e.into_inner()).clear();
                return 0;
            };
            guard
        } else {
            waited(2, crate::watch::title_memory_reading)
        };
        // A recording thread can prepare the next draw while the submitting
        // thread is consuming this staging object. Only the ordered refresh
        // may invalidate the captured parts for a changed mip view.
        if self.on_demand && self.sparse_image.is_none() && self.judgement() != self.judged() {
            *last = Last::default();
            self.parts.lock().unwrap_or_else(|e| e.into_inner()).clear();
            self.dirty.store(false, std::sync::atomic::Ordering::Release);
        }
        if self.sparse_image.as_ref().and_then(|w| w.upgrade()).is_some_and(|copy|
            copy.tiled.as_ref().is_some_and(|tiles| tiles.given_bytes() == 0))
            && crate::inplace::sparse_null_range(start, size) {
            // The host reserved image already represents this precise mapping.
            // There is no tile to populate and no upload allocation to retain.
            self.native_null.store(true, std::sync::atomic::Ordering::Release);
            self.detiled.store(0, std::sync::atomic::Ordering::Relaxed);
            self.dirty.store(false, std::sync::atomic::Ordering::Release);
            self.parts.lock().unwrap_or_else(|e| e.into_inner()).clear();
            last.hash = None;
            last.asked = 0;
            last.mapping_epoch = None;
            last.release_previous();
            return 0;
        }
        self.native_null.store(false, std::sync::atomic::Ordering::Release);
        let epoch = crate::watch::changes();
        if let Some(copy) = self.sparse_image.as_ref().and_then(|w| w.upgrade())
            && let Some(tiles) = &copy.tiled {
            let previous = self.null_levels.load(std::sync::atomic::Ordering::Relaxed);
            if last.mapping_epoch != Some(epoch) || previous != 0 && tiles.without(previous) != previous {
                let absent = tiles.without(tiles.every_level());
                let mut null = 0u32;
                for mip in self.first..self.mips {
                    if absent & (1 << mip) != 0 && (0..self.layout.slices).all(|slice|
                        self.layout.subresource_runs(mip, slice).is_some_and(|mut runs|
                            runs.all(|r| crate::inplace::sparse_null_range(start + r.start, r.len())))) {
                        null |= 1 << mip;
                    }
                }
                // The native packed tail shares tiles. Leave them unmapped
                // only when every guest mip in that group is absent.
                for mip in self.first..self.mips {
                    let group = tiles.with_level(mip);
                    if null & group != group { null &= !group; }
                }
                self.null_levels.store(null, std::sync::atomic::Ordering::Release);
                if null != previous {
                    last.asked = 0;
                    last.hash = None;
                    let mut parts = self.parts.lock().unwrap_or_else(|e| e.into_inner());
                    for (i, part) in parts.iter_mut().enumerate() {
                        if (null ^ previous) & (1 << self.part(i).0) != 0 {
                            *part = StagedPart::default();
                        }
                    }
                }
            }
        }
        if last.mapping_epoch != Some(epoch) {
            let backing = crate::inplace::backing(start, size);
            if backing != last.backing {
                last.asked = 0;
                last.hash = None;
                last.partial = None;
                let mut parts = self.parts.lock().unwrap_or_else(|e| e.into_inner());
                for (i, part) in parts.iter_mut().enumerate() {
                    let (mip, slice) = self.part(i);
                    let Some(mut runs) = self.layout.subresource_runs(mip, slice) else {
                        *part = StagedPart::default();
                        continue;
                    };
                    let changed = runs.any(|range| {
                        let (lo, hi) = (start + range.start, start + range.end);
                        let clip = |&(base, end, section, offset, generation): &(
                            usize,
                            usize,
                            usize,
                            u64,
                            u64,
                        )| {
                            let from = lo.max(base);
                            let to = hi.min(end);
                            (from < to).then(|| {
                                (from, to, section, offset + (from - base) as u64, generation)
                            })
                        };
                        !last
                            .backing
                            .iter()
                            .filter_map(clip)
                            .eq(backing.iter().filter_map(clip))
                    });
                    if changed {
                        *part = StagedPart::default();
                    }
                }
                self.dirty.store(
                    parts.iter().any(|p| p.dirty),
                    std::sync::atomic::Ordering::Release,
                );
                last.backing = backing;
            }
            let guard = if last.backing.is_empty() { None } else {
                crate::inplace::guarded_view_epoch(start, start + size, &last.backing)
            };
            let epoch_for_source = guard.map(|(generation, _)| generation);
            if epoch_for_source != last.guarded_epoch {
                last.guarded_epoch = epoch_for_source;
                last.asked = 0;
            }
            #[cfg(feature = "diagnostics")]
            {
                let owner = if last.backing.is_empty() {
                    SnapshotOwner::NoPhysicalView
                } else if matches!(guard, Some((_, false))) {
                    SnapshotOwner::Unique
                } else if matches!(guard, Some((_, true))) {
                    SnapshotOwner::Shared
                } else if last.backing.iter().map(|&(from, to, ..)| to - from).sum::<usize>() != size {
                    SnapshotOwner::Partial
                } else {
                    SnapshotOwner::ManyViews
                };
                last.previous.set_owner(owner);
            }
            last.mapping_epoch = Some(epoch);
        }
        let journal = waited(1, || {
            if last.backing.is_empty() { crate::watch::writes(start, start + size, false) }
            else if last.guarded_epoch.is_some() { crate::watch::physical_view_writes(start, start + size) }
            else { None }
        });
        if let Some((now, writes)) = journal {
            let previous = last.asked;
            last.asked = now;
            if !last.previous.bytes.is_empty() { last.release_previous(); }
            if previous != 0 && writes.iter().all(|&(_, stamp)| stamp <= previous) {
                return 0;
            }
            let mut retry = false;
            let bytes = with_read(size, |read| {
                let (coverage, total) = self.snapshot_parts(read, |range| {
                    if previous == 0 {
                        return true;
                    }
                    let first = (start + range.start) / crate::watch::PAGE;
                    let end = (start + range.end).div_ceil(crate::watch::PAGE);
                    let from = writes.partition_point(|&(p, _)| p < first);
                    writes[from..]
                        .iter()
                        .take_while(|&&(p, _)| p < end)
                        .any(|&(_, stamp)| stamp > previous)
                });
                retry = coverage.contains(&Some(false));
                retry |= !self.detile_parts(read, Some(&coverage), None);
                total
            });
            if retry {
                last.asked = 0;
            }
            return bytes;
        }
        last.asked = 0;
        // Where the journal cannot vouch for the bytes, capture
        // once. Comparison, detiling, and the queued copy all use that version.
        with_read(size, |read| {
            if !crate::host::read_into(self.memory, read) {
                last.asked = 0;
                last.hash = None;
                last.partial = Some(crate::frame::current());
                let (coverage, bytes) = self.snapshot_parts(read, |_| true);
                self.detile_parts(read, Some(&coverage), Some(&mut last.previous));
                return bytes;
            }
            if last.hash.is_some() && last.previous.get(0..size) == Some(&*read) { return size; }
            let hash = digest(read);
            last.hash = self.detile_parts(read, None, Some(&mut last.previous)).then_some(hash);
            if last.hash.is_some() { self.keep_whole(read, Some(hash)); }
            size
        })
    }

    /// `tiled`, read whole, was detiled: a texture of all its levels from
    /// level 0 is given to the store (`crate::texstore`), detiled again
    /// into memory of ours (the staging buffer is the card's to read, slow
    /// for the processor).
    fn keep_whole(&self, tiled: &[u8], stable: Option<Digest>) {
        if !crate::texstore::on()
            || self.first != 0
            || self.level0 != 0
            || self.on_demand
            || self.layout.slices != 1
            || tiled.len() as u64 != self.layout.size
            || crate::texstore::key_levels(&self.layout, self.mips)
                .next()
                .is_none()
            || self.footprints.len() != self.mips as usize
        {
            return;
        }
        // (The detiling, digests and writing on the store's thread: here
        // only a copy of the memory, once per content.)
        let parts: Vec<(u64, u32, u32)> = self
            .footprints
            .iter()
            .zip(&self.rows)
            .map(|(f, &rows)| (f.Offset, f.Footprint.RowPitch, rows * f.Footprint.Depth))
            .collect();
        crate::texstore::keep(self.shape, &self.layout, tiled, stable, parts);
    }

    /// Capture shared packed tails once. If a coalesced read hits a hole,
    /// check each distinct original run so neighbouring valid mips survive.
    fn snapshot_parts(
        &self,
        read: &mut [u8],
        mut changed: impl FnMut(std::ops::Range<usize>) -> bool,
    ) -> (Vec<Option<bool>>, usize) {
        let mut coverage = vec![None; self.footprints.len()];
        let mut spans = std::collections::BTreeMap::<(usize, usize), Vec<usize>>::new();
        for (i, ready) in coverage.iter_mut().enumerate() {
            let (mip, slice) = self.part(i);
            if self.null_levels.load(std::sync::atomic::Ordering::Relaxed) & (1 << mip) != 0 { continue; }
            let Some(runs) = self.layout.subresource_runs(mip, slice) else {
                *ready = Some(false);
                continue;
            };
            if !runs.clone().any(&mut changed) {
                continue;
            }
            *ready = Some(true);
            for run in runs {
                if run.end > read.len() {
                    *ready = Some(false);
                    break;
                }
                spans.entry((run.start, run.end)).or_default().push(i);
            }
        }
        let spans: Vec<_> = spans.into_iter().collect();
        let mut bytes = 0;
        let mut i = 0;
        let at = self.memory as usize;
        while i < spans.len() {
            let first = i;
            let (lo, mut hi) = spans[i].0;
            i += 1;
            while i < spans.len() && spans[i].0.0 <= hi {
                hi = hi.max(spans[i].0.1);
                i += 1;
            }
            if crate::host::read_into((at + lo) as *const u8, &mut read[lo..hi]) {
                bytes += hi - lo;
                continue;
            }
            for ((lo, hi), indices) in &spans[first..i] {
                if i - first > 1
                    && crate::host::read_into((at + lo) as *const u8, &mut read[*lo..*hi])
                {
                    bytes += hi - lo;
                } else if let Some(actual) = self.read_sparse_blocks(at + lo, &mut read[*lo..*hi]) {
                    bytes += actual;
                } else {
                    for &index in indices {
                        coverage[index] = Some(false);
                    }
                }
            }
        }
        (coverage, bytes)
    }

    fn read_sparse_blocks(&self, at: usize, out: &mut [u8]) -> Option<usize> {
        let format = self.shape[2];
        let block = if self.linear_null { &[0; 4] } else { null_sample_block(format)? };
        let copied = capture_sparse_blocks(self.memory as usize, at, out, block)?;
        #[cfg(feature = "diagnostics")]
        if self.linear_null && copied < out.len() && crate::switch::on("VICTORY_TRACE_TEXTURE_OWNER") {
            static FIRST: [std::sync::Once; 17] = [const { std::sync::Once::new() }; 17];
            FIRST[(format - 27) as usize].call_once(|| tracing::info!(image = self.serial, format,
                address = format_args!("{at:#x}"), bytes = out.len(), copied, width = self.layout.desc.width,
                "staged explicit NULL Tex1D pages as zero texels"));
        }
        Some(copied)
    }

    fn detile_parts(&self, tiled: &[u8], coverage: Option<&[Option<bool>]>, previous: Option<&mut Previous>) -> bool {
        // Levels on demand: which levels are the title's is judged here,
        // once, for what the staging buffer holds from now on. The step
        // that gives levels their memory goes by this answer.
        let held = if self.sparse_image.is_some() {
            // The reservation and page mappings decide ownership. A partly
            // mapped mip must retain the blocks captured from its live pages.
            let levels = (self.first..self.mips).fold(0, |mask, mip| mask | 1u32 << mip)
                & !self.null_levels.load(std::sync::atomic::Ordering::Relaxed);
            self.detiled.store(levels, std::sync::atomic::Ordering::Relaxed);
            levels
        } else if self.on_demand {
            use std::sync::atomic::Ordering::Relaxed;
            let judged = self.judgement();
            self.judged[0].store(judged[0], Relaxed);
            self.judged[1].store(judged[1], Relaxed);
            let held = self.held_levels(judged);
            self.detiled.store(held, Relaxed);
            held
        } else {
            u32::MAX
        };
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        let mut parts = self.parts.lock().unwrap_or_else(|e| e.into_inner());
        parts.resize_with(self.footprints.len(), StagedPart::default);
        let mut storage = self.storage.lock().unwrap_or_else(|e| e.into_inner());
        let mut conversion = self.layout.conversion();
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            let (mip, slice) = self.part(i);
            if let Some(coverage) = coverage {
                match coverage[i] {
                    None => continue,
                    Some(false) => {
                        parts[i] = StagedPart::default();
                        continue;
                    }
                    Some(true) => {}
                }
            }
            let Some(mut runs) = self.layout.subresource_runs(mip, slice) else {
                continue;
            };
            let Some(first) = runs.next().and_then(|r| tiled.get(r)) else {
                continue;
            };
            let hash = if self.layout.desc.dimension == victory_formats::texture::Dimension::Tex3d {
                let mut hash = blake3::Hasher::new();
                hash.update(first);
                if !runs.all(|r| {
                    tiled.get(r).is_some_and(|data| {
                        hash.update(data);
                        true
                    })
                }) {
                    continue;
                }
                Digest(u128::from_le_bytes(
                    hash.finalize().as_bytes()[..16].try_into().unwrap(),
                ))
            } else {
                digest(first)
            };
            let resident = mip >= 32 || held & (1 << mip) != 0;
            let signature = (hash, resident);
            if parts[i].content == Some(signature)
                && previous.as_deref().is_some_and(|old| self.layout.subresource_runs(mip, slice)
                    .is_some_and(|mut runs| runs.all(|r| old.get(r.clone()).is_some_and(|data| tiled.get(r) == Some(data)))))
            {
                continue;
            }
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            // The last row of the last subresource is not padded to its pitch
            // (VIctory 4fc9d70): to the end of the buffer at most, where a
            // block of the pool goes on with another texture's.
            let len = (pitch * rows as usize * f.Depth as usize)
                .min(self.bytes.saturating_sub(footprint.Offset) as usize);
            let data = match &mut *storage {
                StageStorage::Cpu(blocks) => {
                    if blocks.len() <= i { blocks.resize_with(i + 1, Vec::new); }
                    let bytes = &mut blocks[i];
                    if bytes.is_empty() {
                        let size = len;
                        if bytes.try_reserve_exact(size).is_err() {
                            crate::residency::note_staging_deferred();
                            crate::host::refuse_frame(format_args!("could not stage {size} texture bytes"));
                            // Leave no ready signature for an upload that could not be captured.
                            parts.clear();
                            self.dirty.store(false, std::sync::atomic::Ordering::Release);
                            return false;
                        }
                        bytes.resize(size, 0);
                        STAGED[1].fetch_add(size as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                    bytes.as_mut_ptr()
                }
                StageStorage::Upload { data, .. } => unsafe { data.add(footprint.Offset as usize) },
            };
            // SAFETY: the subresource's part of the mapped staging buffer.
            let linear = unsafe {
                std::slice::from_raw_parts_mut(data, len)
            };
            if mip < 32 && held & (1 << mip) == 0 {
                // Not the title's: zeros, not what lies in that memory.
                linear.fill(0);
                parts[i] = StagedPart {
                    content: Some(signature),
                    dirty: true,
                    pending: 0, captured: None,
                };
                continue;
            }
            if let Err(e) =
                conversion.detile(tiled, mip, slice, linear, pitch, pitch * rows as usize)
            {
                parts[i] = StagedPart::default();
                tracing::warn!(?e, mip, slice, "texture not detiled");
                continue;
            }
            parts[i] = StagedPart {
                content: Some(signature),
                dirty: true,
                pending: 0, captured: None,
            };
        }
        if let Some(previous) = previous {
            let runs = coverage.map(|coverage| {
                let mut captured = Vec::new();
                for (i, _) in coverage.iter().enumerate().filter(|(_, c)| **c == Some(true)) {
                    let (mip, slice) = self.part(i);
                    if let Some(runs) = self.layout.subresource_runs(mip, slice) {
                        captured.extend(runs);
                    }
                }
                captured
            });
            previous.capture(tiled, runs);
        }
        self.dirty.store(
            parts.iter().any(|p| p.dirty),
            std::sync::atomic::Ordering::Release,
        );
        true
    }
}

/// Lets go of a title texture's staging buffer (lists that recorded an
/// upload from it hold it until they have run); the next upload makes one
/// anew.
pub(crate) fn unstage(resource: This) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&(resource as usize));
    }
}

/// Where a staged title texture's memory is: address and bytes.
pub(crate) fn staged_memory(resource: This) -> Option<(u64, u64)> {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let (at, len) = map.as_ref()?.get(&(resource as usize))?.memory();
    Some((at as u64, len as u64))
}

/// Hash the entire view. A same-sized write may change a single texel anywhere in it.
pub(crate) fn fingerprint(at: u64, len: u64) -> u64 {
    const CHUNK: usize = 64 << 10;
    thread_local! {
        static SCRATCH: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0; CHUNK]);
    }
    SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        let mut hash = blake3::Hasher::new();
        let mut done = 0;
        while done < len {
            let n = (len - done).min(CHUNK as u64) as usize;
            crate::host::copy_mapped(scratch.as_mut_ptr(), (at + done) as *const u8, n, false);
            hash.update(&scratch[..n]);
            done += n as u64;
        }
        u64::from_le_bytes(hash.finalize().as_bytes()[..8].try_into().unwrap()) | 1
    })
}

/// Whether a title texture has a staging buffer: a list recorded its
/// upload, and the lists that sample it check it at submission.
/// For the frame listing: whether a list has uploaded `resource`'s staging
/// buffer, and whether it was detiled since. None: it has none.
#[cfg(feature = "diagnostics")]
pub(crate) fn staged_state(resource: This) -> Option<(bool, bool)> {
    use std::sync::atomic::Ordering::Relaxed;
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let staging = map.as_ref()?.get(&(resource as usize))?;
    Some((staging.uploaded.load(Relaxed), staging.dirty.load(Relaxed)))
}

/// Whether the levels from `first` on of `resource`'s host copy were filled
/// from the bytes title memory holds now: an upload would bring nothing new.
/// (Of 787 repeated uploads of a frame that streams 398 were equal in every
/// byte, and a texture uploaded again costs a staging buffer, a detiling and
/// a copy more than this look.) `VICTORY_TEXTURE_SAME=0`: not asked.
pub(crate) fn unchanged(resource: This, first: u32) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_SAME").is_none_or(|v| v != "0")) {
        return false;
    }
    let Some(copy) = crate::resource::made_copy(resource) else {
        return false;
    };
    let Some((from, held)) = *copy.holds.lock().unwrap_or_else(|e| e.into_inner()) else {
        return false;
    };
    let Some((memory, desc)) = crate::resource::texture_memory(resource) else {
        return false;
    };
    let Some(layout) = crate::resource::texture_layout(&desc) else {
        return false;
    };
    let layout = &layout.planes[0].layout;
    if from > crate::resource::read_from(layout, first).max(copy.base) {
        return false;
    }
    let size = layout.bytes_from(from) as usize;
    with_read(size, |read| {
        if crate::host::read_there(memory, read) == 0 {
            return false;
        }
        digest(read) == held
    })
}

/// Where the levels from `first` on of a title texture are: address and
/// bytes.
pub(crate) fn title_span(resource: This, first: u32) -> Option<(u64, u64)> {
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let layout = crate::resource::texture_layout(&desc)?;
    let layout = &layout.planes[0].layout;
    Some((memory as u64, layout.bytes_from(crate::resource::read_from(layout, first))))
}

pub(crate) fn is_staged(resource: This) -> bool {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref().is_some_and(|map| map.contains_key(&(resource as usize)))
}

/// The bytes of a title texture's memory, and whether all of it is mapped,
/// if the texture has been staged for upload.
/// (Whether it is mapped is asked only if `ask`; else it counts as mapped.)
pub(crate) fn staged(resource: This, ask: bool) -> Option<(u64, bool)> {
    let s = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()?.get(&(resource as usize))?.clone()
    };
    Some((s.read as u64, !ask || committed(s.memory, s.read)))
}

/// Whether `[at, at + len)` is committed memory (streamed textures may be
/// unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    if by_query() {
        return committed_by_query(at, len);
    }
    if len == 0 {
        return true;
    }
    let (there, of) = readable(at as usize, len, true);
    there == of
}

/// Whether the two questions above and below are put to `VirtualQuery`, as
/// they were (`VICTORY_COMMITTED_QUERY=1`). `VirtualQuery` answers with the
/// length of the run of pages in the same state, which it finds by walking
/// the page tables to the run's end: 8.8 ms for one question in the title's
/// 1.1 GB block (round 6). The pass over the heap's views asks three or
/// more of them for every texture slot that is due, 3,000 slots a frame
/// when the card has no room (run 75): that pass then ran 208-264 ms, and
/// while it ran the submitting thread's write-watch askings waited behind
/// the walks (run 76: 8-22 askings a frame of 6-21 ms each, 46-172 ms).
fn by_query() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COMMITTED_QUERY").is_some_and(|v| v == "1"))
}

/// Of the kernel's pages that hold `[at, at + len)` (`len > 0`): how many
/// can be read, and how many there are; with `all`, it stops at the first
/// that cannot. One question to the kernel for up to 256 pages at once
/// (`QueryWorkingSetEx`: the state of each page asked about, no walk); a
/// page not in the working set may still be there (never touched, or
/// trimmed), and a one-byte read through the kernel says.
/// A page that is committed and may not be read counts as not there, which
/// is what the callers ask for: they read or write the memory next.
fn readable(at: usize, len: usize, all: bool) -> (usize, usize) {
    use windows_sys::Win32::System::ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const KERNEL_PAGE: usize = 4096;
    let (first, last) = (at / KERNEL_PAGE, at.saturating_add(len - 1) / KERNEL_PAGE);
    let of = last - first + 1;
    let mut there = 0;
    // SAFETY: plain data.
    let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
    let mut page = first;
    while page <= last {
        let n = (last - page + 1).min(info.len());
        for (i, entry) in info[..n].iter_mut().enumerate() {
            entry.VirtualAddress = ((page + i) * KERNEL_PAGE) as *mut _;
        }
        // SAFETY: `n` entries of the size given; our own process.
        let asked = unsafe {
            K32QueryWorkingSetEx(
                GetCurrentProcess(),
                info.as_mut_ptr().cast(),
                (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
            )
        } != 0;
        for (i, entry) in info[..n].iter().enumerate() {
            // Bit 0 of the answer: the page is in the working set.
            // SAFETY: plain data the query filled in.
            let valid = asked && unsafe { entry.VirtualAttributes.Flags } & 1 != 0;
            let mut byte = [0u8];
            if valid || crate::host::read_into(((page + i) * KERNEL_PAGE) as *const u8, &mut byte) {
                there += 1;
            } else if all {
                return (there, of);
            }
        }
        page += n;
    }
    (there, of)
}

#[cfg(feature = "diagnostics")]
pub(crate) static BVH_COMMIT_CHECKS: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Validate a BVH declaration without touching its contents. VirtualQuery
/// can walk a multi-gigabyte protection run for a single eight-byte field.
/// QueryWorkingSetEx costs only the 4 KiB pages explicitly requested. A
/// committed page outside the working set still takes the full mapping check.
pub(crate) fn committed_declaration(at: usize, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::PAGE_NOACCESS;
    use windows_sys::Win32::System::ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const KERNEL_PAGE: usize = 4096;
    const BATCH: usize = 256;
    if len == 0 { return true; }
    let Some(end) = at.checked_add(len) else { return false };
    if crate::watch::guarded_readable(at, end) {
        #[cfg(feature = "diagnostics")]
        BVH_COMMIT_CHECKS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return true;
    }
    let first = at / KERNEL_PAGE;
    let last = end.div_ceil(KERNEL_PAGE);
    // Closely spaced, short declarations caused the repeated kernel walks.
    // For longer spans the region query already amortizes across many pages.
    let count = last - first;
    if count <= BATCH {
        // SAFETY: plain C records. The kernel fills their attribute field.
        let mut pages: [PSAPI_WORKING_SET_EX_INFORMATION; BATCH] = unsafe { std::mem::zeroed() };
        let pages = &mut pages[..count];
        for (i, page) in pages.iter_mut().enumerate() {
            page.VirtualAddress = ((first + i) * KERNEL_PAGE) as *mut _;
        }
        let ok = unsafe { K32QueryWorkingSetEx(GetCurrentProcess(), pages.as_mut_ptr().cast(),
            std::mem::size_of_val(pages) as u32) } != 0;
        if ok && pages.iter().all(|page| {
            // PSAPI_WORKING_SET_EX_BLOCK: Valid, ShareCount:3, Win32Protection:11.
            let flags = unsafe { page.VirtualAttributes.Flags };
            let base_protection = (flags >> 4) & 0xff;
            flags & 1 != 0 && base_protection != 0 && base_protection != PAGE_NOACCESS as usize
        }) {
            #[cfg(feature = "diagnostics")]
            BVH_COMMIT_CHECKS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return true;
        }
    }
    #[cfg(feature = "diagnostics")]
    let before = std::time::Instant::now();
    let valid = committed_by_query(at as *const u8, len);
    #[cfg(feature = "diagnostics")]
    {
        let micros = before.elapsed().as_micros() as u64;
        BVH_COMMIT_CHECKS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        BVH_COMMIT_CHECKS[3].fetch_add(micros, std::sync::atomic::Ordering::Relaxed);
        if micros >= 2_000 && crate::switch::on("VICTORY_GPU_PROGRESS") {
            static SLOW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let occurrence = SLOW.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if occurrence <= 8 || occurrence.is_power_of_two() {
                tracing::info!(address = format_args!("{at:#x}"), len, pages = count, micros, valid,
                    tried_working_set = count <= BATCH, occurrence, "slow BVH declaration mapping query");
            }
        }
    }
    valid
}

pub(crate) fn committed_by_query(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let end = p + len;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0
            || info.State != MEM_COMMIT
            || info.Protect & 0xff == windows_sys::Win32::System::Memory::PAGE_NOACCESS
        {
            return false;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// Identity of the covered runs, including committed regions outside the section-view index.
pub(crate) fn backing_identity(start: usize, len: usize) -> [u8; 32] {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let _mapped = crate::watch::title_memory_reading();
    let mut hash = blake3::Hasher::new();
    for (from, to, section, offset, generation) in crate::inplace::backing(start, len) {
        for value in [from as u64, to as u64, section as u64, offset, generation] {
            hash.update(&value.to_le_bytes());
        }
    }
    let end = start.saturating_add(len);
    let mut p = start;
    while p < end {
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) } == 0 {
            break;
        }
        let next = (info.BaseAddress as usize)
            .saturating_add(info.RegionSize)
            .min(end);
        if next <= p {
            break;
        }
        if info.State == MEM_COMMIT {
            for value in [p, next, info.AllocationBase as usize, info.Protect as usize] {
                hash.update(&value.to_le_bytes());
            }
        }
        p = next;
    }
    *hash.finalize().as_bytes()
}

/// How many bytes of `[at, at + len)` are committed memory: all of a
/// texture that is there, some of one streaming in (its smaller mips lie
/// at the start of its memory, and their pages are mapped first), none of
/// one not there. With neither the first nor the last page committed the
/// answer is none without a look at the pages between (two queries for the
/// slots whose texture is away, which are asked again and again).
pub(crate) fn mapped_bytes(at: *const u8, len: usize) -> u64 {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    if len == 0 {
        return 0;
    }
    let start = at as usize;
    let end = start.saturating_add(len);
    // Streamed textures lie in views of physical pages, of which the runtime
    // knows which are there: one look, where the host was asked for every
    // 64 KiB of the texture (and once more for each that is not there).
    // `VICTORY_MAPPED_BY_VIEWS=0`: the host is asked, as before.
    static BY_VIEWS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *BY_VIEWS.get_or_init(|| std::env::var_os("VICTORY_MAPPED_BY_VIEWS").is_none_or(|v| v != "0"))
        && let Some(there) = crate::inplace::shown_bytes(start, len)
    {
        return there;
    }
    let (mut p, mut there) = (start, 0u64);
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize).min(end);
        if n == 0 || next <= p {
            break;
        }
        if info.State == MEM_COMMIT
            && info.Protect
                & (windows_sys::Win32::System::Memory::PAGE_NOACCESS
                    | windows_sys::Win32::System::Memory::PAGE_GUARD)
                == 0
        {
            there += (next - p) as u64;
        }
        p = next;
    }
    there
}

/// Native first subresource, output first subresource, and count per consecutive run.
fn footprint_runs(mips: u32, first: u32, layers: u32) -> impl Iterator<Item = (u32, u32, u32)> {
    let groups = if first == 0 { 1 } else { layers };
    let count = if first == 0 { mips * layers } else { mips - first };
    (0..groups).map(move |g| (g * mips + first, g * count, count))
}

/// The staging state of a title texture, made on first use.
fn staging(resource: This, copy: &Arc<crate::resource::HostTexture>, first: u32, ahead: bool) -> Option<Arc<Staging>> {
    staging_for(resource,copy,first,ahead,false)
}

fn staging_for(resource: This, copy: &Arc<crate::resource::HostTexture>, first: u32, ahead: bool, narrow: bool) -> Option<Arc<Staging>> {
    let texture = &copy.texture;
    let on_demand = copy.tiled.is_some();
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(resource as usize))
        && s.first <= first.max(copy.base)
        && s.serial == copy.serial
        && (!narrow || s.first == crate::resource::read_from(&s.layout, first).max(copy.base).min(s.mips - 1))
    {
        return Some(s.clone());
    }
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).filter(|t| t.planes.len() == 1).and_then(|t| t.planes.into_iter().next().map(|p| p.layout))
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let level0 = copy.base;
    let mips = level0 + u32::from(d.MipLevels);
    let first = crate::resource::read_from(&layout, first).max(level0).min(mips - 1);
    let read = layout.bytes_from(first) as usize;
    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D { 1 } else { u32::from(d.DepthOrArraySize) };
    let count = (mips - first) * layers;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0u64;
    // Native mips skipped at the start of each array slice make separate runs.
    for (native, dest, n) in footprint_runs(mips - level0, first - level0, layers) {
        let base = total.checked_next_multiple_of(u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT))?;
        let mut bytes = 0u64;
        let out = &mut footprints[dest as usize..][..n as usize];
        // SAFETY: both output slices have n entries. Each group starts at
        // zero so the native total ends exactly at its unpadded final row.
        unsafe {
            host.device.GetCopyableFootprints(&d, native, n, 0, Some(out.as_mut_ptr()),
                Some(rows[dest as usize..].as_mut_ptr()), None, Some(&mut bytes));
        }
        if bytes == u64::MAX || bytes == 0 { return None; }
        for fp in out { fp.Offset = fp.Offset.checked_add(base)?; }
        total = base.checked_add(bytes)?;
    }
    let speculative = PICKING.get().is_some() && !BAKING.get();
    let storage = if ahead {
        let (piece, buffer, base, data) = match crate::pool::staging(host, total, crate::pool::StagingUse::Detile) {
            Some((piece, buffer, base, data)) => (Some(piece), buffer, base, data),
            None => {
                if crate::residency::upload_refused()
                    || (speculative && !crate::residency::allow_staging_growth(false))
                {
                    crate::residency::note_staging_deferred();
                    return None;
                }
                let (buffer, data) = host.upload_buffer(total)?;
                (None, buffer, 0, data)
            }
        };
        StageStorage::Upload { buffer, base, data, piece }
    } else {
        if speculative && !crate::residency::allow_staging_growth(false) {
            crate::residency::note_staging_deferred();
            return None;
        }
        usize::try_from(total).ok()?;
        StageStorage::Cpu(Vec::new())
    };
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    STAGED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    STAGED[1].fetch_add(storage.allocated(total), std::sync::atomic::Ordering::Relaxed);
    let linear_null = !on_demand && linear_1d_null_sample(&layout, desc.shape()[2],
        d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 != 0);
    let s = Arc::new(Staging {
        storage: Mutex::new(storage),
        footprints,
        rows,
        layout,
        mips,
        first,
        level0,
        read,
        serial: copy.serial,
        holds: copy.holds.clone(),
        bytes: total,
        shape: desc.shape(),
        memory,
        last: Mutex::default(),
        parts: Mutex::default(),
        dirty: std::sync::atomic::AtomicBool::new(false),
        uploaded: std::sync::atomic::AtomicBool::new(false),
        used: std::sync::atomic::AtomicU64::new(crate::host::moment()),
        on_demand,
        sparse_image: (on_demand && null_sample_block(desc.shape()[2]).is_some()
            && crate::inplace::sparse_range(memory as usize, read)).then(|| Arc::downgrade(copy)),
        linear_null,
        native_null: Default::default(),
        null_levels: Default::default(),
        detiled: std::sync::atomic::AtomicU32::new(0),
        judged: [const { std::sync::atomic::AtomicU64::new(0) }; 2],
        waiting: std::sync::atomic::AtomicU64::new(0),
    });
    // A closed list can still name an older pinned native generation after
    // another recording installed the replacement in the shared staging cache.
    if map.get(&(resource as usize)).is_none_or(|current| current.serial <= copy.serial) {
        map.insert(resource as usize, s.clone());
    }
    Some(s)
}

#[cfg(test)]
mod title_memory_tests {
    use super::with_title;

    #[test]
    fn bvh_declarations_require_every_page_even_after_protection_changes() {
        use windows_sys::Win32::System::Memory::*;
        const PAGE: usize = 4096;
        unsafe {
            let p = VirtualAlloc(std::ptr::null(), 3 * PAGE, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE).cast::<u8>();
            assert!(!p.is_null());
            // Fresh committed pages need not have working-set entries.
            assert!(super::committed_declaration(p as usize + 8, 2 * PAGE));
            p.write_bytes(1, 3 * PAGE);
            assert!(super::committed_declaration(p as usize + 8, 2 * PAGE));
            let mut old = 0;
            assert_ne!(VirtualProtect(p.add(PAGE).cast(), PAGE, PAGE_NOACCESS, &mut old), 0);
            assert!(!super::committed_declaration(p as usize + 8, 2 * PAGE));
            assert!(super::committed_declaration(p as usize, PAGE));
            assert_ne!(VirtualFree(p.add(2 * PAGE).cast(), PAGE, MEM_DECOMMIT), 0);
            assert!(!super::committed_declaration(p as usize + 2 * PAGE, 1));
            assert_ne!(VirtualFree(p.cast(), 0, MEM_RELEASE), 0);
        }
    }

    /// Title memory whose tail is reserved and not committed reads through
    /// the copy as its bytes and then zeros, without a fault; so does
    /// memory a page of which is taken away between two reads, and the
    /// zeros are zeros whatever the read buffer held before.
    #[test]
    fn an_uncommitted_tail_reads_as_zeros() {
        use windows_sys::Win32::System::Memory::*;
        const HEAD: usize = 0xb_0000;
        const SIZE: usize = 0x20_0000;
        // SAFETY: a private reservation of our own; only committed pages
        // are written in place, and nothing else reads them in place.
        unsafe {
            // The read buffer holds something else first.
            let other = vec![0x77u8; SIZE];
            with_title(other.as_ptr(), SIZE, |bytes| assert!(bytes.iter().all(|&b| b == 0x77)));
            let base = VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(VirtualAlloc(base.cast(), HEAD, MEM_COMMIT, PAGE_READWRITE), base.cast());
            base.write_bytes(0x5a, HEAD);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..HEAD].iter().all(|&b| b == 0x5a), "what is mapped");
                assert!(bytes[HEAD..].iter().all(|&b| b == 0), "what is not: zeros");
            });
            // The title takes a page in the middle away: zeros from then
            // on, the rest as before.
            assert_ne!(VirtualFree(base.add(0x4_0000).cast(), 0x1_0000, MEM_DECOMMIT), 0);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..0x4_0000].iter().all(|&b| b == 0x5a));
                assert!(bytes[0x4_0000..0x5_0000].iter().all(|&b| b == 0));
                assert!(bytes[0x5_0000..HEAD].iter().all(|&b| b == 0x5a));
                assert!(bytes[HEAD..].iter().all(|&b| b == 0));
            });
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    /// The texture both device-loss runs named last (16 texels wide, 4 or 8
    /// high, format 10, layout 0x100, console flags 0x300000; its memory
    /// began 0x300 into a page in run 73 and 0x700 in run 72, and here also
    /// 0xf00, so that a page line runs through it): what the detiling is
    /// given and what it writes are the same read in place and read through
    /// the copy, and the rows it writes end where the staging buffer the
    /// host sizes for it ends (896 bytes for 4 rows of 128 bytes 256 apart,
    /// 1,920 for 8).
    #[test]
    fn the_small_texture_of_runs_72_and_73_detiles_the_same_both_ways() {
        use windows_sys::Win32::System::Memory::*;
        for (height, within) in [(4u32, 0x300usize), (8, 0x700), (4, 0xf00), (8, 0xf00)] {
            // The description as the log printed it, word for word.
            let words: [u32; 14] = [3, 0, 0, 0, 16, 0, height, 65537, 10, 1, 0, 256, 3_145_728, 0];
            // SAFETY: fourteen words are a resource description (56 bytes,
            // plain numbers).
            let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(words) };
            let layout = crate::resource::texture_layout(&desc)
                .expect("a console layout")
                .planes[0]
                .layout
                .clone();
            let size = layout.size as usize;
            let (row, pitch) = (16 * 8, 256usize);
            assert!(size >= row * height as usize, "the layout holds every texel");
            // Committed pages and a reserved one after them; the texture
            // begins `within` bytes into the first, as the title placed it.
            // SAFETY: our own reservation; the committed part is written
            // and read in place, by this thread alone.
            unsafe {
                let pages = size.div_ceil(4096) + 1;
                let base = VirtualAlloc(std::ptr::null(), (pages + 1) * 4096, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
                assert!(!base.is_null());
                assert_eq!(VirtualAlloc(base.cast(), pages * 4096, MEM_COMMIT, PAGE_READWRITE), base.cast());
                for i in 0..pages * 4096 {
                    base.add(i).write((i * 7 + i / 251) as u8);
                }
                let memory = base.add(within);
                let in_place = std::slice::from_raw_parts(memory, size);
                // The bytes: the same in place and through the copy.
                with_title(memory, size, |copied| assert_eq!(copied, in_place));
                // What the detiling writes: the same, and inside the bytes
                // the host's staging buffer for this texture has.
                let staged = (height as usize - 1) * pitch + row;
                assert_eq!(staged, if height == 4 { 896 } else { 1920 });
                let detile = |tiled: &[u8]| {
                    // The slice `Staging::detile` hands over: whole rows.
                    let mut linear = vec![0xeeu8; pitch * height as usize];
                    layout
                        .detile(tiled, 0, 0, &mut linear, pitch, pitch * height as usize)
                        .expect("detiled");
                    linear
                };
                let direct = detile(in_place);
                let through = with_title(memory, size, |copied| detile(copied));
                assert_eq!(direct, through);
                for y in 0..height as usize {
                    assert!(direct[y * pitch..][..row].iter().any(|&b| b != 0xee), "row {y} written");
                    assert!(direct[y * pitch + row..(y + 1) * pitch].iter().all(|&b| b == 0xee), "nothing past row {y}");
                }
                // A buffer of exactly the staged bytes is enough.
                let mut exact = vec![0u8; staged];
                layout
                    .detile(in_place, 0, 0, &mut exact, pitch, pitch * height as usize)
                    .expect("the staging buffer's bytes are enough");
                assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
            }
        }
    }
}

#[cfg(test)]
mod prelude_upload_tests {
    use super::claim_upload;
    use std::sync::atomic::{AtomicBool, Ordering::Release};

    #[test]
    fn a_placed_srv_requires_only_its_guest_mips_and_layers() {
        use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};
        let layout = Layout::new(&Desc { dimension: Dimension::Tex2d, width: 512, height: 256,
            depth: 2, mips: 9, element_bytes: 4, block: 1, samples: 1, swizzle: Swizzle::SW_4KB_S }).unwrap();
        let view = super::ViewRange { mip: 1, levels: 1, slice: 1, slices: 1 };
        let selected = view.storage_runs(&layout).unwrap();
        assert_eq!(selected, layout.subresource_runs(1, 1).unwrap().collect::<Vec<_>>());
        for r in layout.subresource_runs(0, 1).unwrap().chain(layout.subresource_runs(1, 0).unwrap()) {
            assert!(selected.iter().all(|s| s.end <= r.start || r.end <= s.start));
        }
        let tail = super::ViewRange { mip: layout.first_tail_mip, levels: u32::MAX, ..view };
        assert_eq!(tail.storage_runs(&layout).unwrap(), layout.subresource_runs(layout.first_tail_mip, 1).unwrap().collect::<Vec<_>>());
    }

    #[test]
    fn view_can_be_supplied_by_several_ranges_of_the_same_image() {
        use super::ViewRange;
        let view = ViewRange { mip: 1, levels: u32::MAX, slice: 0, slices: 2 };
        let large = ViewRange { levels: 2, ..view };
        let small = ViewRange { mip: 3, ..view };
        let ranges = [large, small];
        assert!(view.first_missing(5, 2, |p| ranges.iter().any(|r| r.includes(p.mip, p.slice))).is_none());
        assert_eq!(view.first_missing(5, 2, |p| large.includes(p.mip, p.slice)),
            Some(ViewRange { mip: 3, levels: 1, slice: 0, slices: 1 }));
    }

    #[test]
    fn partial_array_footprints_skip_large_native_mips_in_every_slice() {
        assert_eq!(super::footprint_runs(5, 2, 3).collect::<Vec<_>>(), [(2, 0, 3), (7, 3, 3), (12, 6, 3)]);
        assert_eq!(super::footprint_runs(5, 0, 3).collect::<Vec<_>>(), [(0, 0, 15)]);
    }

    #[test]
    fn sparse_snapshots_keep_only_readable_spans_and_compare_at_guest_offsets() {
        let tiled = (0..4096).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        let mut previous = super::Previous::default();
        previous.capture(&tiled, None);
        assert_eq!(previous.get(0..4096), Some(tiled.as_slice()));
        previous.capture(&tiled, Some(vec![4090..4096, 100..115, 106..118, 115..119]));
        assert_eq!(previous.bytes.len(), 25);
        assert!(previous.bytes.capacity() < 4096);
        assert_eq!(previous.get(102..118), Some(&tiled[102..118]));
        assert_eq!(previous.get(4090..4096), Some(&tiled[4090..4096]));
        assert_eq!(previous.get(118..120), None);
        assert_eq!(previous.get(0..3), None);
        previous.capture(&tiled, Some(vec![32..40]));
        assert_eq!(previous.get(4090..4096), None);
        assert_eq!(previous.get(32..40), Some(&tiled[32..40]));
        previous.capture(&tiled, None);
        assert_eq!(previous.get(4080..4096), Some(&tiled[4080..4096]));
    }

    #[test]
    fn sparse_r16_unorm_uses_zero_texels_for_explicit_null_pages() {
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R16_TYPELESS, DXGI_FORMAT_R16_UNORM};

        let format = DXGI_FORMAT_R16_UNORM.0 as u32;
        let block = super::null_sample_block(format).unwrap();
        assert_eq!(block.len(), 2);
        let mut texels = [0xff; 8];
        super::fill_null_blocks(&mut texels, block);
        assert_eq!(texels, [0; 8]);
        assert_eq!(super::null_sample_block(DXGI_FORMAT_R16_TYPELESS.0 as u32), Some(block));
    }

    #[test]
    fn linear_1d_null_texels_require_an_unbiased_color_format_and_single_subresource() {
        use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};
        let desc = Desc { dimension: Dimension::Tex1d, width: 129, height: 1, depth: 1,
            mips: 1, element_bytes: 4, block: 1, samples: 1, swizzle: Swizzle::LINEAR };
        let layout = Layout::new(&desc).unwrap();
        for format in (27..=39).chain(41..=43) {
            assert!(super::linear_1d_null_sample(&layout, format, false));
        }
        for format in [24, 40, 87, 88, 89] {
            assert!(!super::linear_1d_null_sample(&layout, format, false));
        }
        assert!(!super::linear_1d_null_sample(&layout, 39, true));
        for other in [Desc { dimension: Dimension::Tex2d, ..desc }, Desc { depth: 2, ..desc },
            Desc { mips: 2, ..desc }, Desc { swizzle: Swizzle::SW_4KB_S, ..desc }]
        {
            assert!(!super::linear_1d_null_sample(&Layout::new(&other).unwrap(), 28, false));
        }
    }

    #[test]
    fn sparse_texel_capture_keeps_the_mapped_bytes_beside_a_declared_null_page() {
        use windows_sys::Win32::System::Memory::*;
        const PAGE: usize = 0x10000;
        // SAFETY: this private reservation is committed and released only here.
        unsafe {
            let at = VirtualAlloc(std::ptr::null(), 2 * PAGE, MEM_RESERVE, PAGE_NOACCESS) as usize;
            assert_ne!(at, 0);
            crate::inplace::title_placeholder_reserved(at, 2 * PAGE);
            let mapped = (at + PAGE) as *mut u8;
            assert_eq!(VirtualAlloc(mapped.cast(), PAGE, MEM_COMMIT, PAGE_READWRITE), mapped.cast());
            mapped.write_bytes(0x5a, PAGE);
            let start = at + PAGE - 256;
            let mut output = [0x77; 768];
            {
                let _mapping = crate::watch::title_memory_reading();
                assert_eq!(super::capture_sparse_blocks(start, start, &mut output, &[0; 4]), None);
                crate::inplace::title_gpu_mapping_ranges(&[(at as u64, 0, PAGE as u64)]);
                assert_eq!(super::capture_sparse_blocks(start, start, &mut output[..256], &[0; 4]), Some(0));
                assert_eq!(super::capture_sparse_blocks(start, start, &mut output, &[0; 4]), Some(512));
            }
            assert!(crate::inplace::release_title_placeholder(at, 0,
                || VirtualFree(at as *mut _, 0, MEM_RELEASE) != 0));
            assert_eq!(output[..256], [0; 256]);
            assert_eq!(output[256..], [0x5a; 512]);
        }
    }

    #[test]
    fn only_the_outstanding_upload_can_make_a_cached_part_retryable() {
        let part = std::sync::Mutex::new(super::StagedPart {
            content: Some((super::Digest(42), true)), dirty: false, pending: 2, captured: None,
        });
        std::thread::scope(|scope| {
            scope.spawn(|| part.lock().unwrap().resolve_upload(1, false));
            part.lock().unwrap().resolve_upload(2, true);
        });
        let mut part = part.into_inner().unwrap();
        assert_eq!((part.pending, part.dirty), (0, false));
        part.pending = 3;
        part.resolve_upload(3, false);
        assert_eq!((part.pending, part.dirty), (0, true));
    }

    #[test]
    fn first_upload_clean_skip_and_immediate_same_frame_reupload() {
        let uploaded = AtomicBool::new(false);
        let dirty = AtomicBool::new(true);

        // Frame 1: first upload runs after refresh() detiled into staging.
        assert_eq!(claim_upload(&uploaded, &dirty), Some(true));
        // Second list in the same submission skips redundant copy.
        assert_eq!(claim_upload(&uploaded, &dirty), None);

        // Frame 2: the texture is unchanged, so refresh and the prelude have no work.
        assert_eq!(claim_upload(&uploaded, &dirty), None);

        // Frame 3: refresh detiles the changed texture before claim_upload,
        // so upload is claimed on the exact same frame (0 frames late).
        dirty.store(true, Release);
        assert_eq!(claim_upload(&uploaded, &dirty), Some(false));

        // Frame 4: consecutive change (e.g. refresh_front() on every frame)
        // must also upload immediately, never alternating upload/skip.
        dirty.store(true, Release);
        assert_eq!(claim_upload(&uploaded, &dirty), Some(false));
    }

    #[test]
    fn three_buffer_ring_never_displays_stale_frame() {
        let ring: [(AtomicBool, AtomicBool); 3] =
            std::array::from_fn(|_| (AtomicBool::new(false), AtomicBool::new(true)));
        let mut gpu_frame = [0u32; 3];

        // 12 decoded frames cycling through the 3-buffer ring (like Bink intro),
        // with frame 5 repeated once to exercise the clean-frame transition.
        let schedule = [
            (0, 1, true),
            (1, 2, true),
            (2, 3, true),
            (0, 4, true),
            (1, 5, true),
            (1, 5, false), // held frame: unchanged
            (2, 6, true),
            (0, 7, true),
            (1, 8, true),  // immediately after held frame: must NOT be stale!
            (2, 9, true),
            (0, 10, true),
            (1, 11, true),
        ];

        for (slot, video_frame, changed) in schedule {
            let (uploaded, dirty) = &ring[slot];
            if changed {
                dirty.store(true, Release);
            }
            if claim_upload(uploaded, dirty).is_some() {
                gpu_frame[slot] = video_frame;
            }
            assert_eq!(
                gpu_frame[slot], video_frame,
                "slot {slot} displayed stale frame {} instead of {video_frame}",
                gpu_frame[slot]
            );
        }
    }
}

/// How long this PC's card takes to make the allocations a staged texture needs, for
/// `cargo test -p victory-gpu allocation_costs -- --ignored --nocapture`.
#[cfg(test)]
mod allocation_costs {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    fn texture(size: u32) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(size),
            Height: size,
            DepthOrArraySize: 1,
            MipLevels: 0,
            Format: DXGI_FORMAT_BC7_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        }
    }

    fn buffer(size: u64) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        }
    }

    fn committed(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, flags: D3D12_HEAP_FLAGS, desc: &D3D12_RESOURCE_DESC) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() };
        let state = if kind == D3D12_HEAP_TYPE_UPLOAD { D3D12_RESOURCE_STATE_GENERIC_READ } else { D3D12_RESOURCE_STATE_COMMON };
        let mut out = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, flags, desc, state, None, &mut out) }.expect("committed");
        out.unwrap()
    }

    fn timed(what: &str, n: usize, threads: usize, make: &(dyn Fn(usize) -> ID3D12Resource + Sync)) {
        let began = std::time::Instant::now();
        let all: Vec<_> = std::thread::scope(|s| {
            (0..threads)
                .map(|t| s.spawn(move || (t..n).step_by(threads).map(make).collect::<Vec<_>>()))
                .collect::<Vec<_>>()
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });
        let took = began.elapsed().as_secs_f64() * 1e3;
        println!("{what}, {threads} thread(s): {n} in {took:.1} ms, {:.0} us each", took * 1e3 / n as f64);
        drop(all);
    }

    /// What a look at how much of a texture is there costs: by questions to
    /// the host, and by the runtime's record of its views.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn look_costs() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        const BLOCK: usize = 64 << 10;
        for (name, blocks, there) in [("350 KB, all there", 6, 6), ("22 MB, all there", 350, 350), ("22 MB, 1 MB there", 350, 16)] {
            let make = || {
                // SAFETY: a reservation of our own, of which the first part
                // is committed and touched.
                unsafe {
                    let at = VirtualAlloc(std::ptr::null(), blocks * BLOCK, MEM_RESERVE, PAGE_READWRITE);
                    VirtualAlloc(at, there * BLOCK, MEM_COMMIT, PAGE_READWRITE);
                    std::ptr::write_bytes(at.cast::<u8>(), 1, there * BLOCK);
                    at as usize
                }
            };
            let (asked, noted) = (make(), make());
            for block in 0..there {
                crate::title_memory_view(noted + block * BLOCK, BLOCK, 9, 0);
            }
            for (how, at) in [("asking the host", asked), ("by the views", noted)] {
                const N: u32 = 200;
                let began = std::time::Instant::now();
                for _ in 0..N {
                    assert_eq!(super::mapped_bytes(at as *const u8, blocks * BLOCK), (there * BLOCK) as u64);
                }
                println!("{name}, {how}: {:.1} us a look", began.elapsed().as_secs_f64() * 1e6 / f64::from(N));
            }
            crate::inplace::views_gone(noted, blocks * BLOCK);
        }
    }

    /// The digest tells every length and every changed byte apart, zeros
    /// among them.
    #[test]
    fn a_digest_tells_bytes_apart() {
        use super::digest;
        let zeros = [0u8; 400];
        let mut seen: Vec<super::Digest> = (0..400).map(|n| digest(&zeros[..n])).collect();
        for at in 0..400 {
            for value in [1u8, 0x80, 0xff] {
                let mut bytes = zeros;
                bytes[at] = value;
                seen.push(digest(&bytes));
            }
        }
        let text: Vec<u8> = (0..400u32).map(|i| ((i * 37) >> 2) as u8).collect();
        for at in 0..399 {
            let mut bytes = text.clone();
            bytes.swap(at, at + 1);
            if bytes != text {
                seen.push(digest(&bytes));
            }
        }
        seen.push(digest(&text));
        let n = seen.len();
        seen.sort_unstable_by_key(|d| d.0);
        seen.dedup();
        assert_eq!(seen.len(), n);
        assert_eq!(digest(&text), digest(&text.clone()));
    }

    /// What a texture met for the first time costs, by step, in
    /// milliseconds a megabyte on one thread: for whether a store of
    /// detiled copies on disk could be worth it (its key is a hash of the
    /// title's bytes, so reading and hashing them stay; `VICTORY_BENCH_FILE`
    /// is a big file to read past the system's cache).
    #[test]
    #[ignore = "a measurement, not a check"]
    fn cold_texture_costs() {
        let Some(host) = crate::host::host() else { return };
        let desc = [3u32, 0, 0, 0, 1024, 0, 1024, 0xb_0001, 98, 1, 0, 0x105, 0, 0];
        // SAFETY: 56 bytes in the description's layout.
        let desc = unsafe { desc.as_ptr().cast::<crate::resource::ResourceDesc>().read_unaligned() };
        let layout = crate::resource::texture_layout(&desc).unwrap().planes[0].layout.clone();
        let size = layout.size as usize;
        let title: Vec<u8> = (0..size).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let (_buffer, up) = host.upload_buffer(2 * layout.size).unwrap();
        // SAFETY: the mapping.
        let up = unsafe { std::slice::from_raw_parts_mut(up, 2 * size) };
        let mut read = vec![0u8; size];
        const N: u32 = 200;
        let per_mb = |t: std::time::Duration| t.as_secs_f64() * 1e3 / (f64::from(N) * size as f64 / 1e6);
        let timed = |what: &str, f: &mut dyn FnMut()| {
            f();
            let started = std::time::Instant::now();
            (0..N).for_each(|_| f());
            println!("{what}: {:.3} ms/MB", per_mb(started.elapsed()));
        };
        timed("read through the kernel or a safe copy", &mut || {
            std::hint::black_box(crate::host::read_into(title.as_ptr(), &mut read));
        });
        timed("blake3", &mut || {
            std::hint::black_box(blake3::hash(&title));
        });
        timed("digest", &mut || {
            std::hint::black_box(super::digest(&title));
        });
        timed("detiled into upload memory", &mut || {
            let mut at = 0;
            for (mip, m) in layout.mips.iter().enumerate() {
                let pitch = (m.width as usize * 16).next_multiple_of(256);
                let len = pitch * m.height as usize;
                layout.detile(&title, mip as u32, 0, &mut up[at..at + len], pitch, len).unwrap();
                at += len.next_multiple_of(512);
            }
        });
        timed("copied into upload memory", &mut || up[..size].copy_from_slice(&title));
        if let Some(file) = std::env::var_os("VICTORY_BENCH_FILE") {
            use std::os::windows::fs::{FileExt, OpenOptionsExt};
            const NO_BUFFERING: u32 = 0x2000_0000;
            let file = std::fs::OpenOptions::new().read(true).custom_flags(NO_BUFFERING).open(file).unwrap();
            let len = file.metadata().unwrap().len();
            let mut block = vec![0u8; size + 4096];
            let skew = block.as_ptr().align_offset(4096);
            let n = size / 4096 * 4096;
            let mut at = 0u64;
            timed("read from the disk, one request at a time", &mut || {
                at = (at + 7919 * 4096 * 64) % (len - n as u64) / 4096 * 4096;
                file.seek_read(&mut block[skew..skew + n], at).unwrap();
            });
        }
    }

    #[test]
    #[ignore = "a measurement, not a check"]
    fn allocation_costs() {
        let Some(host) = crate::host::host() else { return };
        let device = &host.device;
        let not_zeroed = D3D12_HEAP_FLAG_CREATE_NOT_ZEROED;
        const N: usize = 1000;
        for size in [128u32, 512, 1024] {
            let (t, b) = (texture(size), buffer(u64::from(size * size) * 4 / 3 + 65536));
            println!("-- BC7 {size}x{size}");
            for threads in [1, 4, 8] {
                timed("texture committed", N, threads, &|_| committed(device, D3D12_HEAP_TYPE_DEFAULT, D3D12_HEAP_FLAG_NONE, &t));
                timed("texture committed not zeroed", N, threads, &|_| committed(device, D3D12_HEAP_TYPE_DEFAULT, not_zeroed, &t));
                let mapped = |flags| {
                    let b = &b;
                    move |_| {
                        let r = committed(device, D3D12_HEAP_TYPE_UPLOAD, flags, b);
                        let mut data = std::ptr::null_mut();
                        // SAFETY: a live upload buffer.
                        unsafe { r.Map(0, None, Some(&mut data)) }.unwrap();
                        // SAFETY: the first byte of the mapping (its first touch).
                        unsafe { data.cast::<u8>().write_volatile(1) };
                        r
                    }
                };
                timed("staging committed and mapped", N, threads, &mapped(D3D12_HEAP_FLAG_NONE));
                timed("staging committed and mapped, not zeroed", N, threads, &mapped(not_zeroed));
            }
            // SAFETY: plain query.
            let info = unsafe { device.GetResourceAllocationInfo(0, &[t]) };
            let per_heap = ((64u64 << 20) / info.SizeInBytes).max(1) as usize;
            let heaps: Vec<ID3D12Heap> = (0..N.div_ceil(per_heap))
                .map(|_| {
                    let desc = D3D12_HEAP_DESC {
                        SizeInBytes: per_heap as u64 * info.SizeInBytes,
                        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | not_zeroed,
                        ..Default::default()
                    };
                    let mut heap = None;
                    // SAFETY: plain creation.
                    unsafe { device.CreateHeap(&desc, &mut heap) }.expect("heap");
                    heap.unwrap()
                })
                .collect();
            let place = |i: usize| {
                let mut out: Option<ID3D12Resource> = None;
                let at = (i % per_heap) as u64 * info.SizeInBytes;
                // SAFETY: a part of a live heap no other texture has.
                unsafe { device.CreatePlacedResource(&heaps[i / per_heap], at, &t, D3D12_RESOURCE_STATE_COMMON, None, &mut out) }.expect("placed");
                out.unwrap()
            };
            timed("texture placed in 64 MB heaps", N, 1, &place);
            timed("texture placed in 64 MB heaps", N, 8, &place);
        }
    }
}
