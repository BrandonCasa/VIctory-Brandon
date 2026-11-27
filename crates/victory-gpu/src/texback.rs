//! Textures shaders write that the title's own code then reads on the CPU.
//! On the console the CPU and the GPU share memory, so what a compute pass
//! stores into a texture is in title memory for the CPU to read. Here it stays
//! in the texture's host copy. The title's ocean is one: its wave passes write
//! a 256x256 height map (R32_FLOAT) and a displacement map (R32G32_FLOAT), and
//! its CPU code samples both, which is what boats float on; it read memory
//! nothing had written (run rw1, `VICTORY_READ_WATCH=1`: about 30 places in
//! the title read those two kinds), and boats sank.
//!
//! For textures of the kinds in `VICTORY_TEXTURE_BACK` (comma-separated
//! `<width>x<height>f<format>`; unset or `1`: the four water formats found by
//! the upstream trace; `0`: none), a list in which shaders write one copies it
//! into a read-back buffer of ours when it closes, and once the list has run
//! the texels are tiled into the texture's title memory in the console's
//! layout, as the console's GPU would have left them.
//!
//! shortcut: by kind, not by texture: textures of those kinds the title never
//! reads are copied back too (about a megabyte per such list). The copy shows
//! what the list's last write left; a title read in between sees the frame
//! before, as it would before the console's GPU has run.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::Win32::Graphics::Direct3D12::*;

use crate::object::This;

/// The kinds copied back: (width, height, format).
fn kinds() -> &'static [(u64, u32, u32)] {
    static KINDS: OnceLock<Vec<(u64, u32, u32)>> = OnceLock::new();
    KINDS.get_or_init(|| {
        const DEFAULT: &str = "256x256f41,256x256f16,64x64f16,128x128f36";
        let list = match std::env::var("VICTORY_TEXTURE_BACK").as_deref() {
            Ok("1") | Err(_) => DEFAULT.to_owned(),
            Ok(spec) => spec.to_owned(),
        };
        let kinds: Vec<(u64, u32, u32)> = list
            .split(',')
            .filter_map(|k| {
                let (size, format) = k.trim().split_once('f')?;
                let (w, h) = size.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?, format.parse().ok()?))
            })
            .collect();
        if !kinds.is_empty() {
            tracing::info!(?kinds, "textures of these kinds that shaders write are copied back into title memory (VICTORY_TEXTURE_BACK)");
        }
        kinds
    })
}

/// Whether title texture `resource` is of a kind copied back.
pub(crate) fn wanted(resource: This) -> bool {
    let kinds = kinds();
    crate::resource::title_fills(resource) || !kinds.is_empty()
        && crate::resource::texture_kind(resource).is_some_and(|k| kinds.contains(&k))
}

/// An outstanding write to a specific allocation. Mapping and resource
/// changes invalidate only the writes that could touch the replaced memory.
struct Ticket {
    owner: usize,
    at: usize,
    end: usize,
    valid: AtomicBool,
}

struct Publication {
    ticket: Arc<Ticket>,
    /// Absolute guest byte ranges. A full allocation has a single range.
    bytes: Vec<std::ops::Range<usize>>,
}

static PUBLISHED: std::sync::LazyLock<Mutex<std::collections::HashMap<usize, Publication>>> =
    std::sync::LazyLock::new(Default::default);

static GPU_WRITTEN: std::sync::LazyLock<Mutex<std::collections::HashSet<usize>>> =
    std::sync::LazyLock::new(Default::default);

pub(crate) fn written(owner: usize) {
    GPU_WRITTEN.lock().unwrap_or_else(|e| e.into_inner()).insert(owner);
    PUBLISHED.lock().unwrap_or_else(|e| e.into_inner()).remove(&owner);
}

pub(crate) fn available(owner: usize, lo: usize, hi: usize) -> bool {
    if !GPU_WRITTEN.lock().unwrap_or_else(|e| e.into_inner()).contains(&owner) { return true; }
    PUBLISHED.lock().unwrap_or_else(|e| e.into_inner()).get(&owner)
        .is_some_and(|p| {
            let i = p.bytes.partition_point(|r| r.end <= lo);
            p.ticket.valid.load(Ordering::Acquire) && p.bytes.get(i).is_some_and(|r| r.start <= lo && hi <= r.end)
        })
}

