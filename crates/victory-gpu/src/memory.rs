//! Title memory by address, for translated shaders that read and write
//! through raw 40-bit addresses (`victory_shader::emit::SLOT_MEMORY`: the
//! console's ray-tracing structure builder and the programs that walk its
//! structures; the structures themselves lie in plain title memory, in no
//! buffer object).
//!
//! A 64 KiB page in a title buffer that holds all of it is in that buffer's
//! copy ([`crate::shadow`]), which every other access uses. A page outside
//! buffers has a page of its own in an **arena**: memory of ours with a copy
//! on the card ([`crate::shadow::by_address`]). The streamed structures lie
//! anywhere between other data (6.6 GB of title memory in 4 MiB windows, run
//! light-1; 8 GB in windows of 256 KiB, light-2), so a page is what is kept.
//!
//! This module keeps what a shader needs to find the pages:
//!
//! - the **directory**: per 64 KiB page of title address space the number
//!   of the copy that holds the page (0: none) and the page's byte offset
//!   in it; one for all lists, entries only ever go from empty to filled.
//!   The CPU writes it in memory of its own and shaders read a copy of that
//!   on the card, brought up to date as title memory is (over the bus, every
//!   step of every ray waited for it);
//! - per view heap a run of slots with a raw unordered access view of each
//!   numbered copy (slot 0 of the run: a null view, for pages without a
//!   copy);
//! - the **faults**: the page numbers shaders found empty, read back here
//!   once a frame, so the pages are there from the next frame on. A pointer a
//!   shader takes out of memory cannot be known before it runs.
//!
//! **A page that is not there when a shader touches it is a lost access**:
//! the read gives 0, the store is dropped, and the fault note only helps
//! the next frame. For the structure builder that is the same as a dropped
//! stage, so before one of its dispatches the host makes ready what it can
//! see the dispatch name:
//!
//! - 256 KiB from each root view outside buffers, and where title memory has a
//!   packed ("dehydrated") structure there, all of it: the title ships its
//!   structures packed, and the builder's rehydration stages unpack each in
//!   place once it is streamed in;
//! - 4 MiB from each unordered access view outside buffers (structures built);
//! - the page of each pair of root constants that is a title address and of
//!   each root constant that is one in 256-byte units;
//! - the first page of every structure a list's upload memory names
//!   (a top-level build's instances).
//!
//! **What comes back to title memory**: what the builder stores in title
//! buffers of at most [`BACK`] bytes it names (the sizes the title reads to
//! make a structure smaller). Structures and scratch memory stay on the card
//! (the first whole run of the builder wrote garbage over the title's
//! objects). So an arena's page is the only place a structure is whole, and
//! title memory must not go over it: of a page the title wrote only the
//! dwords that changed since they were last taken go up. A packed structure
//! goes up whole when the builder comes to it, whatever is known of it: the
//! same bytes streamed in again do not differ from those last taken, and the
//! card holds them unpacked (run light-3: a quarter of the structures
//! unpacked a second time, into garbage all over their neighbours).
//!
//! **Stores are bounded**: a structure the builder misreads makes it store
//! anywhere. Pages of title buffers take its stores only once one of its
//! dispatches was given that buffer as an unordered access view (its scratch
//! memory, its results); what else a store reaches is an arena's.
//!
//! A packed structure is gone when title memory has no header of it any more
//! (looked at over [`SWEEP`] frames); the pages it alone has are given back.
//!
//! shortcut: a page held for anything else is kept for the rest of the run.
//! shortcut: pages a list does not name are looked at again in turn, over
//! [`SWEEP`] frames.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

use victory_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES, MEMORY_READ_ONLY};
use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::host::HostHeap;
use crate::host::{HostList, host};
use crate::shadow::Shadow;

/// Copies that can be numbered (the run of heap slots is one longer).
const COPIES: u32 = (1 << 12) - 1;
/// A directory page.
const PAGE: u64 = 1 << 16;
/// An arena's bytes.
const ARENA: u64 = 64 << 20;
/// What is compared at a time.
const PIECE: usize = 4096;
/// Pages held, at most (3 GiB).
const MOST: usize = 3 << 14;
/// Pages of title memory with small title buffers in them, at most.
const WINDOWS: usize = 512;
/// The biggest packed structure taken for one.
const PACKED: u64 = 64 << 20;
/// For so many frames what a page holds may still be on its way.
const YOUNG: u64 = 4;
/// Free pages there are, at least, when a list is recorded.
const SPARE: usize = 1024;
/// From a root view; from one that is written (a structure being built).
const VIEW: u64 = 256 << 10;
const BUILT: u64 = 4 << 20;
/// The biggest title buffer whose builder results come back.
const BACK: u64 = 256 << 10;
/// Pages taken from the faults, of one frame and in all, at most: a ray in a
/// structure that is not right goes anywhere, and 2,048 a frame took all the
/// pages there are in the first seconds (run light-4).
const FAULTS: usize = 128;
const FAULTS_ALL: usize = 2048;
/// Frames over which every page is looked at once.
const SWEEP: usize = 128;

/// What a draw or dispatch does with title memory by address.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Use {
    Reads,
    SizeQuery { source: u64, destination: u64 },
    /// Stores by address, as the structure builder; `back`: its results in
    /// small title buffers come back.
    Builds { back: bool },
}

pub(crate) type BuilderInputs = Arc<Mutex<std::collections::BTreeSet<u64>>>;

/// A page of title memory in an arena.
struct Held {
    /// Where in the arena's memory, and the arena's place in `copies`.
    at: usize,
    copy: usize,
    /// The write-watch asking after which it was last taken (0: never).
    taken: u64,
    /// The frame it was last looked at in, and the one it came in.
    looked: u64,
    born: u64,
    /// Packed structures that have bytes in it, and whether it is held for
    /// them alone.
    owners: u32,
    theirs: bool,
}

/// A packed structure the builder came to.
struct Packed {
    backing: Backing,
    content: blake3::Hash,
    /// The frame it last did.
    frame: u64,
    len: u64,
    /// Its first two dwords in title memory.
    header: u64,
}

type Backing = Vec<(usize, usize, usize, u64, u64)>;

// Section views have their own mapping identity. Private VA reuse has no
// section handle, so retain a lifetime stamp for pages observed by the BVH.
type PrivatePages = std::collections::BTreeMap<usize, u64>;
static PRIVATE_PAGES: std::sync::OnceLock<Mutex<PrivatePages>> = std::sync::OnceLock::new();

fn mapping_identity(at: usize, len: usize) -> Backing {
    let mut backing = crate::inplace::backing(at, len);
    if len == 0 || backing.iter().map(|&(a, b, ..)| b - a).sum::<usize>() == len {
        return backing;
    }
    let mut pages = PRIVATE_PAGES.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    let range = at / PAGE as usize..at.saturating_add(len).div_ceil(PAGE as usize);
    let generation = range.map(|p| *pages.entry(p).or_insert(0)).max().unwrap_or(0);
    backing.push((at, at.saturating_add(len), 0, 0, generation));
    backing
}

