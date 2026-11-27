//! Ranges declared at the Scarlett builder ABI, before translation into dispatches.
use crate::object::This;
use std::sync::OnceLock;
mod packed_indices;
pub(crate) use packed_indices::packed_index_extent;

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
pub(crate) const PACKED_GEOMETRY: u64 = 9;

fn add(ranges: &mut Ranges, start: u64, size: u64, kind: u64) -> Option<()> {
    if size != 0 {
        if start == 0 || start.checked_add(size)? > (1 << 40) {
            tracing::error!(address=format_args!("{start:#x}"),size,kind,"invalid GPU buffer extent in BVH declaration");
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
            let index_at = addr(desc, 32);
            match word(desc, 16) {
                42 => add(ranges, index_at, indices * 4, INPUT)?,
                57 if indices != 0 => {
                    // The shader aligns the base and fetches whole dwords.
                    let base = index_at & !3;
                    let bytes = (index_at - base + indices * 2).next_multiple_of(4);
                    add(ranges, base, bytes, INPUT)?;
                }
                53 if indices != 0 && index_at != 0 => {
                    // XboxBvhBuilder encodes this format as mode 4. Its meshlet,
                    // triangle and vertex-remap tables are sized by GPU data.
                    ranges.push([index_at & !3, indices, PACKED_GEOMETRY]);
                }
                0 if index_at == 0 => {}
                53 | 57 if indices == 0 => {}
                index_format => { tracing::error!(index_format,indices,"unsupported BVH index stream"); return None; }
            }
            let width = match word(desc, 20) {
                6 => 12,
                10 | 11 | 13 | 16 => 8,
                24 | 28 | 31 | 34 | 35 | 37 => 4,
                49 | 51 => 2,
                vertex_format => { tracing::error!(vertex_format,vertices=word(desc,28),"unsupported BVH vertex stream"); return None; }
            };
            // XboxBvhBuilder's triangle kernels use the low 15 bits for
            // vertex addressing. Bit 15 reverses the triangle winding.
            let vertex_stride = addr(desc, 48) & 0x7fff;
            add(
                ranges,
                addr(desc, 40),
                extent(u64::from(word(desc, 28)), vertex_stride, width)?,
                INPUT,
            )
        }
        1 => add(
            ranges,
            addr(desc, 16),
            extent(addr(desc, 8), addr(desc, 24), 24)?,
            INPUT,
        ),
        kind => { tracing::error!(kind,"unsupported BVH geometry type"); None }
    }
}

#[cfg(test)]
mod geometry_tests {
    use super::*;