fn publish(back: &Back, bytes: Vec<std::ops::Range<usize>>) {
    PUBLISHED.lock().unwrap_or_else(|e| e.into_inner())
        .insert(back.ticket.owner, Publication { ticket: back.ticket.clone(), bytes });
}

static TICKETS: Mutex<Vec<Weak<Ticket>>> = Mutex::new(Vec::new());
static TRACKING: AtomicBool = AtomicBool::new(false);

fn track(owner: usize, at: usize, end: usize) -> Arc<Ticket> {
    let ticket = Arc::new(Ticket { owner, at, end, valid: AtomicBool::new(true) });
    let mut tickets = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
    tickets.retain(|weak| weak.strong_count() != 0);
    tickets.push(Arc::downgrade(&ticket));
    TRACKING.store(true, Ordering::Relaxed);
    ticket
}

fn invalidate(matches: impl Fn(&Ticket) -> bool) {
    if !TRACKING.load(Ordering::Relaxed) {
        return;
    }
    TICKETS.lock().unwrap_or_else(|e| e.into_inner()).retain(|weak| {
        let Some(ticket) = weak.upgrade() else { return false };
        if matches(&ticket) {
            ticket.valid.store(false, Ordering::Release);
        }
        true
    });
}

pub(crate) fn released(owner: usize) {
    GPU_WRITTEN.lock().unwrap_or_else(|e| e.into_inner()).remove(&owner);
    PUBLISHED.lock().unwrap_or_else(|e| e.into_inner()).remove(&owner);
    invalidate(|ticket| ticket.owner == owner);
}

pub(crate) fn replaced(at: usize) {
    invalidate(|ticket| ticket.at == at);
}