pub(crate) fn mapping_changed(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let Some(pages) = PRIVATE_PAGES.get() else { return };
    let mut pages = pages.lock().unwrap_or_else(|e| e.into_inner());
    let range = at / PAGE as usize..at.saturating_add(len).div_ceil(PAGE as usize);
    let generation = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    for (_, value) in pages.range_mut(range) {
        *value = generation;
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ViewSpan {
    pub(crate) at: u64,
    /// No byte count in a native acceleration-structure descriptor.
    pub(crate) bytes: Option<u64>,
}

#[derive(Clone)]
struct Structure {
    size: u64,
    generation: u64,
    backing: Backing,
    compacted: Option<u64>,
    published: bool,
    // Instance VA, canonical producer VA and generation (zero for an
    // externally supplied BLAS whose header is re-read before traversal).
    dependencies: Vec<(u64, u64, u64)>,
}

struct PriorOutput {
    at: u64,
    previous: Option<Structure>,
    backing: Backing,
    bytes: u64,
    // Owner buffers and offsets of the previous published GPU bytes.
    pieces: Vec<(ID3D12Resource, u64, u64)>,
    snapshot: Option<ID3D12Resource>,
}

type InstanceGraph = HashMap<u64, Vec<(u64, u64, u64)>>;

struct BuildVersion {
    ranges: Arc<[[u64; 3]]>,
    instances: Arc<InstanceGraph>,
    generation: u64,
    outputs: Vec<PriorOutput>,
}

struct DirectoryAlias {
    owner: u64,
    previous: [u32; 2],
    backing: Backing,
    owner_backing: Backing,
}

struct Memory {
    directory_aliases: HashMap<u64, DirectoryAlias>,
    declared_backing: HashMap<u64, Backing>,
    gpu_pages: std::collections::HashSet<u64>,
    versions: HashMap<u64, BuildVersion>,
    structures: HashMap<u64, Structure>,
    /// The CPU's entries (number, offset), and the copy shaders read.
    directory: (*mut u32, Arc<Shadow>),
    faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`.
    copies: Vec<Arc<Shadow>>,
    /// Places of title buffers' copies among `copies`.
    numbers: HashMap<usize, usize>,
    /// Arenas: place in `copies`, address.
    arenas: Vec<(usize, usize)>,
    /// By title page.
    pages: HashMap<u64, Held>,
    /// Their numbers in the order they came, and where the sweep is.
    order: Vec<u64>,
    sweep: usize,
    /// Windows of title memory itself ([`Memory::map`]).
    windows: usize,
    page_scratch: Vec<u8>,
    /// Packed structures the builder came to: the frame it last did.
    packed: HashMap<u64, Packed>,
    /// Their addresses in the order they came, and where the sweep is.
    packed_order: Vec<u64>,
    packed_sweep: usize,
    /// Places of pages that are free: the copy, the address in the arena.
    free: Vec<(usize, usize)>,
    /// Copies of title buffers the builder may store in.
    open: std::collections::HashSet<usize>,
    /// Pages taken from faults.
    faulted: usize,
    /// Per host view heap (by address): the first slot of its run, and how
    /// many copies have their view there.
    heaps: HashMap<usize, (u32, u32)>,
    /// The frame the faults were last read in.
    frame: u64,
    /// Pages asked for this frame that are not the title's memory.
    refused: std::collections::HashSet<u64>,
    stats: Stats,
    #[cfg(feature = "diagnostics")]
    dump: dump::Dump,
    #[cfg(feature = "diagnostics")]
    trace: trace::Trace,
}

/// Since the last report.
#[derive(Default)]
struct Stats {
    from: Option<(std::time::Instant, u64)>,
    uses: [u64; 2],
    faults: u64,
    unknown: u64,
    back: u64,
    /// Pages looked at, bytes that changed, packed structures sent.
    looked: u64,
    changed: u64,
    packed: u64,
    packed_bytes: u64,
    gone: u64,
    /// Pages that came for an address in constants, a written view, another
    /// view, a structure named in upload memory.
    came: [u64; 4],
    micros: u64,
}

// SAFETY: the mappings are of memory the struct owns; all access is under
// the mutex.
unsafe impl Send for Memory {}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// What a draw binds.
pub(crate) struct Ready {
    pub(crate) directory: u64,
    pub(crate) faults: u64,
    /// The view heap slot of the null view; copy `n`'s view is `n` further.
    pub(crate) base: u32,
}

impl Memory {
    fn new() -> Option<Memory> {
        let host = host()?;
        let bytes = u64::from(MEMORY_PAGES) * 8;
        // Zeroed, and watched for writes as the title's memory is.
        let entries = crate::upload::reserve(bytes as usize)?;
        let directory = crate::shadow::by_address(entries as u64, bytes)?;
        crate::host::name(&directory.buffer, || "title memory by address: directory".into());
        let (faults, data) = host.shared_buffer(u64::from(MEMORY_FAULTS) * 4)?;
        crate::host::name(&faults, || "title memory by address: faults".into());
        // SAFETY: a fresh mapping of at least that many bytes.
        unsafe { std::ptr::write_bytes(data, 0, MEMORY_FAULTS as usize * 4) };
        // SAFETY: a live buffer.
        let gpu = unsafe { faults.GetGPUVirtualAddress() };
        tracing::info!("title memory by address: a shader reads or writes through raw addresses");
        Some(Memory {
            directory: (entries as *mut u32, directory),
            faults: (faults, data.cast::<u32>(), gpu),
            copies: Vec::new(),
            numbers: HashMap::new(),
            arenas: Vec::new(),
            pages: HashMap::new(),
            order: Vec::new(),
            sweep: 0,
            windows: 0,
            page_scratch: vec![0; PAGE as usize],
            directory_aliases: Default::default(),
            declared_backing: Default::default(),
            gpu_pages: Default::default(),
            versions: HashMap::new(),
            structures: HashMap::new(),
            packed: HashMap::new(),
            packed_order: Vec::new(),
            packed_sweep: 0,
            free: Vec::new(),
            open: Default::default(),
            faulted: 0,
            heaps: HashMap::new(),
            frame: u64::MAX,
            refused: Default::default(),
            stats: Stats::default(),
            #[cfg(feature = "diagnostics")]
            dump: Default::default(),
            #[cfg(feature = "diagnostics")]
            trace: Default::default(),
        })
    }

    /// The directory's entry of `page`.
    fn entry(&self, page: u64) -> *mut u32 {
        debug_assert!(page < u64::from(MEMORY_PAGES));
        // SAFETY: inside the directory.
        unsafe { self.directory.0.add(page as usize * 2) }
    }

    /// The place in `copies` of the copy that holds `page`, if one does.
    fn copy_of(&self, page: u64) -> Option<usize> {
        if page >= u64::from(MEMORY_PAGES) {
            return None;
        }
        // SAFETY: an entry of the directory.
        ((unsafe { self.entry(page).read() } & !MEMORY_READ_ONLY) as usize).checked_sub(1)
    }

    /// Whether `page` can be given a place: in the directory, not turned down
    /// this frame, and the title's GPU memory. (A shader's pointer, or a
    /// constant taken for an address, may be anything.)
    fn possible(&mut self, page: u64) -> bool {
        // Not the last entry: addresses past the directory read it, and it
        // stays empty.
        if page < 0x1_0000 || page >= u64::from(MEMORY_PAGES) - 1 || self.refused.contains(&page) {
            return false;
        }
        let at = (page * PAGE) as usize;
        let ours = crate::texture::committed(at as *const u8, PAGE as usize)
            && crate::watch::writes(at, at + PAGE as usize, true).is_some();
        if !ours {
            self.refused.insert(page);
            self.stats.unknown += 1;
        }
        ours
    }

    fn reserve_ranges(&mut self, ranges: &[[u64; 3]], instances: &InstanceGraph) -> bool {
        use crate::bvh::{INSTANCE_POINTERS, INSTANCES, OUTPUT, SCRATCH};
        let mut output = None;
        for &[at, len, stamp] in ranges {
            let kind = stamp & 255;
            #[track_caller]
            fn rejected(at: u64, len: u64, kind: u64) -> bool {
                tracing::error!(address = format_args!("{at:#x}"), len, kind, site = %std::panic::Location::caller(), "address range rejected during BVH build");
                false
            }
            if kind == crate::bvh::COPY_EXTENT {
                continue;
            }
            if kind == crate::bvh::COMPACT_COPY {
                let source = len;
                let Some(canonical) = self.structure_at(source) else {
                    return rejected(at, len, kind);
                };
                let structure = &self.structures[&canonical];
                let Some(size) = structure.compacted else {
                    return rejected(at, len, kind);
                };
                if !self.published_range(canonical) || size == 0 || size > structure.size {
                    return rejected(at, len, kind);
                }
                let src_len = structure.size;
                let deps = structure.dependencies.clone();
                let batch_extent = ranges.iter().find(|r| r[0] == at && r[2] & 255 == crate::bvh::COPY_EXTENT)
                    .map(|r| r[1]);
                let buffer_extent = crate::shadow::in_buffer(at, 1)
                    .and_then(|(base, len)| len.checked_sub(at - base));
                if batch_extent.into_iter().chain(buffer_extent).min().is_none_or(|len| size > len)
                    || std::iter::once(source as usize).chain(crate::inplace::aliases(source as usize, src_len as usize))
                        .any(|other| at.abs_diff(other as u64) < if at < other as u64 { size } else { src_len })
                {
                    return rejected(at, len, kind);
                }
                if self.structures.iter().any(|(&base, s)| {
                    base != at
                        && base != source
                        && base < at.saturating_add(size)
                        && at < base.saturating_add(s.size)
                        && s.published
                        && mapping_identity(base as usize, s.size as usize) == s.backing
                }) {
                    return rejected(at, len, kind);
                }
                if !self.reserve(source, src_len, false) || !self.reserve(at, size, true) {
                    return rejected(at, len, kind);
                }
                let generation = stamp >> 8;
                let target = self.structure_at(at).unwrap_or(at);
                if target != at && !crate::inplace::aliases(at as usize, size as usize).contains(&(target as usize)) {
                    return rejected(at, len, kind);
                }
                if self
                    .structures
                    .get(&target)
                    .is_none_or(|s| s.generation != generation)
                {
                    self.structures.insert(
                        target,
                        Structure {
                            size,
                            generation,
                            backing: mapping_identity(target as usize, size as usize),
                            compacted: None,
                            published: false,
                            dependencies: deps,
                        },
                    );
                }
                continue;
            }
            if kind == crate::bvh::INPUT
                && self
                    .structures
                    .get(&at)
                    .is_some_and(|s| !s.published && s.generation != stamp >> 8)
            {
                return rejected(at, len, kind);
            }
            if !self.reserve(at, len, matches!(kind, OUTPUT | SCRATCH)) {
                return rejected(at, len, kind);
            }
            if kind == crate::bvh::PACKED_INPUT {
                if !self.land(at, Some(len)) {
                    return rejected(at, len, kind);
                }
            } else if kind == OUTPUT {
                let target = self.structure_at(at).unwrap_or(at);
                if target != at && !crate::inplace::aliases(at as usize, len as usize).contains(&(target as usize)) {
                    return rejected(at, len, kind);
                }
                output = Some(target);
                let generation = stamp >> 8;
                if self
                    .structures
                    .get(&target)
                    .is_none_or(|s| s.generation != generation)
                {
                    let backing = mapping_identity(target as usize, len as usize);
                    self.structures.insert(
                        target,
                        Structure {
                            size: len,
                            backing,
                            generation,
                            published: false,
                            dependencies: Vec::new(),
                            compacted: None,
                        },
                    );
                }
            } else if matches!(kind, INSTANCES | INSTANCE_POINTERS) {
                let mut dependencies = Vec::new();
                let Some(records) = instances.get(&at) else {
                    return rejected(at, len, kind);
                };
                let mut visited = std::collections::HashSet::new();
                for &(instance, blas, h) in records {
                    if kind == INSTANCE_POINTERS && !self.reserve(instance, 64, false) {
                        return rejected(at, len, kind);
                    }
                    if !visited.insert(blas) {
                        continue;
                    }
                    if blas == 0 {
                        continue;
                    }
                    let canonical = self.structure_at(blas).unwrap_or(blas);
                    let size = if let Some(s) = self.structures.get(&canonical) {
                        if (!s.published && s.generation != stamp >> 8)
                            || mapping_identity(canonical as usize, s.size as usize)
                                != s.backing
                        {
                            return rejected(at, len, kind);
                        }
                        dependencies.push((blas, canonical, s.generation));
                        s.size
                    } else {
                        if h as u16 != 6 || h & 0x8100_0000 != 0x8000_0000 {
                            return rejected(at, len, kind);
                        }
                        dependencies.push((blas, blas, 0));
                        ((h >> 32) + 1) * 64
                    };
                    if !self.reserve(blas, size, false) {
                        return rejected(at, len, kind);
                    }
                }
                dependencies.sort_unstable();
                dependencies.dedup();
                if let Some(s) = output.and_then(|at| self.structures.get_mut(&at)) {
                    s.dependencies = dependencies;
                }
            }
        }
        true
    }

    fn invalidate_directory(&self, page: u64) {
        let off = (page * 8) as usize;
        crate::watch::wrote(self.entry(page) as usize, 8);
        self.directory.1.resend(off, off + 8);
    }

    fn reconcile_aliases(&mut self) {
        let stale: Vec<_> = self
            .directory_aliases
            .iter()
            .filter_map(|(&p, a)| {
                (mapping_identity((p * PAGE) as usize, PAGE as usize) != a.backing
                    || mapping_identity((a.owner * PAGE) as usize, PAGE as usize)
                        != a.owner_backing
                    || self.copy_of(a.owner).is_none())
                .then_some(p)
            })
            .collect();
        for page in stale {
            let alias = self.directory_aliases.remove(&page).unwrap();
            unsafe {
                self.entry(page).cast::<[u32; 2]>().write(alias.previous);
            }
            self.invalidate_directory(page);
        }
    }

    fn structure_at(&mut self, address: u64) -> Option<u64> {
        if address / PAGE >= u64::from(MEMORY_PAGES) - 1 {
            return None;
        }
        if let Some(s) = self.structures.get(&address) {
            if mapping_identity(address as usize, s.size as usize) == s.backing {
                return Some(address);
            }
            self.structures.remove(&address);
        }
        for other in crate::inplace::aliases(address as usize, 1) {
            let Some(s) = self.structures.get(&(other as u64)) else {
                continue;
            };
            if other % PAGE as usize != address as usize % PAGE as usize
                || !crate::inplace::aliases(address as usize, s.size as usize).contains(&other)
            {
                continue;
            }
            let end = address.checked_add(s.size)?.div_ceil(PAGE);
            if end >= u64::from(MEMORY_PAGES) {
                return None;
            }
            let pages = address / PAGE..end;
            if !pages
                .clone()
                .enumerate()
                .all(|(i, _)| self.copy_of(other as u64 / PAGE + i as u64).is_some())
            {
                continue;
            }
            for (i, page) in pages.enumerate() {
                let other_page = other as u64 / PAGE + i as u64;
                let owner = self.directory_aliases.get(&other_page).map_or(other_page, |a| a.owner);
                if owner == page {
                    continue;
                }
                let value = unsafe { self.entry(owner).cast::<[u32; 2]>().read() };
                let ptr = self.entry(page).cast::<[u32; 2]>();
                self.directory_aliases
                    .entry(page)
                    .or_insert_with(|| DirectoryAlias {
                        owner,
                        previous: unsafe { ptr.read() },
                        backing: mapping_identity((page * PAGE) as usize, PAGE as usize),
                        owner_backing: mapping_identity(
                            (owner * PAGE) as usize,
                            PAGE as usize,
                        ),
                    });
                unsafe {
                    ptr.write(value);
                }
                self.invalidate_directory(page);
            }
            return Some(other as u64);
        }
        None
    }

    fn published_range(&self, at: u64) -> bool {
        let Some(s) = self.structures.get(&at) else {
            return true;
        };
        s.published
            && mapping_identity(at as usize, s.size as usize) == s.backing
            && s.dependencies.iter().all(|&(va, owner, generation)| {
                generation == 0 || self.structures.get(&owner).is_some_and(|child| {
                    child.published && child.generation == generation
                        && child.backing == mapping_identity(owner as usize, child.size as usize)
                        && (va == owner || crate::inplace::aliases(va as usize, child.size as usize).contains(&(owner as usize)))
                })
            })
    }

    fn reserve(&mut self, at: u64, len: u64, writable: bool) -> bool {
        let Some(end) = at
            .checked_add(len)
            .filter(|&end| end <= (u64::from(MEMORY_PAGES) - 1) * PAGE)
        else {
            return false;
        };
        if at < PAGE || len == 0 {
            return false;
        }
        // The original builder can name low-address geometry in ordinary
        // title buffers. The 4 GiB heuristic for speculative pointer scans
        // must not discard its explicit, mapped input declarations.
        if at < 1 << 32 && crate::shadow::in_buffer(at, len).is_none()
            && crate::watch::writes(at as usize, end as usize, true).is_none() {
            tracing::error!(address=format_args!("{at:#x}"), len, writable, "low BVH declaration lacks a guest buffer or tracked mapping");
            return false;
        }
        let pages = at / PAGE..end.div_ceil(PAGE);
        if !crate::texture::committed_declaration(at as usize, len as usize)
            || !pages.clone().all(|page| self.reserve_page(page))
        {
            return false;
        }
        // Declared inputs may be filled more than once in the same frame.
        for page in pages.clone() {
            let owner = self.directory_aliases.get(&page).map_or(page, |alias| alias.owner);
            if let Some(held) = self.pages.get_mut(&owner) {
                held.looked = u64::MAX;
            }
            if owner != page {
                self.bring(owner, owner + 1);
            }
        }
        self.bring(pages.start, pages.end);
        if writable {
            for page in pages {
                let owner = self.directory_aliases.get(&page).map_or(page, |a| a.owner);
                if !self.pages.contains_key(&owner) {
                    let shadow = &self.copies[self.copy_of(owner).expect("reserved page")];
                    if !shadow.preserve_raw_page((owner * PAGE) as usize, !self.gpu_pages.contains(&owner)) {
                        return false;
                    }
                }
                self.opens(page * PAGE);
            }
        }
        true
    }

    /// Reuse an arena slot only for the bytes its current guest mapping
    /// names. Equal CPU dwords in a replacement mapping must replace any
    /// GPU-only stores in the previous page too.
    fn reserve_page(&mut self, page: u64) -> bool {
        let va = (page * PAGE) as usize;
        let backing = mapping_identity(va, PAGE as usize);
        if self.declared_backing.get(&page).is_some_and(|old| old != &backing) {
            self.gpu_pages.remove(&page);
            if let Some(held) = self.pages.get_mut(&page) {
                held.taken = 0;
                held.looked = u64::MAX;
            } else if !self.directory_aliases.contains_key(&page) {
                unsafe { self.entry(page).write(0) };
                self.invalidate_directory(page);
            }
        }
        if self.declared_backing.get(&page) != Some(&backing)
            && !self.directory_aliases.contains_key(&page) && !backing.is_empty() {
            for other in crate::inplace::aliases(va, PAGE as usize) {
                let owner = other as u64 / PAGE;
                if !other.is_multiple_of(PAGE as usize) || self.directory_aliases.contains_key(&owner) || self.copy_of(owner).is_none() {
                    continue;
                }
                let owner_backing = mapping_identity(other, PAGE as usize);
                if self.declared_backing.get(&owner) != Some(&owner_backing) {
                    continue;
                }
                let entry = self.entry(page).cast::<[u32; 2]>();
                self.directory_aliases.insert(page, DirectoryAlias {
                    owner, backing: backing.clone(), owner_backing, previous: unsafe { entry.read() },
                });
                unsafe { entry.write(self.entry(owner).cast::<[u32; 2]>().read()) };
                self.invalidate_directory(page);
                break;
            }
        }
        self.declared_backing.insert(page, backing);
        self.map_page(page, true)
    }

    fn descriptors(&mut self, heap: &HostHeap) -> Option<u32> {
        let key = std::ptr::from_ref(heap) as usize;
        let (base, made) = match self.heaps.get(&key) {
            Some(&run) => run,
            None => {
                // Held for the heap's life: not given back with a list's slots.
                let base = heap.dynamic(COPIES + 1)?;
                // Every slot of the run: a number shows no view of another kind.
                for n in 0..=COPIES {
                    heap.null_raw_uav_at(base + n);
                }
                (base, 0)
            }
        };
        for (n, shadow) in self.copies.iter().enumerate().skip(made as usize) {
            heap.raw_uav_at(base + 1 + n as u32, &shadow.buffer, 0, shadow.width());
        }
        self.heaps.insert(key, (base, self.copies.len() as u32));
        Some(base)
    }

    /// Gives a copy a number, if one is left.
    fn number(&mut self, shadow: Arc<Shadow>) -> Option<usize> {
        // Page directory clients use raw dword loads and atomics. A shadow
        // must preserve the low two bits of each guest address in its UAV.
        let (first, _) = shadow.span();
        if (shadow.offset(first) ^ first) & 3 != 0 {
            tracing::warn!(first, "unaligned shadow cannot back the raw GPU page directory");
            return None;
        }
        let key = Arc::as_ptr(&shadow) as usize;
        if let Some(&n) = self.numbers.get(&key) {
            return Some(n);
        }
        if self.copies.len() >= COPIES as usize {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(COPIES, "title memory by address: no number left for another copy; its pages read zeros"));
            return None;
        }
        // SAFETY: a live buffer.
        let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
        if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            return None;
        }
        self.copies.push(shadow);
        let n = self.copies.len() - 1;
        self.numbers.insert(key, n);
        Some(n)
    }

    /// Gives `page` a place, if it has none and the memory is the title's;
    /// whether it has one then.
    fn map(&mut self, page: u64) -> bool {
        self.map_page(page, false)
    }

    fn map_page(&mut self, page: u64, declared: bool) -> bool {
        if self.copy_of(page).is_some() {
            return true;
        }
        if !declared && !self.possible(page) {
            return false;
        }
        let start = page * PAGE;
        if crate::shadow::in_buffer(start, PAGE).is_some() {
            let Some(shadow) = crate::shadow::find(start, PAGE) else {
                return false;
            };
            let fresh = !self.numbers.contains_key(&(Arc::as_ptr(&shadow) as usize));
            let Some(n) = self.number(shadow.clone()) else {
                return false;
            };
            // Every page the copy holds whole, not only the one asked for.
            // shortcut: a page of a title buffer inside this one that has a
            // copy of its own is given this copy too.
            if fresh {
                let (first, len) = shadow.span();
                let mut at = first.next_multiple_of(PAGE);
                while at + PAGE <= first + len && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
                    let entry = self.entry(at / PAGE);
                    // SAFETY: an entry of the directory.
                    if unsafe { entry.read() } == 0 {
                        close(entry, n, shadow.offset(at));
                    }
                    at += PAGE;
                }
            }
            // SAFETY: as above.
            if unsafe { self.entry(page).read() } == 0 {
                close(self.entry(page), n, shadow.offset(start));
            }
            return true;
        }
        // Small title buffers in it: what the builder stores in them comes
        // back, so title memory itself.
        if self.windows < WINDOWS && crate::shadow::small_buffer_in(start, PAGE, BACK) {
            if let Some(shadow) = crate::shadow::by_address(start, PAGE)
                && let Some(n) = self.number(shadow)
            {
                self.windows += 1;
                fill(self.entry(page), n, 0);
                return true;
            }
            return false;
        }
        if self.pages.len() >= MOST {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!(MOST, "title memory by address: no more pages are taken; others read zeros"));
            return false;
        }
        if let Some((copy, at)) = self.free.pop() {
            self.hold(page, at, copy);
            return true;
        }
        if !self.another() {
            return false;
        }
        let Some((copy, at)) = self.free.pop() else {
            return false;
        };
        self.hold(page, at, copy);
        true
    }

    /// Another arena's pages.
    fn another(&mut self) -> bool {
        let Some(at) = crate::upload::reserve(ARENA as usize) else {
            return false;
        };
        let Some(n) = crate::shadow::by_address(at as u64, ARENA).and_then(|s| self.number(s)) else {
            return false;
        };
        self.arenas.push((n, at));
        // The lowest first.
        self.free.extend((0..(ARENA / PAGE) as usize).rev().map(|i| (n, at + i * PAGE as usize)));
        true
    }

    /// `page` is held at `at` in the arena that is `copy`.
    fn hold(&mut self, page: u64, at: usize, copy: usize) {
        self.pages.insert(page, Held { at, copy, taken: 0, looked: u64::MAX, born: self.frame, owners: 0, theirs: false });
        self.order.push(page);
        fill(self.entry(page), copy, at as u64 - self.copies[copy].span().0);
    }

    /// The packed structure at `address` is gone: the pages it alone had are
    /// given back.
    fn gone(&mut self, address: u64) {
        let Some(packed) = self.packed.remove(&address) else { return };
        for page in address / PAGE..=(address + packed.len - 1) / PAGE {
            let Some(held) = self.pages.get_mut(&page) else { continue };
            held.owners = held.owners.saturating_sub(1);
            if held.owners == 0 && held.theirs && !self.declared_backing.contains_key(&page)
                && !self.directory_aliases.values().any(|alias| alias.owner == page) {
                self.free.push((held.copy, held.at));
                self.pages.remove(&page);
                // SAFETY: an entry of the directory, under the mutex.
                unsafe { self.entry(page).write_volatile(0) };
                self.invalidate_directory(page);
            }
        }
        self.stats.gone += 1;
    }

    /// Whether the packed structure at `address` is still in title memory.
    fn there(&self, address: u64) -> bool {
        let mut header = [0u8; 8];
        crate::texture::committed(address as *const u8, 8) && {
            crate::host::copy_mapped(header.as_mut_ptr(), address as *const u8, 8, false);
            self.packed.get(&address).is_some_and(|p| p.header == u64::from_le_bytes(header))
        }
    }

    /// Looks at some of the pages and packed structures again.
    fn look_again(&mut self) {
        for _ in 0..self.order.len().div_ceil(SWEEP) {
            self.sweep = (self.sweep + 1) % self.order.len();
            let page = self.order[self.sweep];
            if self.pages.contains_key(&page) {
                self.bring(page, page + 1);
            } else {
                self.order.swap_remove(self.sweep);
                if self.order.is_empty() {
                    break;
                }
            }
        }
        for _ in 0..self.packed_order.len().div_ceil(SWEEP) {
            self.packed_sweep = (self.packed_sweep + 1) % self.packed_order.len();
            let address = self.packed_order[self.packed_sweep];
            if !self.packed.contains_key(&address) {
                self.packed_order.swap_remove(self.packed_sweep);
                if self.packed_order.is_empty() {
                    break;
                }
            } else if !self.there(address) {
                self.gone(address);
            }
        }
    }

    /// A dispatch of the builder was given `address` to write: if it is in a
    /// title buffer's copy, stores by address reach that from here on.
    fn opens(&mut self, address: u64) {
        let page = address / PAGE;
        if let Some(owner) = self.directory_aliases.get(&page).map(|a| a.owner) {
            self.opens(owner * PAGE);
            // The owner may just have been opened for its first write.
            unsafe { self.entry(page).cast::<[u32; 2]>().write(self.entry(owner).cast::<[u32; 2]>().read()) };
            self.invalidate_directory(page);
            return;
        }
        self.gpu_pages.insert(address/PAGE);
        let Some(n) = self.map(address / PAGE).then(|| self.copy_of(address / PAGE)).flatten() else {
            return;
        };
        if self.pages.contains_key(&(address / PAGE)) || !self.open.insert(n) {
            return;
        }
        // (Its pages hold what the CPU writes beside the builder's scratch.)
        self.copies[n].keep_stores();
        let (first, len) = self.copies[n].span();
        tracing::info!(
            start = format_args!("{first:#x}"),
            len,
            "title memory by address: the structure builder may store in a title buffer"
        );
        for page in first.div_ceil(PAGE)..(first + len) / PAGE {
            if self.copy_of(page) == Some(n) {
                fill(self.entry(page), n, self.copies[n].offset(page*PAGE));
            }
        }
    }

    /// [`Memory::map`] of the pages of `[address, address + len)`, up to the
    /// first that is not the title's, and what the title wrote there since
    /// they were taken goes into the arenas.
    fn ready(&mut self, address: u64, len: u64) {
        let first = address / PAGE;
        let mut end = first;
        while end * PAGE < address + len && self.map(end) {
            end += 1;
        }
        self.bring(first, end);
    }

    /// Takes the pages `[first, end)` that are in arenas from title memory
    /// where it wrote them.
    fn bring(&mut self, first: u64, end: u64) {
        // Once a frame: the builder names the same pages list after list.
        let frame = self.frame;
        if !(first..end).any(|page| self.pages.get(&page).is_some_and(|h| h.looked != frame)) {
            return;
        }
        let started = std::time::Instant::now();
        let answer = crate::watch::writes((first * PAGE) as usize, (end * PAGE) as usize, true);
        for page in first..end {
            let Some(held) = self.pages.get_mut(&page) else {
                continue;
            };
            if held.looked == frame {
                continue;
            }
            held.looked = frame;
            self.stats.looked += 1;
            let wrote = answer
                .as_ref()
                .map(|(_, w)| w.binary_search_by_key(&(page as usize), |w| w.0).map_or(0, |i| w[i].1));
            let now = answer.as_ref().map_or(1, |a| a.0);
            let stale = held.taken == 0 || wrote.is_none_or(|w| w > held.taken);
            if !stale {
                continue;
            }
            let never = std::mem::replace(&mut held.taken, now.max(1)) == 0;
            let shadow = &self.copies[held.copy];
            let base = held.at - shadow.span().0 as usize;
            if never && !crate::host::skip_memory_copy() {
                crate::host::copy_mapped(
                    held.at as *mut u8,
                    (page * PAGE) as *const u8,
                    PAGE as usize,
                    false,
                );
                self.stats.changed += PAGE;
                shadow.resend(base, base + PAGE as usize);
                continue;
            }
            let bulk = !crate::safecopy::on() && !crate::host::skip_memory_copy();
            if bulk {
                crate::host::copy_mapped(
                    self.page_scratch.as_mut_ptr(),
                    (page * PAGE) as *const u8,
                    PAGE as usize,
                    false,
                );
            }
            let mut local = [0u8; PIECE];
            for piece in (0..PAGE as usize).step_by(PIECE) {
                let from = (page * PAGE) as usize + piece;
                // (Pages that are not there read as zeros.)
                let title: &[u8; PIECE] = if bulk {
                    self.page_scratch[piece..piece + PIECE].try_into().unwrap()
                } else {
                    crate::host::copy_mapped(local.as_mut_ptr(), from as *const u8, PIECE, false);
                    &local
                };
                // SAFETY: a page of an arena of ours, under the mutex.
                let ours = unsafe { std::slice::from_raw_parts_mut((held.at + piece) as *mut u8, PIECE) };
                self.stats.changed += merge(title, ours, never) as u64 * 4;
            }
            if never {
                shadow.resend(base, base + PAGE as usize);
            }
        }
        self.stats.micros += started.elapsed().as_micros() as u64;
    }

    /// The bytes of a packed structure title memory has at `address` by now,
    /// and its first two dwords.
    fn packed_at(&self, address: u64) -> Option<(u64, u64)> {
        if address & 0xFF != 0 {
            return None;
        }
        let mut header = [0u8; 8];
        crate::host::copy_mapped(header.as_mut_ptr(), address as *const u8, 8, false);
        let header = u64::from_le_bytes(header);
        Some((packed_len(header)?, header))
    }

    /// At the submission of a list whose builder dispatch was given `address`
    /// outside buffers, in the order of its copies: what title memory has
    /// there now goes up. When the dispatch is recorded it may not be there
    /// yet: the title's queue waits for its streaming. (Nothing is asked of
    /// the write watch here: the submission's asking stands.)
    fn land(&mut self, address: u64, declared: Option<u64>) -> bool {
        let page = address / PAGE;
        if !crate::texture::committed((page * PAGE) as *const u8, PAGE as usize) {
            return false;
        }
        let packed = match declared {
            Some(len) => {
                crate::bvh::read::<8>(address).map(|bytes| (len, u64::from_le_bytes(bytes)))
            }
            None => self.packed_at(address),
        };
        let Some((len, header)) = packed else {
            if declared.is_some() {
                return false;
            }
            // Anything else: what came since its page was first taken.
            for page in page..=(address + VIEW - 1) / PAGE {
                if self.pages.get(&page).is_some_and(|h| h.born + YOUNG >= self.frame) {
                    self.take(page, 0, PAGE as usize, false);
                }
            }
            return true;
        };
        // Capture once. The hash, comparison and GPU upload all refer to
        // these bytes, even if a producer subsequently writes the same VA.
        let backing = mapping_identity(address as usize, len as usize);
        let Ok(length) = usize::try_from(len) else {
            return false;
        };
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(length).is_err() {
            return false;
        }
        bytes.resize(length, 0);
        if !crate::host::read_into(address as *const u8, &mut bytes)
            || bytes.get(..8) != Some(header.to_le_bytes().as_slice())
            || mapping_identity(address as usize, len as usize) != backing
        {
            crate::host::refuse_frame(format_args!(
                "packed BVH changed backing during landing at {address:#x}"
            ));
            return false;
        }
        let content = blake3::hash(&bytes);
        if let Some(known) = self.packed.get_mut(&address) {
            known.frame = self.frame;
            if known.content == content && known.backing == backing {
                return true;
            }
            // A replacement of the same extent uses the same set of pages.
            // This submission has already retired earlier GPU readers.
            if known.len == len {
                known.content = content;
                known.backing.clone_from(&backing);
                known.header = header;
            } else {
                self.gone(address);
            }
        }
        let last = (address + len - 1) / PAGE;
        if !self.packed.contains_key(&address) {
            // Reserve the complete range before publishing any ownership. A partial
            // directory must never be recorded as a successful landing.
            let missing: Vec<_> = (page..=last)
                .filter(|&p| self.copy_of(p).is_none())
                .collect();
            if missing.len() > self.free.len()
                || missing.iter().any(|&p| {
                    p < 0x1_0000
                        || p >= u64::from(MEMORY_PAGES) - 1
                        || !crate::texture::committed((p * PAGE) as *const u8, PAGE as usize)
                        || crate::shadow::in_buffer(p * PAGE, PAGE).is_some()
                })
            {
                crate::host::refuse_frame(format_args!(
                    "no complete page reservation for packed BVH at {address:#x} ({len} bytes)"
                ));
                return false;
            }
            for p in missing {
                let (copy, at) = self.free.pop().expect("reserved packed page");
                self.hold(p, at, copy);
                self.pages.get_mut(&p).unwrap().theirs = true;
                crate::watch::wrote(self.entry(p) as usize, 8);
                self.take(p, 0, PAGE as usize, true);
            }
            for p in page..=last {
                if let Some(held) = self.pages.get_mut(&p) {
                    held.owners += 1;
                }
            }
            self.packed.insert(
                address,
                Packed {
                    frame: self.frame,
                    len,
                    header,
                    backing,
                    content,
                },
            );
            self.packed_order.push(address);
            self.stats.packed_bytes += len;
        }
        for page in page..=last {
            let from = address.max(page * PAGE) - page * PAGE;
            let to = (address + len).min((page + 1) * PAGE) - page * PAGE;
            let owner = self.directory_aliases.get(&page).map_or(page, |alias| alias.owner);
            if let Some(held) = self.pages.get(&owner) {
                let src = (page * PAGE + from - address) as usize;
                let n = (to - from) as usize;
                let dst = held.at + from as usize;
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr().add(src), dst as *mut u8, n)
                };
                crate::watch::wrote(dst, n);
                let shadow = &self.copies[held.copy];
                let off = dst - shadow.span().0 as usize;
                shadow.resend(off, off + n);
            } else if let Some(n) = self.copy_of(page) {
                // Buffer-backed entries refresh directly from title memory. An
                // identical CPU value can still replace a GPU-produced value.
                let shadow = &self.copies[n];
                let Some(off) = shadow.offset_in(owner * PAGE + from) else {
                    return false;
                };
                shadow.resend(off as usize, (off + to - from) as usize);
            } else {
                return false;
            }
        }
        self.stats.packed += 1;
        true
    }

    /// Bytes `from..to` of `page` from title memory into its arena's page, at
    /// a submission; `whole`: sent whether they differ from those last taken
    /// or not.
    fn take(&mut self, page: u64, from: usize, to: usize, whole: bool) {
        let Some(held) = self.pages.get(&page) else { return };
        let shadow = &self.copies[held.copy];
        let base = held.at - shadow.span().0 as usize;
        let bulk =
            to - from >= PIECE * 2 && !crate::safecopy::on() && !crate::host::skip_memory_copy();
        if bulk {
            crate::host::copy_mapped(
                self.page_scratch.as_mut_ptr(),
                (page as usize * PAGE as usize + from) as *const u8,
                to - from,
                false,
            );
        }
        let mut local = [0u8; PIECE];
        let mut at = from;
        while at < to {
            let n = (PIECE - at % PIECE).min(to - at);
            let title = if bulk {
                &self.page_scratch[at - from..at - from + n]
            } else {
                crate::host::copy_mapped(
                    local.as_mut_ptr(),
                    (page as usize * PAGE as usize + at) as *const u8,
                    n,
                    false,
                );
                &local[..n]
            };
            // SAFETY: a page of an arena of ours, under the mutex.
            let ours = unsafe { std::slice::from_raw_parts_mut((held.at + at) as *mut u8, n) };
            if *ours != title[..n] {
                ours.copy_from_slice(&title[..n]);
                crate::watch::wrote(held.at + at, n);
            }
            at += n;
        }
        if whole {
            shadow.resend(base + from, base + to);
        }
    }

    /// The pages shaders found empty since the last frame.
    fn take_faults(&mut self) {
        let mut pages = Vec::new();
        for i in 1..MEMORY_FAULTS as usize {
            // SAFETY: inside the faults' mapping; the GPU may be writing,
            // a number missed now is written again next frame.
            let at = unsafe { self.faults.1.add(i) };
            // SAFETY: as above.
            let page = unsafe { at.read_volatile() };
            if page != 0 {
                pages.push(u64::from(page));
                // SAFETY: as above.
                unsafe { at.write_volatile(0) };
            }
        }
        if pages.is_empty() {
            return;
        }
        pages.sort_unstable();
        let mut made = 0;
        for &page in &pages {
            if made < FAULTS && self.faulted < FAULTS_ALL && self.copy_of(page).is_none() && self.map(page) {
                made += 1;
                self.faulted += 1;
                self.bring(page, page + 1);
            }
        }
        self.stats.faults += pages.len() as u64;
        // Each is an access that read 0 or was dropped.
        static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let said = SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if said < 40 || said.is_multiple_of(100) {
            let some: Vec<String> = pages
                .iter()
                .step_by(pages.len().div_ceil(12))
                .map(|&p| format!("{:#x}", p * PAGE))
                .collect();
            tracing::warn!(
                frame = self.frame,
                pages = pages.len(),
                made,
                held = self.pages.len(),
                ?some,
                "title memory by address: shaders touched pages that had no copy (those accesses read 0 or were dropped); those that are the title's are there now"
            );
        }
    }

    /// The structures named in a list's upload memory around `address`
    /// (instances of a top-level build: a structure's address is their last
    /// 8 bytes): the first page of each.
    fn named_in_upload(&mut self, address: u64) {
        let Some((from, len)) = crate::upload::block(address as usize) else {
            return;
        };
        // SAFETY: the block is handed out and stays mapped; the title wrote
        // it before it recorded the dispatch.
        let words = unsafe { std::slice::from_raw_parts(from as *const u64, len / 8) };
        let mut seen = std::collections::HashSet::new();
        for &word in words {
            if word & 0xFF == 0
                && (1 << 32..1 << 37).contains(&word)
                && seen.insert(word)
                && self.map(word / PAGE)
            {
                let page = word / PAGE;
                if let Some((len, _)) = self.packed_at(word) {
                    self.ready(word, len);
                } else {
                    self.bring(page, page + 1);
                }
            }
        }
    }

    /// A small title buffer whose results come back, if its pages are
    /// windows of title memory itself (or a copy of the buffer's own).
    fn small(&mut self, list: &HostList, start: u64, len: u64) {
        let mut at = start;
        while at < start + len {
            let page = at / PAGE;
            let n = ((page + 1) * PAGE).min(start + len) - at;
            self.map(page);
            if let Some(k) = self.copy_of(page)
                && !self.pages.contains_key(&page)
            {
                self.copies[k].refresh(list, at, n);
                self.copies[k].gpu_writes(list, at, n);
                self.stats.back += 1;
            }
            at += n;
        }
    }

    /// Every copy is brought up to date when `list` is submitted, after what
    /// it deferred so far (once until a copy of the title's intervenes).
    fn bring_up(&self, list: &HostList) {
        list.defer_numbered(&self.directory.1, &self.copies);
    }

    /// Every ten seconds or so, what was used.
    fn report(&mut self) {
        let now = std::time::Instant::now();
        let (since, frame) = *self.stats.from.get_or_insert((now, self.frame));
        if now.duration_since(since).as_secs_f64() < 10.0 {
            return;
        }
        let frames = self.frame.wrapping_sub(frame).max(1) as f64;
        let s = std::mem::take(&mut self.stats);
        tracing::info!(
            "title memory by address: {} pages ({} MB) in {} arenas, {} copies in all; per frame {:.0} dispatches that read, {:.0} that build, \
             {:.1} pages that had no copy, {:.1} pages turned down, {:.1} results brought back; {:.0} pages looked at, {:.1} KB changed, \
             {:.1} packed structures ({} there), {:.2} ms; {} windows; since the last line {} KB of packed structures came and {} of them went, \
             and pages for constants {}, views written {}, others {}, upload memory {}, faults in all {}",
            self.pages.len(),
            (self.pages.len() as u64 * PAGE) >> 20,
            self.arenas.len(),
            self.copies.len(),
            s.uses[0] as f64 / frames,
            s.uses[1] as f64 / frames,
            s.faults as f64 / frames,
            s.unknown as f64 / frames,
            s.back as f64 / frames,
            s.looked as f64 / frames,
            s.changed as f64 / 1024.0 / frames,
            s.packed as f64 / frames,
            self.packed.len(),
            s.micros as f64 / 1000.0 / frames,
            self.windows,
            s.packed_bytes >> 10,
            s.gone,
            s.came[0],
            s.came[1],
            s.came[2],
            s.came[3],
            self.faulted
        );
        self.stats.from = Some((now, self.frame));
    }
}

/// A list with this key ([`HostList::used`]) asks the builder for sizes.
const SIZES: usize = usize::MAX - (9 << 33);

/// Whether a submission that asks for the sizes of structures is waited for,
/// and landed, before the title goes on (`VICTORY_BVH_SIZES_WAIT=1`; off).
///
/// Off: it does not help. In run light-15, with it, 33 of 154 copies had the
/// slot's old size still (all of them recorded one frame after the size was
/// asked for) against 53 of 230 in light-11, 48 of 275 dynamic structures
/// were good, and the title asks in every frame, so every frame waits (109 ms
/// against 90). The title reads the slot before our submission of the asking
/// list is through, or reads it for another purpose; not known.
///
/// The title asks for a new structure's compacted size (a program of one
/// thread stores it into a small buffer), reads it a frame or two later
/// without looking whether it is there, takes memory of that size and copies
/// the structure into it. With submissions that land up to two frames late it
/// read what the slot held before, some other structure's size, and the copy
/// wrote over its neighbours: of 230 such copies of run light-11, 99 were
/// asked for two frames earlier and landed in that frame, none of those with
/// the right size.
fn sizes_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_SIZES_WAIT").is_some_and(|v| v == "1"))
}

/// Whether a dispatch of the builder is its program of a structure's size:
/// one thread, the structure's address and what to store (1: the compacted
/// size) as constants, a small title buffer to store into.
fn asks_size(written: &[u64], constants: &[u32], threads: Option<[u32; 3]>) -> bool {
    threads == Some([1, 1, 1])
        && matches!(*constants, [_, hi, 1..=3, ..] if (1..0x20).contains(&hi))
        && written.first().is_some_and(|&at| crate::shadow::in_buffer(at, 1).is_some_and(|b| b.1 <= BACK))
}

/// Read logical GPU addresses after preceding producers. The upload-side
/// baseline distinguishes an intervening CPU replacement from bytes written
/// only on the GPU. All GPU reads in one batch share a single fence.
fn discover_words(addresses: &[u64]) -> Option<Vec<u64>> {
    if addresses.is_empty() {
        return Some(Vec::new());
    }
    let mut values = Vec::with_capacity(addresses.len());
    let mut pieces = Vec::new();
    {
        let guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
        for (index, &at) in addresses.iter().enumerate() {
            if at % 8 != 0 {
                return None;
            }
            let cpu = crate::bvh::read::<8>(at);
            let mut mask = [false; 2];
            let arena_word = |va: u64| guard.as_ref().and_then(|m| {
                let page = va / PAGE;
                let owner = m.directory_aliases.get(&page).map_or(page, |a| a.owner);
                if !m.gpu_pages.contains(&owner) || m.declared_backing.get(&owner).is_some_and(|b|
                    *b != mapping_identity((owner * PAGE) as usize, PAGE as usize)) {
                    return None;
                }
                let offset = u64::from(unsafe { m.entry(owner).add(1).read() }) + va % PAGE;
                let shadow = m.copies.get(m.copy_of(owner)?)?;
                let baseline = m.pages.get(&owner).and_then(|h| crate::bvh::read::<8>(h.at as u64 + va % PAGE))
                    .or_else(|| shadow.raw_baseline((owner * PAGE + va % PAGE) as usize));
                Some((shadow.clone(), offset, baseline))
            });
            let buffer_word = |va: u64| {
                let s = crate::shadow::peek(va, 8).filter(|s| s.written())?;
                let off = s.offset(va);
                Some((s, off, None))
            };
            let owner = arena_word(at).or_else(|| buffer_word(at)).or_else(|| {
                // An instance can contain the interior VA of another view of
                // the producer's pages before that VA has a directory entry.
                crate::inplace::aliases(at as usize, 8).into_iter().find_map(|alias| {
                    arena_word(alias as u64).or_else(|| buffer_word(alias as u64))
                })
            });
            if let Some((shadow, offset, baseline)) = owner {
                let baseline = baseline.or_else(|| shadow.carded().then_some(()).and_then(|_| {
                    crate::bvh::read::<8>(unsafe { shadow.card_data().add(offset as usize) as u64 })
                }));
                if let Some(last) = baseline {
                    for i in 0..2 {
                        mask[i] = cpu.is_none_or(|b| b[i * 4..i * 4 + 4] == last[i * 4..i * 4 + 4]);
                    }
                }
                if mask.contains(&true) {
                    pieces.push((index, shadow.buffer.clone(), offset, mask));
                }
            }
            if cpu.is_none() && mask != [true, true] {
                return None;
            }
            values.push(cpu.unwrap_or_default());
        }
    }
    if !pieces.is_empty() {
        let host = host()?;
        let back = host.read_back_buffer((pieces.len() * 8) as u64)?;
        let mut transfers: Vec<(ID3D12Resource, u64, ID3D12Resource, u64, u64)> = Vec::new();
        for (i, (_, src, offset, _)) in pieces.iter().enumerate() {
            if let Some((_, _, previous, start, size)) = transfers.last_mut() {
                if previous == src && start.checked_add(*size) == Some(*offset) {
                    *size += 8;
                    continue;
                }
            }
            transfers.push((back.clone(), (i * 8) as u64, src.clone(), *offset, 8));
        }
        host.copy_preserved_buffers(&transfers)?;
        let mut data = std::ptr::null_mut();
        unsafe {
            back.Map(0, None, Some(&mut data)).ok()?;
            if data.is_null() {
                back.Unmap(0, None);
                return None;
            }
            for (i, (index, _, _, mask)) in pieces.iter().enumerate() {
                let bytes = data.cast::<[u8; 8]>().add(i).read_unaligned();
                for d in 0..2 {
                    if mask[d] {
                        values[*index][d * 4..d * 4 + 4].copy_from_slice(&bytes[d * 4..d * 4 + 4]);
                    }
                }
            }
            back.Unmap(0, None);
        }
    }
    Some(values.into_iter().map(u64::from_le_bytes).collect())
}

fn discover_instances(ranges: &[[u64; 3]]) -> Option<InstanceGraph> {
    let mut graph = InstanceGraph::new();
    for &[at, len, tag] in ranges {
        let kind = tag & 255;
        if !matches!(kind, crate::bvh::INSTANCES | crate::bvh::INSTANCE_POINTERS) {
            continue;
        }
        let indirect = kind == crate::bvh::INSTANCE_POINTERS;
        let step = if indirect { 8 } else { 64 };
        let count = len / step;
        let records = graph.entry(at).or_default();
        for start in (0..count).step_by(2048) {
            let mut addresses = (start..count.min(start + 2048))
                .map(|i| at.checked_add(i * step))
                .collect::<Option<Vec<_>>>()?;
            if indirect {
                addresses = discover_words(&addresses)?;
            }
            let fields = addresses
                .iter()
                .map(|at| at.checked_add(56))
                .collect::<Option<Vec<_>>>()?;
            let children = discover_words(&fields)?;
            let mut unknown = children.clone();
            unknown.sort_unstable();
            unknown.dedup();
            {
                let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
                unknown.retain(|&at| at != 0 && guard.as_mut().is_none_or(|m| m.structure_at(at).is_none()));
            }
            let headers: HashMap<_, _> = unknown.iter().copied().zip(discover_words(&unknown)?).collect();
            records.extend(addresses.into_iter().zip(children).map(|(at, blas)| {
                (at, blas, headers.get(&blas).copied().unwrap_or(0))
            }));
        }
    }
    Some(graph)
}

/// Run after preceding producers have retired. Tracked TLASes carry the
/// original instance dependencies. For an external TLAS, retail XDXR
/// reads the byte offset at +52, strides its instances by 128, and shifts
/// the dword at instance+60 left eight to obtain the referenced BLAS.
fn reserve_trace_views(mut views: Vec<ViewSpan>, rays: bool, observe: bool) -> Option<Option<(u64, u64)>> {
    use std::collections::HashSet;
    views.sort_unstable();
    views.dedup();
    let mut seen = HashSet::new();
    let mut spans = Vec::new();
    let mut next = Vec::new();
    let mut published = None;
    while !views.is_empty() {
        let mut external = Vec::new();
        {
            let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
            let m = guard.as_mut()?;
            for view in views.drain(..) {
                let at = view.at;
                if at == 0 || !seen.insert(view) {
                    continue;
                }
                if let Some(base) = m.structure_at(at) {
                    if !m.published_range(base) {
                        tracing::error!(address = format_args!("{at:#x}"), "traversal preceded publication of its acceleration structure");
                        return None;
                    }
                    let s = m.structures.get(&base)?;
                    if observe && published.is_none() {
                        published = Some((base, s.generation));
                    }
                    next.extend(s.dependencies.iter().map(|&(at, _, _)| ViewSpan { at, bytes: None }));
                    spans.push((at, s.size));
                    if let Some(bytes) = view.bytes.filter(|&bytes| bytes > s.size) {
                        spans.push((at, bytes));
                    }
                    external.push((at, true, Some(s.size)));
                } else if let Some(len) = view.bytes {
                    if len > 0 {
                        spans.push((at, len));
                    }
                    // Retail raw SRVs can expose a TLAS just as an AS SRV
                    // can. Follow it if its unpacked header identifies one.
                    if rays && len >= 64 && at % 256 == 0 {
                        external.push((at, false, Some(len)));
                    }
                } else {
                    if at % 256 != 0 || at < 1 << 32 || at >= (u64::from(MEMORY_PAGES) - 1) * PAGE {
                        tracing::error!(address = format_args!("{at:#x}"), "invalid acceleration structure address");
                        return None;
                    }
                    external.push((at, true, None));
                }
            }
        }
        for batch in external.chunks(2048) {
            let fields: Vec<_> = batch.iter().flat_map(|&(at, _, _)| [at, at + 8, at + 48]).collect();
            let headers = discover_words(&fields)?;
            for (&(at, required, limit), words) in batch.iter().zip(headers.chunks_exact(3)) {
                let header = words[0];
                if !required && (header as u16 != 6 || header & (1 << 24) != 0) {
                    continue;
                }
                if header as u16 != 6 || header & (1 << 24) != 0 {
                    tracing::error!(address = format_args!("{at:#x}"), header = format_args!("{header:#x}"), "traversal encountered an invalid unpacked header");
                    return None;
                }
                let size = ((header >> 32) + 1) * 64;
                if size > 2 << 30 || limit.is_some_and(|bytes| size > bytes) {
                    if !required { continue; }
                    tracing::error!(address = format_args!("{at:#x}"), size, ?limit, "acceleration structure exceeds its view");
                    return None;
                }
                if header & (1 << 31) == 0 {
                    let count = words[1] >> 32;
                    let offset = words[2] >> 32;
                    if count != 0 && (offset < 64 || offset % 64 != 0 || offset + 128 * count > size) {
                        if !required { continue; }
                        return None;
                    }
                    for start in (0..count).step_by(2048) {
                        let fields: Vec<_> = (start..count.min(start + 2048))
                            .map(|i| at + offset + i * 128 + 56).collect();
                        next.extend(discover_words(&fields)?.into_iter().map(|w| ViewSpan {
                            at: (w >> 32) << 8,
                            bytes: None,
                        }));
                    }
                }
                spans.push((at, size));
            }
        }
        std::mem::swap(&mut views, &mut next);
    }
    // A reservation may refresh the CPU-side upload comparison bytes. Finish
    // all discovery against the preceding producer before changing them.
    merge_trace_spans(&mut spans);
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    let m = guard.as_mut()?;
    spans.into_iter().all(|(at, bytes)| {
        if m.reserve(at, bytes, false) { return true; }
        tracing::error!(address = format_args!("{at:#x}"), bytes, "traversal range unavailable");
        false
    }).then_some(published)
}

/// Discovery may reach a BLAS by its header, its published size, and a
/// containing raw SRV. Reserve their exact union after all header reads.
/// Low addresses retain each declaration's original buffer provenance.
fn merge_trace_spans(spans: &mut Vec<(u64, u64)>) {
    spans.sort_unstable();
    spans.dedup();
    let mut count = 0;
    for i in 0..spans.len() {
        let (at, size) = spans[i];
        if let Some((base, length)) = spans[..count].last_mut()
            && *base >= 1 << 32 && size != 0 && *length != 0
            && let Some(end) = base.checked_add(*length)
            && let Some(next_end) = at.checked_add(size)
            && at <= end
        {
            *length = end.max(next_end) - *base;
        } else {
            spans[count] = (at, size);
            count += 1;
        }
    }
    spans.truncate(count);
}

/// Contiguous face tables are often still CPU-owned. While remaps are held,
/// prove there is no written arena page, buffer, window or texture in any
/// alias. Otherwise the ordinary word reader resolves each native producer.
fn cpu_geometry_words(addresses: &[u64]) -> Option<Vec<u64>> {
    let &first = addresses.first()?;
    let end = addresses.last()?.checked_add(8)?;
    let bytes = usize::try_from(end.checked_sub(first)?).ok()?;
    if addresses.len() < 8 || bytes > addresses.len().checked_mul(32)? || bytes > 64 * 1024
        || !addresses.windows(2).all(|pair| pair[0] <= pair[1])
        || addresses.iter().any(|at| at % 8 != 0)
    {
        return None;
    }
    let mapping = crate::watch::title_memory_reading_now()?;
    let backing = crate::inplace::backing(first as usize, bytes);
    crate::inplace::guarded_view_epoch(first as usize, end as usize, &backing)?;
    let generation = crate::resource::alias_generation();
    let page = first & !(PAGE - 1);
    let pages_end = end.next_multiple_of(PAGE);
    let ranges = crate::inplace::aliases_of_bytes(page as usize, (pages_end - page) as usize);
    let guard = MEMORY.try_lock().ok()?;
    let m = guard.as_ref()?;
    for &(lo, hi) in &ranges {
        if (lo as u64 / PAGE..(hi as u64).div_ceil(PAGE)).any(|p| {
            let owner = m.directory_aliases.get(&p).map_or(p, |a| a.owner);
            m.gpu_pages.contains(&owner)
        }) { return None; }
    }
    if crate::resource::texture_writer_overlaps(&ranges) || crate::shadow::has_word_producer(&ranges) {
        return None;
    }
    let mut data = vec![0u8; bytes];
    if !crate::host::read_into(first as *const u8, &mut data)
        || generation != crate::resource::alias_generation() || crate::shadow::has_word_producer(&ranges)
    {
        return None;
    }
    drop(guard);
    drop(mapping);
    addresses.iter().map(|&at| {
        let offset = (at - first) as usize;
        Some(u64::from_le_bytes(data.get(offset..offset + 8)?.try_into().ok()?))
    }).collect()
}

/// Resolve the packed headers after preceding producers and before the first
/// in-place kernel can overwrite them. All source words share one GPU fence.
fn build_ranges(ranges: &[[u64; 3]], generation: u64) -> Option<Arc<[[u64; 3]]>> {
    use crate::bvh::{INPUT, OUTPUT, PACKED_GEOMETRY, PACKED_INPUT, REHYDRATE};
    const HEADER_OFFSETS: [u64; 5] = [0, 8, 40, 48, 56];
    let mut addresses = Vec::new();
    for &[_, source, kind] in ranges {
        if kind & 255 == REHYDRATE {
            for offset in HEADER_OFFSETS { addresses.push(source.checked_add(offset)?); }
        }
    }
    let headers = discover_words(&addresses)?;
    let mut headers = headers.chunks_exact(HEADER_OFFSETS.len());
    let mut declared = Vec::with_capacity(ranges.len() + headers.len());
    let stamp = generation << 8;
    let mut packed_extents = std::collections::HashMap::new();
    for &[at, len, kind] in ranges {
        if kind & 255 == PACKED_GEOMETRY {
            // Winding variants can use the exact same index stream in the
            // same batch. Discovery precedes this batch's first dispatch.
            let size = if let Some(&size) = packed_extents.get(&(at, len)) {
                size
            } else {
                let size = crate::bvh::packed_index_extent(at, len, |addresses| {
                    cpu_geometry_words(addresses).or_else(|| discover_words(addresses))
                }).or_else(|| {
                    tracing::error!(address=format_args!("{at:#x}"), indices=len, "invalid packed BVH index stream at submission");
                    None
                })?;
                packed_extents.insert((at, len), size);
                size
            };
            declared.push([at, size, stamp | INPUT]);
            continue;
        }
        if kind & 255 != REHYDRATE {
            declared.push([at, len, stamp | (kind & 255)]);
            continue;
        }
        let mut header = [0u8; 64];
        for (offset, &word) in HEADER_OFFSETS.into_iter().zip(headers.next()?) {
            let offset = offset as usize;
            header[offset..offset + 8].copy_from_slice(&word.to_le_bytes());
        }
        let (input, output) = crate::bvh::rehydration_extents(&header, at == len).or_else(|| {
            tracing::error!(source = format_args!("{len:#x}"), header = ?&header[..8], "invalid packed BVH header at submission");
            None
        })?;
        declared.extend([[len, input, stamp | PACKED_INPUT], [at, output, stamp | OUTPUT]]);
    }
    Some(declared.into())
}

/// A recording can be submitted repeatedly. Assign its output generation
/// here, on the ordered queue, and preserve the preceding published bytes
/// before any stage of an in-place refit can write them.
pub(crate) fn begin_build(ranges: &[[u64; 3]]) -> Option<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let key = ranges.first()?[2] >> 8;
    let generation = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let host = host()?;
    {
        let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Memory::new();
        }
        let m = guard.as_mut()?;
        m.reconcile_aliases();
        let frame = crate::frame::current();
        if m.frame != frame {
            m.frame = frame;
            m.refused.clear();
            m.look_again();
            m.report();
        }
    }
    let declared = build_ranges(ranges, generation)?;
    let instances = Arc::new(discover_instances(ranges).or_else(|| { tracing::error!(?ranges, "BVH instance graph unreadable"); None })?);
    let mut outputs = Vec::new();
    let mut copies = Vec::new();
    {
        let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
        let m = guard.as_mut()?;
        if m.versions.contains_key(&key) {
            return None;
        }
        for &[at, _, kind] in ranges {
            if !matches!(
                kind & 255,
                crate::bvh::OUTPUT | crate::bvh::REHYDRATE | crate::bvh::COMPACT_COPY
            ) || outputs.iter().any(|p: &PriorOutput| p.at == at)
            {
                continue;
            }
            let at = m.structure_at(at).unwrap_or(at);
            if outputs.iter().any(|p: &PriorOutput| p.at == at) {
                continue;
            }
            let previous = m
                .structures
                .get(&at)
                .filter(|s| s.published && m.published_range(at))
                .cloned();
            let bytes = previous.as_ref().map_or(0, |s| s.size);
            let backing = mapping_identity(at as usize, bytes as usize);
            let mut pieces = Vec::new();
            let mut snapshot = None;
            if bytes != 0 {
                let saved = host.uav_buffer(bytes)?;
                let mut pos = 0;
                while pos < bytes {
                    let addr = at.checked_add(pos)?;
                    let page = addr / PAGE;
                    let idx = m.copy_of(page)?;
                    let amount = (bytes - pos).min(PAGE - addr % PAGE);
                    let offset = u64::from(unsafe { m.entry(page).add(1).read() }) + addr % PAGE;
                    let buffer = m.copies.get(idx)?.buffer.clone();
                    copies.push((saved.clone(), pos, buffer.clone(), offset, amount));
                    pieces.push((buffer, offset, amount));
                    pos += amount;
                }
                snapshot = Some(saved);
            }
            outputs.push(PriorOutput {
                at,
                previous,
                bytes,
                backing,
                pieces,
                snapshot,
            });
        }
    }
    host.copy_preserved_buffers(&copies)?;
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    let m = guard.as_mut()?;
    m.versions.insert(
        key,
        BuildVersion {
            ranges: declared.clone(),
            generation,
            outputs,
            instances: instances.clone(),
        },
    );
    m.reserve_ranges(&declared, &instances).then_some(())
}

/// Called under the host submission lock. Restore the previous version of
/// in-place outputs and discard new outputs before a later frame can trace.
pub(crate) fn abort_builds() {
    let versions = {
        let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
        let Some(m) = guard.as_mut() else { return };
        std::mem::take(&mut m.versions)
    };
    for version in versions.into_values() {
        let mut copies = Vec::new();
        for out in &version.outputs {
            if let Some(saved) = &out.snapshot
                && mapping_identity(out.at as usize, out.bytes as usize) == out.backing
            {
                let mut pos = 0;
                for (dst, offset, len) in &out.pieces {
                    copies.push((dst.clone(), *offset, saved.clone(), pos, *len));
                    pos += len;
                }
            }
        }
        let restored = host().is_some_and(|host| host.copy_preserved_buffers(&copies).is_some());
        let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
        let Some(m) = guard.as_mut() else { return };
        for out in version.outputs {
            if m.structures.get(&out.at).is_some_and(|s| s.generation == version.generation)
                && restored
                && let Some(s) = out.previous
                && mapping_identity(out.at as usize, out.bytes as usize) == out.backing {
                m.structures.insert(out.at, s);
            }
            // Otherwise leave the unpublished generation as a tombstone.
            // A plausible header in a partly written output is not an
            // external, pre-built structure a later trace can discover.
        }
    }
}

/// The completion of the group's last stage publishes its outputs together.
pub(crate) fn published(outputs: &[(u64, u64)]) {
    let Some(&(_, key)) = outputs.first() else {
        return;
    };
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    let Some(m) = guard.as_mut() else { return };
    let Some(version) = m.versions.get(&key) else {
        return;
    };
    let generation = version.generation;
    let canonical: Option<Vec<_>> = outputs.iter().map(|&(at, _)| m.structure_at(at)).collect();
    if !canonical.as_ref().is_some_and(|bases| bases.iter().all(|&at| {
        m.structures.get(&at).is_some_and(|s| {
            s.generation == generation
                && mapping_identity(at as usize, s.size as usize) == s.backing
        })
    })) {
        crate::host::refuse_frame(format_args!(
            "BVH output backing changed before publication"
        ));
        return;
    }
    let canonical = canonical.unwrap();
    for at in &canonical {
        m.structures.get_mut(at).unwrap().published = true;
    }
    m.versions.remove(&key);
    static REPORTED: AtomicUsize = AtomicUsize::new(0);
    if trace_enabled() && REPORTED.load(Ordering::Relaxed) < TRACE_LIMIT
        && REPORTED.fetch_add(1, Ordering::Relaxed) < TRACE_LIMIT
    {
        tracing::info!(frame = crate::frame::current(), generation, outputs = canonical.len(),
            first_address = format_args!("{:#x}", canonical[0]), "retired BVH build published");
    }
}

const TRACE_LIMIT: usize = 16;
static TRAVERSALS_REPORTED: AtomicUsize = AtomicUsize::new(0);

fn trace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    cfg!(feature = "diagnostics")
        && *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_TRACE").is_some_and(|v| v == "1"))
}

pub(crate) type TraversalTrace = Arc<Mutex<Option<(String, u64, u64)>>>;

pub(crate) fn trace_retired_traversal(trace: &TraversalTrace) {
    if let Some((shader, address, generation)) = trace.lock().unwrap_or_else(|e| e.into_inner()).take()
        && TRAVERSALS_REPORTED.load(Ordering::Relaxed) < TRACE_LIMIT
        && TRAVERSALS_REPORTED.fetch_add(1, Ordering::Relaxed) < TRACE_LIMIT
    {
        tracing::info!(frame = crate::frame::current(), shader, published_generation = generation,
            address = format_args!("{address:#x}"), "retired ray traversal referenced a published BVH");
    }
}

pub(crate) struct SizeResult {
    source: u64,
    generation: Arc<std::sync::atomic::AtomicU64>,
    buffer: ID3D12Resource,
    address: u64,
    owner: Arc<Shadow>,
    backing: Vec<(usize, usize, usize, u64, u64)>,
}

impl SizeResult {
    pub(crate) fn range(&self) -> (usize, usize) {
        (self.address as usize, self.address as usize + 8)
    }

    pub(crate) fn land(&self) {
        let _mapped = crate::watch::title_memory_reading();
        if !crate::shadow::find(self.address, 8).is_some_and(|s| Arc::ptr_eq(&s, &self.owner))
            || mapping_identity(self.address as usize, 8) != self.backing
        {
            crate::host::refuse_frame(format_args!(
                "compacted-size destination was replaced at {:#x}",
                self.address
            ));
            return;
        }
        let mut mapped = std::ptr::null_mut();
        unsafe {
            if self.buffer.Map(0, None, Some(&mut mapped)).is_ok() && !mapped.is_null() {
                let size = mapped.cast::<u64>().read_unaligned();
                let written =
                    crate::host::write_mapped_exact(self.address as usize, mapped.cast(), 8);
                self.buffer.Unmap(0, None);
                if !written {
                    crate::host::refuse_frame(format_args!(
                        "could not publish compacted size at {:#x}",
                        self.address
                    ));
                    return;
                }
                drop(_mapped);
                if let Some(m) = MEMORY.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
                    && let Some(base) = m.structure_at(self.source)
                    && let Some(s) = m.structures.get_mut(&base)
                    && self.generation.load(std::sync::atomic::Ordering::Acquire) == s.generation
                    && size != 0
                    && size <= s.size
                    && mapping_identity(base as usize, s.size as usize) == s.backing
                {
                    s.compacted = Some(size);
                }
            } else {
                crate::host::refuse_frame(format_args!("could not map compacted-size readback"));
            }
        }
    }
}

pub(crate) fn size_result(list: &HostList, address: u64, source: u64) -> Option<SizeResult> {
    use windows::Win32::Graphics::Direct3D12::*;
    let owner = crate::shadow::find(address, 8)?;
    owner.offset_in(address)?;
    let offset = owner.offset(address);
    if owner.left(address) < 8 {
        return None;
    }
    let buffer = crate::host::host()?.read_back_buffer(8)?;
    let (uav, src) = (
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    unsafe {
        list.list
            .ResourceBarrier(&[crate::host::transition(&owner.buffer, uav, src)]);
        list.list
            .CopyBufferRegion(&buffer, 0, &owner.buffer, offset, 8);
        list.list
            .ResourceBarrier(&[crate::host::transition(&owner.buffer, src, uav)]);
    }
    let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let stamp = generation.clone();
    list.defer_host(move || {
        let current = MEMORY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .and_then(|m| m.structure_at(source).map(|base| m.structures[&base].generation))
            .unwrap_or(0);
        stamp.store(current, std::sync::atomic::Ordering::Release);
    });
    Some(SizeResult {
        buffer,
        address,
        owner,
        generation,
        source,
        backing: mapping_identity(address as usize, 8),
    })
}

/// Whether one of `lists` asks for sizes.
pub(crate) fn asked_sizes<'a>(lists: impl Iterator<Item = &'a HostList>) -> bool {
    let mut lists = lists;
    let asked = sizes_wait() && lists.any(|l| l.used(SIZES));
    if asked {
        static TIMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = TIMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if n.is_power_of_two() || n.is_multiple_of(100) {
            tracing::info!(n, frame = crate::frame::current(), "submissions that ask for structures' sizes, waited for");
        }
    }
    asked
}

/// [`fill`] of a page stores by address do not reach.
fn close(entry: *mut u32, n: usize, offset: u64) {
    fill(entry, n | MEMORY_READ_ONLY as usize, offset);
}

/// The offset first: a shader that sees the number sees it too.
fn fill(entry: *mut u32, n: usize, offset: u64) {
    // SAFETY: an entry of the directory, under the mutex.
    unsafe {
        entry.add(1).write_volatile(offset as u32);
        entry.write_volatile(n as u32 + 1);
    }
}

/// The bytes of a packed structure whose first two dwords these are: the
/// version (6) with the marks of a bottom level and of one that is packed,
/// and its size unpacked in 64-byte nodes.
fn packed_len(header: u64) -> Option<u64> {
    let (version, nodes) = (header as u32, header >> 32);
    let marks = version >> 16;
    // The header counts its nodes of 64 bytes without the last one: the
    // builder's own program of a structure's size stores one more.
    let len = (nodes + 1) * 64;
    (version & 0xFFFF == 6 && marks & 0x8100 == 0x8100 && nodes > 0 && len <= PACKED).then_some(len)
}

/// Puts what `title` holds into `ours` (what was last taken): the dwords that
/// differ. How many.
fn merge(title: &[u8; PIECE], ours: &mut [u8], never: bool) -> usize {
    if never {
        ours.copy_from_slice(title);
        return PIECE / 4;
    }
    if ours == title {
        return 0;
    }
    let mut changed = 0;
    for (t, o) in title.chunks_exact(4).zip(ours.chunks_exact_mut(4)) {
        if t != o {
            o.copy_from_slice(t);
            changed += 1;
        }
    }
    changed
}

/// Before a draw whose shader reads title memory by address: what its
/// root arguments name is made ready (`addresses`: its root views, of them
/// `written` those it writes; `constants`: its root constants, see the
/// module), every numbered copy
/// has its view in `heap`'s run and is brought up to date when `list` is
/// submitted. None if the host cannot hold it; the draw is then dropped.
pub(crate) fn ready(
    list: &HostList,
    heap: &Arc<HostHeap>,
    addresses: &[u64],
    written: &[u64],
    constants: &[u32],
    use_: Use,
    threads: Option<[u32; 3]>,
    ranges: &[[u64; 3]],
    views: &[ViewSpan],
    indexed: crate::descriptor::host::IndexedSlots,
    ray_shader: Option<(&str, bool)>,
) -> Option<Ready> {
    let declared = ranges.first().filter(|_| matches!(use_, Use::Builds { .. }))
        .map(|first| list.builder_inputs(first[2] >> 8, Arc::as_ptr(heap) as usize));
    match use_ {
        Use::Reads => {}
        Use::SizeQuery { destination, .. } => list.guest_write(Some((destination as usize, 8))),
        Use::Builds { .. } if ranges.is_empty() => list.guest_write(None),
        Use::Builds { .. } if declared.as_ref().is_some_and(|(_, first)| !first) => {}
        Use::Builds { .. } => {
            use crate::bvh::{COMPACT_COPY, COPY_EXTENT, OUTPUT, REHYDRATE, SCRATCH};
            for &[at, len, stamp] in ranges {
                match stamp & 255 {
                    OUTPUT | SCRATCH => list.guest_write(Some((at as usize, len as usize))),
                    COMPACT_COPY | REHYDRATE => {
                        // These declarations carry a source VA. The final byte
                        // count is resolved in submission order.
                        let extent = ranges.iter().find(|r| r[0] == at && r[2] & 255 == COPY_EXTENT)
                            .map(|r| r[1]).or_else(|| crate::shadow::in_buffer(at, 1)
                                .and_then(|(base, bytes)| bytes.checked_sub(at - base)));
                        list.guest_write(extent.map(|bytes| (at as usize, bytes as usize)));
                    }
                    _ => {}
                }
            }
        }
    }
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let m = guard.as_mut()?;
    // Declared builders and their consumers acquire directory entries in
    // submission order. A later recording cannot replace an earlier reader's
    // entries while its commands are waiting for a pipeline or queue fence.
    if !ranges.is_empty() || matches!(use_, Use::SizeQuery { .. }) || use_ == Use::Reads && crate::pipeline::builder_enabled() {
        let base = m.descriptors(heap)?;
        let ready = Ready {
            directory: m.directory.1.gpu(m.directory.1.span().0),
            faults: m.faults.2,
            base: heap.physical(base),
        };
        list.keep(&m.faults.0);
        if use_ == (Use::Builds { back: true }) {
            for &va in written {
                if let Some((start, len)) = crate::shadow::in_buffer(va, 1)
                    && len <= BACK
                    && let Some(shadow) = crate::shadow::find(start, len)
                {
                    shadow.refresh(list, start, len);
                    shadow.gpu_writes(list, start, len);
                }
            }
        }
        let heap = heap.clone();
        let fixed = views.to_vec();
        let mut pointers = addresses.to_vec();
        if use_ != Use::Reads {
            for w in constants.windows(2) {
                if (1..256).contains(&w[1]) {
                    pointers.push(u64::from(w[0]) | u64::from(w[1]) << 32);
                }
            }
        }
        pointers.sort_unstable();
        pointers.dedup();
        if let Some((inputs, first_stage)) = declared {
            let key = ranges[0][2] >> 8;
            inputs.lock().unwrap_or_else(|e| e.into_inner()).extend(
                pointers.into_iter().filter(|at| (1 << 32..1 << 37).contains(at))
            );
            if first_stage {
                list.defer_builder(move || {
                    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
                    let m = guard.as_mut()?;
                    // begin_build has already validated and reserved the full
                    // producer generation. In-place rehydration must upload the
                    // packed input once, before the first kernel overwrites it.
                    m.versions.get(&key)?;
                    for &at in inputs.lock().unwrap_or_else(|e| e.into_inner()).iter() {
                        if let Some(base) = m.structure_at(at) {
                            if !m.reserve(at, m.structures[&base].size, false) { return None; }
                        } else {
                            m.ready(at, 1);
                        }
                    }
                    m.descriptors(&heap)?;
                    Some(std::iter::once(m.directory.1.clone()).chain(m.copies.iter().cloned()).collect())
                });
            }
            return Some(ready);
        }
        let spans = ranges.to_vec();
        let rays = ray_shader.is_some();
        let trace = ray_shader.filter(|&(_, issued)| issued && use_ == Use::Reads && trace_enabled()
                && TRAVERSALS_REPORTED.load(Ordering::Relaxed) < TRACE_LIMIT)
            .map(|(shader, _)| (shader.to_owned(), list.traversal_trace()));
        list.defer_builder(move || (|| {
            let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
            let m = guard.as_mut()?;
            m.reconcile_aliases();
            let frame = crate::frame::current();
            if m.frame != frame {
                m.frame = frame;
                m.refused.clear();
                m.look_again();
                m.report();
            }
            if let Some(first) = spans.first() {
                let version = m.versions.get(&(first[2] >> 8))?;
                let (versioned, instances) = (version.ranges.clone(), version.instances.clone());
                if !m.reserve_ranges(&versioned, &instances) {
                    return None;
                }
            }
            if use_ == Use::Reads {
                // A raw root may carry the structure VA itself without any
                // descriptor. Look through the same GPU version that will
                // be read by the traversal, including its instance records.
                let mut views = fixed.clone();
                views.extend(heap.address_candidates(&indexed));
                let mut untyped = Vec::new();
                for &at in &pointers {
                    if at < 1 << 32 || views.iter().any(|v| v.at == at) {
                        continue;
                    }
                    if m.structure_at(at).is_some() {
                        views.push(ViewSpan { at, bytes: None });
                    } else if crate::texture::committed_by_query(at as *const u8, 8) {
                        untyped.push(at);
                    }
                }
                drop(guard);
                for at in untyped {
                    let len = crate::shadow::in_buffer(at, 1).map(|(base, len)| len - (at - base))
                        .or_else(|| crate::shadow::mapped_root_bytes(at))?;
                    views.push(ViewSpan { at, bytes: Some(len) });
                }
                let count = views.len();
                let published = reserve_trace_views(views, rays, trace.is_some())
                    .or_else(|| { tracing::error!(count, "could not reserve traversal views"); None })?;
                if let (Some((shader, trace)), Some((address, generation))) = (&trace, published) {
                    let mut trace = trace.lock().unwrap_or_else(|e| e.into_inner());
                    if trace.is_none() {
                        *trace = Some((shader.clone(), address, generation));
                    }
                }
                guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
            } else if let Use::SizeQuery { source, destination } = use_ {
                let source_base = m.structure_at(source).or_else(|| { tracing::error!(?use_, "BVH size query has no producer"); None })?;
                if !m.published_range(source_base) || !m.reserve(source, m.structures[&source_base].size, false)
                    || !m.reserve(destination, 8, true) {
                    return None;
                }
            } else {
                for &at in &pointers {
                    if !(1 << 32..1 << 37).contains(&at) {
                        continue;
                    }
                    let canonical = m.structure_at(at).unwrap_or(at);
                    if let Some(s) = m.structures.get(&canonical) {
                        if !m.reserve(at, s.size, false) {
                            return None;
                        }
                    } else {
                        m.ready(at, 1);
                    }
                }
            }
            let m = guard.as_mut()?;
            m.descriptors(&heap)?;
            Some(
                std::iter::once(m.directory.1.clone())
                    .chain(m.copies.iter().cloned())
                    .collect(),
            )
        })().or_else(|| { tracing::error!(?use_, ?spans, ?pointers, "could not bind BVH address directory"); None }));
        return Some(ready);
    }
    let frame = crate::frame::current();
    if m.frame != frame {
        m.frame = frame;
        m.refused.clear();
        m.take_faults();
        m.look_again();
        m.report();
        #[cfg(feature = "diagnostics")]
        m.trace.look(frame);
    }
    #[cfg(feature = "diagnostics")]
    if use_ != Use::Reads {
        m.trace.dispatch(frame, list, addresses, written, constants, threads);
    }
    let builds = use_ != Use::Reads;
    if builds && sizes_wait() && asks_size(written, constants, threads) {
        list.first_use(SIZES);
    }
    while builds && m.free.len() < SPARE && m.pages.len() + m.free.len() < MOST && m.another() {}
    m.stats.uses[usize::from(builds)] += 1;
    // Once per list and page named: the builder dispatches hundreds of times
    // a frame with the same arguments.
    const READ: usize = usize::MAX - 0x6d;
    let named = |kind: usize, at: u64| list.first_use(usize::MAX - (kind << 26) - (at / PAGE) as usize);
    // A title address in two constants (low dword, then bits 32 to 39), or
    // in one in 256-byte units (the builder's alone). A constant that only
    // looks like one names memory nothing reads; one that is not the title's
    // memory is refused.
    let held = |m: &Memory| m.pages.len() as u64;
    let before = held(m);
    for (i, &c) in constants.iter().enumerate() {
        if let Some(&high) = constants.get(i + 1)
            && (1..=0x1F).contains(&high)
            && c >= 0x1_0000
        {
            let at = u64::from(c) | u64::from(high) << 32;
            if named(1, at) {
                m.ready(at, 1);
            }
        }
        if builds && (0x0100_0000..0x2000_0000).contains(&c) {
            let at = u64::from(c) << 8;
            if named(1, at) {
                m.ready(at, 1);
            }
        }
    }
    let mut came = [held(m) - before, 0, 0, 0];
    for &at in addresses.iter().filter(|&&a| a >= 1 << 32) {
        // (Several structures to a page.)
        let mut packed = false;
        if builds
            && ranges.is_empty()
            && list.first_use(usize::MAX - (3 << 33) - (at >> 8) as usize)
            && crate::shadow::in_buffer(at, 1).is_none()
            && m.possible(at / PAGE)
        {
            // What it needs, as far as it is known now; the rest when the
            // list is submitted ([`Memory::land`]).
            if let Some((len, _)) = m.packed_at(at) {
                packed = true;
                for page in at / PAGE..=(at + len - 1) / PAGE {
                    let new = m.copy_of(page).is_none();
                    if m.map(page)
                        && new
                        && let Some(held) = m.pages.get_mut(&page)
                    {
                        held.theirs = true;
                    }
                }
                m.bring(at / PAGE, (at + len - 1) / PAGE + 1);
            }
            list.defer_landing(move || {
                if let Some(m) = MEMORY.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    m.land(at, None);
                }
            });
            m.bring_up(list);
        }
        if builds && written.contains(&at) && list.first_use(usize::MAX - (4 << 33) - (at >> 2 & 0x7FFF_FFFF) as usize) {
            if let Some((start, len)) = crate::shadow::in_buffer(at, 1)
                && len <= BACK
                && use_ == (Use::Builds { back: true })
                && list.first_use(usize::MAX - (5 << 33) - (start >> 2 & 0x7FFF_FFFF) as usize)
            {
                m.small(list, start, len);
            }
            m.opens(at);
            if ranges.is_empty() && !packed && crate::shadow::in_buffer(at, 1).is_none() {
                #[cfg(feature = "diagnostics")]
                {
                    let built = m.dump.built.entry(at).or_insert((frame, frame, 0));
                    (built.1, built.2) = (frame, built.2 + 1);
                }
                let before = held(m);
                m.ready(at, BUILT);
                came[1] += held(m).saturating_sub(before);
            }
        }
        if packed || !named(2, at) {
            continue;
        }
        let before = held(m);
        match crate::shadow::in_buffer(at, 1) {
            Some(_) => {
                m.ready(at, 1);
                m.named_in_upload(at);
                came[3] += held(m).saturating_sub(before);
            }
            None => {
                m.ready(at, if builds { VIEW } else { PAGE });
                came[2] += held(m).saturating_sub(before);
            }
        }
    }
    for (sum, n) in m.stats.came.iter_mut().zip(came) {
        *sum += n;
    }
    let base = m.descriptors(heap)?;
    // Every copy by every list that has come to by-address memory, whoever
    // numbered it: an arena another list made while this one was being
    // recorded was brought up to date by that list's submission alone, which
    // may run after this one's (structures unpacked from zeros, and then
    // written over by the arena's first upload: in frames that stream much in
    // half of them, run light-8).
    let first_read = list.first_use(READ);
    if list.first_use(usize::MAX - (6 << 33) - m.copies.len()) {
        m.bring_up(list);
    }
    if first_read {
        list.keep(&m.faults.0);
        #[cfg(feature = "diagnostics")]
        m.dump(list, addresses);
    }
    #[cfg(feature = "diagnostics")]
    if use_ == Use::Reads {
        m.dump.walked(addresses);
    }
    let directory = &m.directory.1;
    Some(Ready {
        directory: directory.gpu(directory.span().0),
        faults: m.faults.2,
        base: heap.physical(base),
    })
}

/// `VICTORY_BVH_TRACE=1`: the sizes the builder is asked for (its program of
/// one thread that stores a structure's compacted size) and when title memory
/// has them, and the copies to a place of that size.
#[cfg(feature = "diagnostics")]
mod trace {
    use super::*;

    #[derive(Default)]
    pub(super) struct Trace {
        /// Frame, where the size goes, the structure, what title memory had.
        asked: Vec<(u64, u64, u64, u64)>,
        /// By structure, what its size was when last seen to change.
        sizes: HashMap<u64, (u64, u64)>,
        said: u32,
    }

    fn value(at: u64) -> u64 {
        let mut b = [0u8; 8];
        crate::host::copy_mapped(b.as_mut_ptr(), at as *const u8, 8, false);
        u64::from_le_bytes(b)
    }

    impl Trace {
        fn say(&mut self, text: String) {
            self.said += 1;
            if self.said < 6000 {
                tracing::info!("bvh trace: {text}");
            }
        }

        pub(super) fn look(&mut self, frame: u64) {
            if !trace_enabled() {
                return;
            }
            let mut gone = Vec::new();
            for i in 0..self.asked.len() {
                let (then, at, structure, was) = self.asked[i];
                let now = value(at);
                if now != was {
                    self.sizes.insert(structure, (now, frame));
                    gone.push(i);
                    self.say(format!("frame {frame}: size of {structure:#x} at {at:#x} asked in {then}: {was:#x} -> {now:#x}"));
                } else if frame > then + 60 {
                    gone.push(i);
                    self.say(format!("frame {frame}: size of {structure:#x} at {at:#x} asked in {then}: still {was:#x}"));
                }
            }
            for i in gone.into_iter().rev() {
                self.asked.swap_remove(i);
            }
        }

        pub(super) fn dispatch(
            &mut self,
            frame: u64,
            list: &HostList,
            addresses: &[u64],
            written: &[u64],
            constants: &[u32],
            threads: Option<[u32; 3]>,
        ) {
            if !trace_enabled() {
                return;
            }
            let _ = addresses;
            let list = std::ptr::from_ref(list) as usize & 0xFFFF_FFFF;
            if threads == Some([1, 1, 1])
                && let [lo, hi, mode, ..] = *constants
                && (1..=3).contains(&mode)
                && hi < 0x20
                && let Some(&at) = written.first()
                && crate::shadow::in_buffer(at, 1).is_some_and(|b| b.1 <= BACK)
            {
                let structure = u64::from(lo) | u64::from(hi) << 32;
                let was = value(at);
                self.say(format!("frame {frame} list {list:x}: asks mode {mode} of {structure:#x} to {at:#x}, there {was:#x}"));
                if mode == 1 {
                    self.asked.push((frame, at, structure, was));
                }
            } else if threads == Some([32, 1, 1])
                && let [from, to] = *written
                && [from, to].iter().all(|&a| a >= 1 << 32 && crate::shadow::in_buffer(a, 1).is_none())
            {
                let known = self.sizes.get(&from).copied();
                self.say(format!("frame {frame} list {list:x}: copies {from:#x} to {to:#x} constants {:x?}, its size was {known:x?}", &constants[..constants.len().min(4)]));
            }
        }
    }
}

/// `VICTORY_MEMORY_DUMP=<folder>`: when a file `go` appears there, the arenas
/// as the card holds them are copied out and written (`arena-<n>.bin`), with
/// `pages.txt` (title page, arena, offset) and the structures the walking
/// programs were given: to look at the structures offline.
#[cfg(feature = "diagnostics")]
mod dump {
    use super::*;

    #[derive(Default)]
    pub(super) struct Dump {
        walked: std::collections::BTreeSet<u64>,
        /// Views the builder was given to write outside buffers: the first
        /// and the last frame, how often.
        pub(super) built: std::collections::BTreeMap<u64, (u64, u64, u64)>,
        /// The frame it is written at, and the copies.
        due: Option<(u64, Vec<(ID3D12Resource, usize)>)>,
        done: bool,
    }

    impl Dump {
        pub(super) fn walked(&mut self, addresses: &[u64]) {
            if !self.done && self.walked.len() < 64 {
                self.walked.extend(addresses);
            }
        }
    }

    fn folder() -> Option<&'static std::path::PathBuf> {
        static FOLDER: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
        FOLDER.get_or_init(|| std::env::var_os("VICTORY_MEMORY_DUMP").map(Into::into)).as_ref()
    }

    fn turn(buffer: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) -> D3D12_RESOURCE_BARRIER {
        D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    // SAFETY: the same pointer, not counted; the barrier is
                    // used while the buffer lives.
                    pResource: unsafe { std::mem::transmute_copy(buffer) },
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: before,
                    StateAfter: after,
                }),
            },
        }
    }

    impl Memory {
        /// At a list's first use: the copies are recorded into it.
        pub(super) fn dump(&mut self, list: &HostList, _: &[u64]) {
            let Some(folder) = folder() else { return };
            if let Some((due, _)) = &self.dump.due {
                if self.frame < *due {
                    return;
                }
                let copies = self.dump.due.take().map(|d| d.1).unwrap_or_default();
                let mut text = String::new();
                for a in &self.dump.walked {
                    text += &format!("walked {a:#x}\n");
                }
                for (a, (first, last, n)) in &self.dump.built {
                    text += &format!("built {a:#x} {first} {last} {n}\n");
                }
                for (a, p) in &self.packed {
                    text += &format!("packed {a:#x} {} {}\n", p.len, p.frame);
                }
                text += &format!("frame {}\n", self.frame);
                for (page, held) in &self.pages {
                    let arena = self.arenas.iter().position(|a| a.0 == held.copy).unwrap_or(usize::MAX);
                    let at = held.at - self.arenas[arena].1;
                    text += &format!("{:#x} {arena} {at:#x}\n", page * PAGE);
                }
                let _ = std::fs::write(folder.join("pages.txt"), text);
                for (n, (_, data)) in copies.iter().enumerate() {
                    // SAFETY: a mapping of an arena's bytes; the copies ran
                    // frames ago.
                    let bytes = unsafe { std::slice::from_raw_parts(*data as *const u8, ARENA as usize) };
                    let _ = std::fs::write(folder.join(format!("arena-{n}.bin")), bytes);
                }
                tracing::info!(arenas = copies.len(), "title memory by address: the arenas are written out");
                return;
            }
            if self.dump.done || !folder.join("go").exists() {
                return;
            }
            self.dump.done = true;
            let Some(host) = host() else { return };
            let mut copies = Vec::new();
            for &(n, _) in &self.arenas {
                let buffer = &self.copies[n].buffer;
                let Some((to, data)) = host.shared_buffer(ARENA) else { break };
                // SAFETY: an open list; live buffers of those sizes. The list
                // has not used the arena yet.
                unsafe {
                    list.list.ResourceBarrier(&[turn(buffer, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                    list.list.CopyBufferRegion(&to, 0, buffer, 0, ARENA);
                    list.list.ResourceBarrier(&[turn(buffer, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COMMON)]);
                }
                list.keep(&to);
                copies.push((to, data as usize));
            }
            self.dump.due = Some((self.frame + 12, copies));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_reserves_the_exact_union_and_keeps_low_buffer_provenance() {
        let high = 1 << 36;
        let mut spans = vec![(high+64, 8), (high, 16), (high+4, 20), (high+24, 4),
            (high+64, 8), (1024, 8), (1028, 8)];
        merge_trace_spans(&mut spans);
        assert_eq!(spans, vec![(1024,8), (1028,8), (high,28), (high+64,8)]);
    }

    #[test]
    fn in_place_rehydration_retains_its_original_extents_for_later_kernels() {
        let mut words = [0u64; 8];
        words[0] = (3u64 << 32) | 0x0100_0006;
        let address = words.as_ptr() as u64;
        let declared = build_ranges(&[[address, address, crate::bvh::REHYDRATE]], 9).unwrap();
        words[0] &= !(1 << 24);
        assert_eq!(crate::bvh::rehydration_extents(&crate::bvh::read(address).unwrap(), true), None);
        assert_eq!(&*declared, &[
            [address, 256, (9 << 8) | crate::bvh::PACKED_INPUT],
            [address, 256, (9 << 8) | crate::bvh::OUTPUT],
        ]);
        std::hint::black_box(words);
    }

    #[test]
    fn a_piece_takes_what_changed() {
        let mut title = [7u8; PIECE];
        let mut ours = vec![0u8; PIECE];
        assert_eq!(merge(&title, &mut ours, true), PIECE / 4);
        assert_eq!(ours, title);
        assert_eq!(merge(&title, &mut ours, false), 0);
        // A dword of it.
        title[41] = 8;
        assert_eq!(merge(&title, &mut ours, false), 1);
        assert_eq!(ours, title);
    }

    #[test]
    fn a_packed_structure_is_known_by_its_header() {
        let header = |version: u32, nodes: u32| u64::from(version) | u64::from(nodes) << 32;
        assert_eq!(packed_len(header(0x8102_0006, 0x9a1a)), Some(0x9a1b * 64));
        // Unpacked, a top level, another version, no size, too big.
        assert_eq!(packed_len(header(0x8002_0006, 0x9a1a)), None);
        assert_eq!(packed_len(header(0x0000_0006, 5)), None);
        assert_eq!(packed_len(header(0x8102_0007, 5)), None);
        assert_eq!(packed_len(header(0x8102_0006, 0)), None);
        assert_eq!(packed_len(header(0x8102_0006, 1 << 21)), None);
    }
}