    #[test]
    fn rehydration_reads_leaf_columns_and_the_full_internal_node_group_table() {
        let mut h = [0u8; 64];
        for (offset, value) in [(0, 0x01000006u32), (4, 4095), (12, 2049), (52, 512)] {
            h[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        assert_eq!(rehydration_extents(&h, false), Some((131596, 262144)));
        h[48] = 4;
        assert_eq!(rehydration_extents(&h, false), Some((262144, 262144)));
    }

    #[test]
    fn rehydration_declares_referenced_geometry_with_an_alternate_cpu_header() {
        let geo = [0u32, 1, 0, 0, 53, 16, 6, 4, 0x1000, 6, 0x2000, 6, 8, 0];
        let header = [0x0100_0006u32, 3, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 64, 0, 0];
        let alternate = [header.as_ptr() as u64];
        let desc = [0x5_0001_0000u64, 1, 1, geo.as_ptr() as u64, 0x5_0002_0000, 0];
        let ranges = rehydrated(1, desc.as_ptr() as u64, alternate.as_ptr() as u64).unwrap();
        assert!(ranges.contains(&[0x6_0000_1000, 6, PACKED_GEOMETRY]));
        assert!(ranges.contains(&[0x6_0000_2000, 32, INPUT]));
    }

    #[test]
    fn title_quad_defers_packed_indices_and_declares_its_four_vertices() {
        let words = [0u32, 1, 0, 0, 53, 16, 6, 4, 420996848, 7, 420996736, 7, 8, 0];
        let desc: Vec<_> = words.into_iter().flat_map(u32::to_le_bytes).collect();
        let mut ranges = Vec::new();
        assert_eq!(geometry(&mut ranges, &desc), Some(()));
        assert_eq!(ranges, [
            [(7 << 32) + 420996848, 6, PACKED_GEOMETRY],
            [(7 << 32) + 420996736, 32, INPUT],
        ]);
    }

    #[test]
    fn reversed_triangles_share_the_same_vertex_bytes() {
        let mut words = [0u32, 1, 0, 0, 53, 16, 6354, 1660, 0x7a99ad40, 9, 0x7a9638d0, 9, 8, 0];
        for stride in [8, 0x8008] {
            words[12] = stride;
            let desc: Vec<_> = words.into_iter().flat_map(u32::to_le_bytes).collect();
            let mut ranges = Vec::new();
            assert_eq!(geometry(&mut ranges, &desc), Some(()));
            assert_eq!(ranges[1], [0x97a9638d0, 13280, INPUT]);
        }
    }
}

fn build_ranges(this: usize, count: u32, desc: u64) -> Option<Ranges> {
    type Query = extern "system" fn(usize, *const u8, *const u8, *mut u64);
    let query: Query = unsafe { std::mem::transmute(*PREBUILD.get()?) };
    let mut ranges = Vec::new();
    for i in 0..count {
        let at = desc.checked_add(u64::from(i) * 80)?;
        let bytes = read::<80>(at).or_else(|| { tracing::error!(i,count,at,"unreadable BVH build description"); None })?;
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
                        u64::from_le_bytes(read(start.checked_add(u64::from(j) * 8)?).or_else(|| {
                            tracing::error!(i,j,n,start,"unreadable BVH geometry pointer array"); None
                        })?)
                    };
                    let geometry_desc = read::<56>(at).or_else(|| {
                        tracing::error!(i,j,n,at,layout,start,"unreadable BVH geometry"); None
                    })?;
                    geometry(&mut ranges, &geometry_desc).or_else(|| {
                        tracing::error!(i,j,n,at,words=?geometry_desc.chunks_exact(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).collect::<Vec<_>>(),
                            "could not decode original BVH geometry"); None
                    })?;
                }
            }
            kind => { tracing::error!(i,kind,layout,n,start,"unsupported BVH build input layout"); return None; }
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
    let ranges = build_ranges(this, n, desc);
    if ranges.is_none() { tracing::error!(n,desc,query=PREBUILD.get().is_some(),"could not capture original BVH build declarations"); }
    start(list, ranges);
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
    // The prefix copy dispatch uses ceil(prefix / 16) lanes, each of which
    // loads and stores a complete uint4 (d9caa05f8683e84b).
    let mut input = if in_place { nodes } else { prefix.next_multiple_of(16) };
    if in_place || matches!(word(h, 48) & 7, 4 | 5) {
        // Compressed internal-node groups are indexed by the trailer at the
        // end of this allocation (XboxBvhBuilder RVA 0x14700).
        input = input.max(nodes);
    } else {
        let leaves = u64::from(word(h, 12));
        if leaves != 0 {
            // a217754f5a99d197 reads packed leaf columns from the source AS,
            // even after a separate destination received the prefix. In the
            // last block it makes three aligned 8-byte reads for 24-bit
            // fields and two single-byte reads per leaf.
            let block = (leaves - 1) / 2048;
            let count = (leaves - 1) % 2048 + 1;
            let start = u64::from(word(h, 52)) + block * 131072;
            let last_pair = (start + 10 * count - 3) & !3;
            input = input.max(start + 11 * count).max(last_pair + 8);
        }
    }
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
            let h = read(header)?;
            let (input, output) = rehydration_extents(&h, source == dest).or_else(|| {
                tracing::error!(i,header,source,dest,words=?h.chunks_exact(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).collect::<Vec<_>>(),
                    "unsupported original BVH rehydration header"); None
            })?;
            add(&mut ranges, source, input, PACKED_INPUT)?;
            add(&mut ranges, dest, output, OUTPUT)?;
            if word(&h, 12) == 0 {
                continue;
            }
        }
        // Rehydrate's second kernel (a217754f5a99d197) follows the index
        // and vertex addresses copied from these triangle records. The DLL
        // copies each 48-byte payload from a contiguous 56-byte record,
        // starting at byte 8. The optional CPU header is a separate array.
        let geometries = addr(&d, 24);
        for j in 0..u64::from(word(&d, 16)) {
            let at = geometries.checked_add(j * 56)?;
            let record = read::<56>(at)?;
            if word(&d, 8) != 1 || word(&record, 0) != 0 {
                tracing::error!(i,j,at,"unsupported original BVH rehydration geometry");
                return None;
            }
            geometry(&mut ranges, &record)?;
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
    let ranges = rehydrated(n, desc, headers);
    if ranges.is_none() { tracing::error!(n,desc,headers,"could not capture original BVH rehydration declarations"); }
    start(list, ranges);
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