pub(crate) fn remapped(at: usize, len: usize) {
    let end = at.saturating_add(len);
    invalidate(|ticket| at < ticket.end && ticket.at < end);
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Write {
    All,
    View(crate::texture::ViewRange),
    /// Native destination subresource and destination bounds in format elements.
    Region(u32, [u32; 6]),
}

pub(crate) struct Target {
    pub(crate) copy: Arc<crate::resource::HostTexture>,
    writes: Vec<Write>,
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
}

impl Target {
    pub(crate) fn new(resource: This, write: Write) -> Option<Self> {
        // Keep the host copy independently of the title's resource lifetime.
        let copy = crate::resource::host_copy(resource)?;
        let _mapping = crate::watch::title_memory_reading();
        let (layout, at, size) = crate::resource::title_texture(resource)?;
        let end = at.checked_add(size).filter(|&end| at != 0 && end > at)?;
        if crate::resource::stands_in(resource) || layout.desc.samples > 1 || layout.desc.swizzle.xor() {
            return None;
        }
        let ticket = track(resource as usize, at, end);
        Some(Self { copy, layout, ticket, writes: vec![write] })
    }

    pub(crate) fn include(&mut self, write: Write) {
        if !self.writes.contains(&write) { self.writes.push(write); }
    }

    pub(crate) fn owner(&self) -> usize { self.ticket.owner }
}

/// The readback resources and the allocation the pixels belong to.
pub(crate) struct Back {
    buffer: Lease,
    dense: bool,
    mips: u32,
    level0: u32,
    parts: Vec<Part>,
    whole: bool,
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
}

struct Part {
    index: usize,
    footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    rows: u32,
    origin: [u32; 3],
    source_box: Option<D3D12_BOX>,
    /// Guest format elements. Empty when the whole subresource was selected by a view.
    regions: Vec<[u32; 6]>,
}

struct Footprints {
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
}

struct Lease {
    buffer: Option<ID3D12Resource>,
    size: u64,
}
static IDLE: Mutex<(u64, Vec<(u64, ID3D12Resource)>)> = Mutex::new((0, Vec::new()));
#[cfg(feature = "diagnostics")]
pub(crate) static LIVE_READBACK_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Lease {
    fn new(size: u64) -> Option<Self> {
        let cached = {
            let mut idle = IDLE.lock().unwrap_or_else(|e| e.into_inner());
            let i = idle.1.iter().position(|(n, _)| *n == size);
            i.map(|i| {
                idle.0 -= size;
                idle.1.swap_remove(i).1
            })
        };
        let buffer = cached.or_else(|| crate::host::host()?.read_back_buffer(size))?;
        #[cfg(feature = "diagnostics")]
        LIVE_READBACK_BYTES.fetch_add(size, Ordering::Relaxed);
        Some(Self {
            buffer: Some(buffer),
            size,
        })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        #[cfg(feature = "diagnostics")]
        LIVE_READBACK_BYTES.fetch_sub(self.size, Ordering::Relaxed);
        let mut idle = IDLE.lock().unwrap_or_else(|e| e.into_inner());
        if idle.0 + self.size <= 64 << 20 {
            idle.0 += self.size;
            if let Some(b) = self.buffer.take() {
                idle.1.push((self.size, b));
            }
        }
    }
}

impl std::ops::Deref for Lease {
    type Target = ID3D12Resource;
    fn deref(&self) -> &ID3D12Resource {
        self.buffer.as_ref().unwrap()
    }
}

impl Back {
    pub(crate) fn range(&self) -> (usize, usize) {
        (self.ticket.at, self.ticket.end)
    }
}

/// Records a host copy from a texture already in COPY_SOURCE.
pub(crate) fn record(list: &ID3D12GraphicsCommandList, target: Target) -> Option<Back> {
    if !target.ticket.valid.load(Ordering::Acquire) {
        return None;
    }
    let host = crate::host::host()?;
    let texture = &target.copy.texture;
    // SAFETY: a live texture kept through the command list's state tracking.
    let desc = unsafe { texture.GetDesc() };
    let mips = u32::from(desc.MipLevels.max(1));
    let layers = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        1
    } else {
        u32::from(desc.DepthOrArraySize.max(1))
    };
    let subresources = mips * layers;
    type Shapes = Vec<(D3D12_RESOURCE_DESC, Arc<Footprints>)>;
    static SHAPES: Mutex<Shapes> = Mutex::new(Vec::new());
    let cached = SHAPES.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(d, _)| *d == desc).map(|(_, f)| f.clone());
    let shape = if let Some(f) = cached { f } else {
    let n = subresources as usize;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n];
    let mut rows = vec![0u32; n];
    // SAFETY: out arrays of `n` entries.
    unsafe {
        host.device.GetCopyableFootprints(
            &desc,
            0,
            subresources,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            None,
        )
    };
    let shape = Arc::new(Footprints { footprints, rows });
    let mut shapes = SHAPES.lock().unwrap_or_else(|e| e.into_inner());
    if shapes.len() >= 128 { shapes.remove(0); }
    shapes.push((desc, shape.clone()));
    shape
    };
    let guest = &target.layout;
    let mip = guest.mips.first()?;
    let f = shape.footprints[0];
    let row = u64::from(mip.width) * u64::from(guest.desc.element_bytes);
    let dense = guest.desc.swizzle == victory_formats::texture::Swizzle::LINEAR
        && subresources == 1
        && guest.mips.len() == 1
        && guest.slices == 1
        && mip.depth == 1
        && mip.offset == 0
        && guest.desc.block == 1
        && f.Footprint.Format == desc.Format
        && f.Footprint.Width == mip.width
        && f.Footprint.Height == mip.height
        && f.Footprint.Depth == 1
        && mip.pitch == mip.width
        && shape.rows[0] == mip.height
        && u64::from(f.Footprint.RowPitch) == row
        && row * u64::from(mip.height) == guest.size
        && guest.size == (target.ticket.end - target.ticket.at) as u64;
    let level0 = target.copy.base;
    let mut parts = Vec::new();
    let mut size = 0u64;
    for (i, &original) in shape.footprints.iter().enumerate() {
        let (mip, slice) = (level0 + i as u32 % mips, i as u32 / mips);
        let whole_sub = target.writes.iter().any(|w| matches!(w, Write::All)
            || matches!(w, Write::View(view) if view.includes(mip, slice)));
        let mut regions = Vec::new();
        let mut f = original.Footprint;
        let mut rows = shape.rows[i];
        let mut origin = [0; 3];
        let mut source_box = None;
        if !whole_sub {
            regions.extend(target.writes.iter().filter_map(|w| match w {
                Write::Region(sub, bounds) if *sub == i as u32 => Some(*bounds),
                _ => None,
            }));
            if regions.is_empty() { continue; }
            let m = guest.mips.get(mip as usize)?;
            let mut limit = [0; 3];
            origin = [u32::MAX; 3];
            for r in &regions {
                for a in 0..3 {
                    if r[a] >= r[a + 3] || r[a + 3] > [m.width, m.height, m.depth][a] { return None; }
                    origin[a] = origin[a].min(r[a]);
                    limit[a] = limit[a].max(r[a + 3]);
                }
            }
            let block = guest.desc.block;
            let b = D3D12_BOX {
                left: origin[0] * block, top: origin[1] * block, front: origin[2],
                right: (limit[0] * block).min(f.Width), bottom: (limit[1] * block).min(f.Height), back: limit[2],
            };
            // Full depth/stencil subresources must use a null source box.
            if origin != [0; 3] || limit != [m.width, m.height, m.depth] {
                f.Width = b.right.checked_sub(b.left)?;
                f.Height = b.bottom.checked_sub(b.top)?;
                f.Depth = b.back - b.front;
                f.RowPitch = (limit[0] - origin[0]).checked_mul(guest.desc.element_bytes)?
                    .checked_next_multiple_of(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT)?;
                rows = limit[1] - origin[1];
                source_box = Some(b);
            }
        }
        let offset = size.checked_next_multiple_of(u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT))?;
        // The tiler accesses the full pitch, including padding after the last row.
        let pitched = u64::from(f.RowPitch).checked_mul(u64::from(rows))?.checked_mul(u64::from(f.Depth))?;
        size = offset.checked_add(pitched)?;
        let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT { Offset: offset, Footprint: f };
        parts.push(Part { index: i, footprint, rows, origin, source_box, regions });
    }
    if parts.is_empty() { return None; }
    let buffer = Lease::new(size)?;
    for part in &parts {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&*buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: part.footprint },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: part.index as u32 },
        };
        // SAFETY: an open list; the texture is in the copy-source state.
        unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, part.source_box.as_ref().map(std::ptr::from_ref)) };
    }
    let whole = level0 == 0 && mips as usize == guest.mips.len()
        && (desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D || layers == guest.slices)
        && parts.len() == shape.footprints.len()
        && parts.iter().all(|p| p.regions.is_empty());
    Some(Back {
        buffer,
        dense,
        mips,
        level0,
        parts,
        whole,
        layout: target.layout,
        ticket: target.ticket,
    })
}

