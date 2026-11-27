//! Ranges declared at the Scarlett builder ABI, before translation into dispatches.
use crate::object::This;
use std::sync::OnceLock;

static BUILD: OnceLock<usize> = OnceLock::new();
static REHYDRATE_FN: OnceLock<usize> = OnceLock::new();
static PREBUILD: OnceLock<usize> = OnceLock::new();
static COPY: OnceLock<usize> = OnceLock::new();

// The title's BVH allocator owns blocks inside a much larger GPU heap.
// The public copy ABI contains only two addresses. A committed VM range,
// the parent buffer, or the size returned by the shader cannot give the
// capacity of a particular block inside that heap.
static TITLE_ALLOCATOR: OnceLock<usize> = OnceLock::new();

pub fn identify_allocator(base: usize, bytes: usize) {
    const QUERY: usize = 0x5092470;
    // Both the allocator vtable and the RIP-relative lock used by the size
    // query identify the retail implementation. An unfamiliar image can
    // still supply an ordinary registered destination buffer.
    if bytes >= 0x8420810
        && read::<8>((base + 0x61883a0 + 0x48) as u64) == Some(((base + QUERY) as u64).to_le_bytes())
        && read::<7>((base + QUERY + 12) as u64) == Some([0x48, 0x8d, 0x1d, 0x05, 0xd6, 0x12, 0x02])
    {
        TITLE_ALLOCATOR.get_or_init(|| base);
    }
}

/// On the calling title thread, under the allocator's own lock. The
/// allocator subtracts its heap base, looks up that exact offset in the
/// allocated-block hash, then returns record[4] (bytes), or zero on miss.
/// Within this allocator, a miss cannot fall back to the enclosing buffer.
fn compact_allocation(at: u64) -> Option<u64> {
    let base = *TITLE_ALLOCATOR.get()? as u64;
    let allocator = u64::from_le_bytes(read(base + 0x8420808)?);
    if u64::from_le_bytes(read(allocator)?) != base + 0x61883a0 {
        return None;
    }
    let start = u64::from_le_bytes(read(allocator.checked_add(0x20)?)?);
    let len = u64::from_le_bytes(read(allocator.checked_add(0x30)?)?);
    if at.checked_sub(start)? >= len {
        return None;
    }
    // SAFETY: the checked retail vtable names this Windows x64 member.
    let size: extern "system" fn(u64, u64) -> u64 = unsafe { std::mem::transmute((base + 0x5092470) as usize) };
    Some(size(allocator, at))
}

pub fn export(name: &str, original: usize) -> usize {
    let (cell, hook) = match name.split('@').next().unwrap_or(name) {
        "?GetRaytracingAccelerationStructurePrebuildInfo" => {
            PREBUILD.get_or_init(|| original);
            return original;
        }
        "?BuildRaytracingAccelerationStructureBatched2X" => (&BUILD, build as *const () as usize),
        "?RehydrateRaytracingAccelerationStructureBatched2X" => {
            (&REHYDRATE_FN, rehydrate as *const () as usize)
        }
        "?CopyRaytracingAccelerationStructure" => (&COPY, copy as *const () as usize),
        _ => return original,
    };
    if *cell.get_or_init(|| original) == original {
        hook
    } else {
        original
    }
}

pub(crate) fn read<const N: usize>(at: u64) -> Option<[u8; N]> {
    let mut bytes = [0; N];
    at.checked_add(N as u64)?;
    (at != 0 && crate::host::read_into(at as *const u8, &mut bytes)).then_some(bytes)
}

