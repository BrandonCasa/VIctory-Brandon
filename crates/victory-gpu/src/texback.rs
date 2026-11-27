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
    invalidate(|ticket| ticket.owner == owner);
}

pub(crate) fn replaced(at: usize) {
    invalidate(|ticket| ticket.at == at);
}

pub(crate) fn remapped(at: usize, len: usize) {
    let end = at.saturating_add(len);
    invalidate(|ticket| at < ticket.end && ticket.at < end);
}

pub(crate) struct Target {
    pub(crate) copy: Arc<crate::resource::HostTexture>,
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
}

impl Target {
    pub(crate) fn new(resource: This) -> Option<Self> {
        // Keep the host copy independently of the title's resource lifetime.
        let copy = crate::resource::host_copy(resource)?;
        let _mapping = crate::watch::title_memory_reading();
        let (layout, at, size) = crate::resource::title_texture(resource)?;
        let end = at.checked_add(size).filter(|&end| at != 0 && end > at)?;
        let ticket = track(resource as usize, at, end);
        Some(Self { copy, layout, ticket })
    }

    pub(crate) fn owner(&self) -> usize { self.ticket.owner }
}

/// The readback resources and the allocation the pixels belong to.
pub(crate) struct Back {
    buffer: Lease,
    shape: Arc<Footprints>,
    dense: Option<usize>,
    mips: u32,
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
}

struct Footprints {
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    total: u64,
}

struct Lease {
    buffer: Option<ID3D12Resource>,
    size: u64,
}
static IDLE: Mutex<(u64, Vec<(u64, ID3D12Resource)>)> = Mutex::new((0, Vec::new()));

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
        Some(Self {
            buffer: Some(buffer),
            size,
        })
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
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
    let mut total = 0u64;
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
            Some(&mut total),
        )
    };
    // The last footprint's advertised extent can omit row-end padding.
    // The tiler reads a pitched slice, so the mapped buffer must include it.
    for (footprint, &rows) in footprints.iter().zip(&rows) {
        let format = footprint.Footprint;
        let pitched = u64::from(format.RowPitch)
            .checked_mul(u64::from(rows))?
            .checked_mul(u64::from(format.Depth))?;
        total = total.max(footprint.Offset.checked_add(pitched)?);
    }
    let shape = Arc::new(Footprints { footprints, rows, total });
    let mut shapes = SHAPES.lock().unwrap_or_else(|e| e.into_inner());
    if shapes.len() >= 128 { shapes.remove(0); }
    shapes.push((desc, shape.clone()));
    shape
    };
    let buffer = Lease::new(shape.total)?;
    let guest = &target.layout;
    let mip = guest.mips.first()?;
    let f = shape.footprints[0];
    let row = u64::from(mip.width) * u64::from(guest.desc.element_bytes);
    let dense = (guest.desc.swizzle == victory_formats::texture::Swizzle::LINEAR
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
        && guest.size == (target.ticket.end - target.ticket.at) as u64)
        .then_some(f.Offset as usize);
    for (i, footprint) in shape.footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&*buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *footprint },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: i as u32 },
        };
        // SAFETY: an open list; the texture is in the copy-source state.
        unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    Some(Back {
        buffer,
        shape,
        dense,
        mips,
        layout: target.layout,
        ticket: target.ticket,
    })
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
            return;
        }
    }
    if let Some(offset) = back.dense {
        // SAFETY: record validated the full extent against this footprint.
        unsafe {
            let written = crate::host::write_mapped_exact(at, data.cast::<u8>().add(offset), size);
            back.buffer.Unmap(0, None);
            if !written {
                crate::host::refuse_frame(format_args!("texture readback could not write {at:#x}"));
                return;
            }
        }
        crate::inplace::notify_texture_write(at, size);
        COPIED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut tiled = vec![0u8; size];
    if !crate::host::read_into(at as *const u8, &mut tiled) {
        unsafe { back.buffer.Unmap(0, None) };
        return;
    }
    let mut conversion = back.layout.conversion();
    for (i, (footprint, &rows)) in back
        .shape
        .footprints
        .iter()
        .zip(&back.shape.rows)
        .enumerate()
    {
        let (mip, slice) = (i as u32 % back.mips, i as u32 / back.mips);
        let f = footprint.Footprint;
        let pitch = f.RowPitch as usize;
        let len = (pitch * rows as usize * f.Depth as usize)
            .min((back.shape.total - footprint.Offset) as usize);
        // SAFETY: the subresource's part of the mapped buffer.
        let linear = unsafe { std::slice::from_raw_parts(data.cast::<u8>().add(footprint.Offset as usize), len) };
        if let Err(e) = conversion.tile(&mut tiled, mip, slice, linear, pitch, pitch * rows as usize) {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(?e, mip, slice, "a texture copied back could not be tiled"));
            // SAFETY: mapped above.
            unsafe { back.buffer.Unmap(0, None) };
            return;
        }
    }
    // SAFETY: mapped above.
    unsafe { back.buffer.Unmap(0, None) };
    if !crate::host::write_mapped_exact(at, tiled.as_ptr(), size) {
        crate::host::refuse_frame(format_args!("texture readback could not write {at:#x}"));
        return;
    }
    crate::inplace::notify_texture_write(at, size);
    let copied = COPIED.fetch_add(1, Ordering::Relaxed) + 1;
    if copied == 1 || copied.is_multiple_of(128) {
        tracing::info!(copied, bytes = size, "texture readback landed");
    }
}


#[cfg(test)]
mod tests {
    use super::*;

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