/// Partial copies can share both a mip and a tiled page with other writers.
/// Assemble only touched pages and write the exact byte runs within them.
fn land_regions(back: &Back, data: &[u8]) -> Option<Vec<std::ops::Range<usize>>> {
    use std::collections::BTreeMap;
    use std::ops::Range;
    const PAGE: usize = 4096;
    struct Page {
        bytes: Box<[u8; PAGE]>,
        runs: Vec<Range<usize>>,
    }
    let mut pages = BTreeMap::<usize, Page>::new();
    let elem = back.layout.desc.element_bytes as usize;
    let size = back.ticket.end - back.ticket.at;
    for part in &back.parts {
        let i = part.index;
        let mip = back.level0 + i as u32 % back.mips;
        let slice = i as u32 / back.mips;
        let m = back.layout.mips.get(mip as usize)?;
        let pitch = part.footprint.Footprint.RowPitch as usize;
        let depth_pitch = pitch.checked_mul(part.rows as usize)?;
        let full = [[0, 0, 0, m.width, m.height, m.depth]];
        let regions = if part.regions.is_empty() { &full[..] } else { &part.regions };
        for &region in regions {
            let mut valid = true;
            back.layout.region_runs(mip, slice, region, |mut dst, x, y, z, len| {
                let [left, top, front] = part.origin;
                let src = part.footprint.Offset as usize + (z - front) as usize * depth_pitch
                    + (y - top) as usize * pitch + (x - left) as usize * elem;
                let Some(mut bytes) = data.get(src..src.saturating_add(len)) else { valid = false; return };
                if dst.saturating_add(len) > size { valid = false; return; }
                while !bytes.is_empty() {
                    let start = dst % PAGE;
                    let n = (PAGE - start).min(bytes.len());
                    let page = pages.entry(dst - start).or_insert_with(|| Page {
                        bytes: Box::new([0; PAGE]), runs: Vec::new(),
                    });
                    page.bytes[start..start + n].copy_from_slice(&bytes[..n]);
                    page.runs.push(start..start + n);
                    bytes = &bytes[n..];
                    dst += n;
                }
            }).ok()?;
            if !valid { return None; }
        }
    }
    let mut covered: Vec<Range<usize>> = Vec::new();
    for (offset, mut page) in pages {
        let at = back.ticket.at + offset;
        page.runs.sort_unstable_by_key(|r| r.start);
        let mut runs = page.runs.into_iter().peekable();
        while let Some(mut r) = runs.next() {
            while runs.peek().is_some_and(|next| next.start <= r.end) {
                r.end = r.end.max(runs.next()?.end);
            }
            if !crate::host::write_mapped_exact(at + r.start, page.bytes[r.clone()].as_ptr(), r.len()) {
                return None;
            }
            if let Some(last) = covered.last_mut() && last.end == at + r.start {
                last.end = at + r.end;
            } else { covered.push(at + r.start..at + r.end); }
        }
    }
    for r in &covered { crate::inplace::notify_texture_write(r.start, r.len()); }
    Some(covered)
}