fn word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}
fn addr(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

type Ranges = Vec<[u64; 3]>;
pub(crate) const INPUT: u64 = 0;
pub(crate) const SCRATCH: u64 = 1;
pub(crate) const INSTANCES: u64 = 2;
pub(crate) const INSTANCE_POINTERS: u64 = 3;
pub(crate) const OUTPUT: u64 = 4;
pub(crate) const PACKED_INPUT: u64 = 5;
pub(crate) const COMPACT_COPY: u64 = 6;
pub(crate) const REHYDRATE: u64 = 7;
pub(crate) const COPY_EXTENT: u64 = 8;

fn add(ranges: &mut Ranges, start: u64, size: u64, kind: u64) -> Option<()> {
    if size != 0 {
        if start == 0 || start.checked_add(size)? > (1 << 40) {
            return None;
        }
        ranges.push([start, size, kind]);
    }
    Some(())
}

fn extent(count: u64, stride: u64, width: u64) -> Option<u64> {
    if count == 0 {
        Some(0)
    } else {
        (count - 1).checked_mul(stride)?.checked_add(width)
    }
}

fn geometry(ranges: &mut Ranges, desc: &[u8]) -> Option<()> {
    match word(desc, 0) {
        0 => {
            if addr(desc, 8) != 0 {
                add(ranges, addr(desc, 8), 48, INPUT)?;
            }
            let indices = u64::from(word(desc, 24));
            let index_bytes = match word(desc, 16) {
                42 => 4,
                57 => 2,
                0 if indices == 0 => 0,
                _ => return None,
            };
            add(ranges, addr(desc, 32), indices * index_bytes, INPUT)?;
            let width = match word(desc, 20) {
                6 => 12,
                10 | 11 | 13 | 16 => 8,
                24 | 28 | 31 | 34 | 35 | 37 => 4,
                49 | 51 => 2,
                _ => return None,
            };
            add(
                ranges,
                addr(desc, 40),
                extent(u64::from(word(desc, 28)), addr(desc, 48), width)?,
                INPUT,
            )
        }
        1 => add(
            ranges,
            addr(desc, 16),
            extent(addr(desc, 8), addr(desc, 24), 24)?,
            INPUT,
        ),
        _ => None,
    }
}

fn build_ranges(this: usize, count: u32, desc: u64) -> Option<Ranges> {
    type Query = extern "system" fn(usize, *const u8, *const u8, *mut u64);
    let query: Query = unsafe { std::mem::transmute(*PREBUILD.get()?) };
    let mut ranges = Vec::new();
    for i in 0..count {
        let at = desc.checked_add(u64::from(i) * 80)?;
        let bytes = read::<80>(at)?;
        let mut sizes = [0u64; 3];
        let stream = if bytes[64..].iter().any(|&b| b != 0) {
            (at + 64) as *const u8
        } else {
            std::ptr::null()
        };
        query(this, (at + 8) as *const u8, stream, sizes.as_mut_ptr());
        let update = word(&bytes, 12) & 0x20 != 0;
        add(&mut ranges, addr(&bytes, 0), sizes[0], OUTPUT)?;
        add(
            &mut ranges,
            addr(&bytes, 40),
            sizes[if update { 2 } else { 1 }],
            SCRATCH,
        )?;
        if update {
            add(&mut ranges, addr(&bytes, 32), sizes[0], INPUT)?;
        }
        let n = word(&bytes, 16);
        let layout = word(&bytes, 20);
        let start = addr(&bytes, 24);
        match word(&bytes, 8) {
            0 if layout <= 1 => add(
                &mut ranges,
                start,
                u64::from(n) * if layout == 0 { 64 } else { 8 },
                INSTANCES + u64::from(layout),
            )?,
            1 if layout <= 1 => {
                for j in 0..n {
                    let at = if layout == 0 {
                        start.checked_add(u64::from(j) * 56)?
                    } else {
                        u64::from_le_bytes(read(start.checked_add(u64::from(j) * 8)?)?)
                    };
                    geometry(&mut ranges, &read::<56>(at)?)?;
                }
            }
            _ => return None,
        }
    }
    Some(ranges)
}

fn start(list: This, mut ranges: Option<Ranges>) {
    static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some(ranges) = &mut ranges {
        for entry in ranges {
            entry[2] |= generation << 8;
        }
    }
    // Invalid input refuses this recording at its first dependent dispatch.
    crate::list::thin::builder_ranges(list, ranges.as_deref().unwrap_or(&[[0, 1, INPUT]]));
}

extern "system" fn build(this: usize, list: This, n: u32, desc: u64) {
    let original: extern "system" fn(usize, This, u32, u64) =
        unsafe { std::mem::transmute(*BUILD.get().unwrap()) };
    if !crate::pipeline::builder_enabled() {
        original(this, list, n, desc);
        return;
    }
    start(list, build_ranges(this, n, desc));
    original(this, list, n, desc);
    crate::list::thin::builder_ranges(list, &[]);
}

pub(crate) fn rehydration_extents(h: &[u8; 64], in_place: bool) -> Option<(u64, u64)> {
    let flags = word(h, 0);
    if flags as u16 != 6 || flags & (1 << 24) == 0 {
        return None;
    }
    let nodes = (u64::from(word(h, 4)) + 1) * 64;
    let radix = u64::from(flags >> 20 & 15);
    let prefix = if word(h, 44) == 0 {
        radix * radix * 0x90 + 64
    } else {
        u64::from(word(h, 56))
    };
    let input = if in_place { nodes } else { prefix };
    (input >= 64).then_some((input, nodes))
}

fn rehydrated(n: u32, desc: u64, headers: u64) -> Option<Ranges> {
    let mut ranges = Vec::new();
    for i in 0..u64::from(n) {
        let d = read::<48>(desc.checked_add(i * 48)?)?;
        let (dest, source) = (addr(&d, 0), addr(&d, 32));
        if headers == 0 {
            // Streamed bytes are consumed after preceding queue work.
            ranges.push([dest, source, REHYDRATE]);
        } else {
            // The caller's alternate CPU header only lives through this call.
            let header = u64::from_le_bytes(read(headers.checked_add(i * 8)?)?);
            let (input, output) = rehydration_extents(&read(header)?, source == dest)?;
            add(&mut ranges, source, input, PACKED_INPUT)?;
            add(&mut ranges, dest, output, OUTPUT)?;
        }
    }
    Some(ranges)
}

extern "system" fn rehydrate(this: usize, list: This, n: u32, desc: u64, headers: u64) -> i32 {
    let original: extern "system" fn(usize, This, u32, u64, u64) -> i32 =
        unsafe { std::mem::transmute(*REHYDRATE_FN.get().unwrap()) };
    if !crate::pipeline::builder_enabled() {
        return original(this, list, n, desc, headers);
    }
    start(list, rehydrated(n, desc, headers));
    let hr = original(this, list, n, desc, headers);
    crate::list::thin::builder_ranges(list, &[]);
    hr
}

extern "system" fn copy(this: usize, list: This, dest: u64, source: u64, mode: u32) {
    let original: extern "system" fn(usize, This, u64, u64, u32) =
        unsafe { std::mem::transmute(*COPY.get().unwrap()) };
    if mode != 1 || !crate::pipeline::builder_enabled() {
        original(this, list, dest, source, mode);
        return;
    }
    // The query can still be in flight when this command is recorded.
    // Its source generation and exact size are validated at submission.
    let mut ranges = vec![[dest, source, COMPACT_COPY]];
    if let Some(len) = compact_allocation(dest) {
        ranges.push([dest, len, COPY_EXTENT]);
    }
    start(list, Some(ranges));
    original(this, list, dest, source, mode);
    crate::list::thin::builder_ranges(list, &[]);
}