/// Once the list has run, tile its texels back into the same title allocation.
pub(crate) fn land(back: &Back) {
    static COPIED: AtomicU64 = AtomicU64::new(0);
    static STALE: AtomicU64 = AtomicU64::new(0);
    let _mapping = crate::watch::title_memory_reading();
    let _ticket_guard = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
    if !back.ticket.valid.load(Ordering::Acquire) {
        let skipped = STALE.fetch_add(1, Ordering::Relaxed) + 1;
        if skipped.is_power_of_two() {
            tracing::info!(skipped, "skipping texture readbacks whose title allocation changed");
        }
        return;
    }
    let (at, end) = back.range();
    let size = end - at;
    let mut data = std::ptr::null_mut();
    // SAFETY: our read-back buffer, written by the list that has run.
    unsafe {
        if back.buffer.Map(0, None, Some(&mut data)).is_err() || data.is_null() {
            crate::host::refuse_frame(format_args!("texture readback could not map completed bytes for {at:#x}"));
            return;
        }
    }
    if !back.whole {
        // SAFETY: this completed readback lease is mapped through the call.
        let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), back.buffer.size as usize) };
        let covered = land_regions(back, bytes);
        unsafe { back.buffer.Unmap(0, None) };
        if let Some(covered) = covered {
            publish(back, covered);
            COPIED.fetch_add(1, Ordering::Relaxed);
        } else {
            crate::host::refuse_frame(format_args!("could not land partial texture write at {at:#x}"));
        }
        return;
    }
    if back.dense {
        // SAFETY: record validated the full extent against this footprint.
        unsafe {
            let written = crate::host::write_mapped_exact(at, data.cast::<u8>(), size);
            back.buffer.Unmap(0, None);
            if !written {
                crate::host::refuse_frame(format_args!("texture readback could not write {at:#x}"));
                return;
            }
        }
        crate::inplace::notify_texture_write(at, size);
        publish(back, vec![at..end]);
        COPIED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // Read the slabs shared with other levels (notably the packed tail)
    // before replacing only the shader-addressable subresources.
    let parts = back.parts.iter().map(|p| (p, back.level0 + p.index as u32 % back.mips, p.index as u32 / back.mips));
    let mut spans = Vec::new();
    for (_, mip, slice) in parts.clone() {
        let Some(runs) = back.layout.subresource_runs(mip, slice) else {
            unsafe { back.buffer.Unmap(0, None) };
            crate::host::refuse_frame(format_args!("unknown readback layout at {at:#x}, mip {mip}, slice {slice}"));
            return;
        };
        spans.extend(runs);
    }
    spans.sort_unstable_by_key(|r| (r.start, r.end));
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for span in spans {
        if let Some(last) = merged.last_mut() && span.start <= last.end {
            last.end = last.end.max(span.end);
        } else {
            merged.push(span);
        }
    }
    let mut tiled = vec![0u8; size];
    for span in &merged {
        if !crate::host::read_into((at + span.start) as *const u8, &mut tiled[span.clone()]) {
            unsafe { back.buffer.Unmap(0, None) };
            crate::host::refuse_frame(format_args!("could not preserve texture backing at {:#x}", at + span.start));
            return;
        }
    }
    let mut conversion = back.layout.conversion();
    for (part, mip, slice) in parts {
        let offset = part.footprint.Offset;
        let rows = part.rows;
        let f = part.footprint.Footprint;
        let pitch = f.RowPitch as usize;
        let len = pitch * rows as usize * f.Depth as usize;
        // SAFETY: the subresource's part of the mapped buffer.
        let linear = unsafe { std::slice::from_raw_parts(data.cast::<u8>().add(offset as usize), len) };
        if let Err(e) = conversion.tile(&mut tiled, mip, slice, linear, pitch, pitch * rows as usize) {
            unsafe { back.buffer.Unmap(0, None) };
            crate::host::refuse_frame(format_args!("could not retile mip {mip}, slice {slice} at {at:#x} ({e:?})"));
            return;
        }
    }
    unsafe { back.buffer.Unmap(0, None) };
    for span in &merged {
        if !crate::host::write_mapped_exact(at + span.start, tiled[span.clone()].as_ptr(), span.len()) {
            crate::host::refuse_frame(format_args!("texture readback could not write {:#x}", at + span.start));
            return;
        }
        crate::inplace::notify_texture_write(at + span.start, span.len());
    }
    publish(back, merged.into_iter().map(|span| at + span.start..at + span.end).collect());
    let copied = COPIED.fetch_add(1, Ordering::Relaxed) + 1;
    if copied == 1 || copied.is_multiple_of(128) {
        tracing::info!(copied, bytes = size, "texture readback landed");
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_landing_vouches_only_for_contiguous_written_bytes() {
        let owner = 95;
        let ticket = track(owner, 0x8000, 0x9000);
        written(owner);
        PUBLISHED.lock().unwrap().insert(owner, Publication {
            ticket, bytes: vec![0x8110..0x8120, 0x8150..0x8180],
        });
        assert!(available(owner, 0x8114, 0x8120));
        assert!(!available(owner, 0x8114, 0x8154));
        assert!(!available(owner, 0x8000, 0x9000));
        remapped(0x8100, 0x10);
        assert!(!available(owner, 0x8114, 0x8120));
        released(owner);
    }

    #[test]
    fn only_readbacks_for_changed_allocations_are_invalidated() {
        let first = track(91, 0x1000, 0x2000);
        let next = track(92, 0x2000, 0x3000);
        let far = track(93, 0x4000, 0x5000);

        remapped(0x0f00, 0x200);
        assert!(!first.valid.load(Ordering::Acquire));
        assert!(next.valid.load(Ordering::Acquire));
        replaced(0x2000);
        assert!(!next.valid.load(Ordering::Acquire));
        assert!(far.valid.load(Ordering::Acquire));

        released(93);
        assert!(!far.valid.load(Ordering::Acquire));
        assert!(track(94, 0x2000, 0x3000).valid.load(Ordering::Acquire));
    }
}
