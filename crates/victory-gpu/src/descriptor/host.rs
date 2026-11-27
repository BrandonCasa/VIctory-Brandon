//! Host descriptor heaps behind the title's shader-visible heaps (decision
//! 0007, step 3). Translated shaders index the heaps directly at the
//! title's descriptor positions, so each host heap keeps the title heap's
//! order: slot `i` is made from what the title wrote at slot `i`, when it
//! writes it (view creation and descriptor copies) and again when a draw
//! reads it through a table and finds the slot changed.

use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::{VIEW_TAG, View, view_at};
use crate::host::host;

/// A set of a heap's slots, a bit for each: no lock (a pass that went through
/// a set of 70,000 under its lock kept everyone who writes slots waiting for
/// 5 ms of a frame, timeline thin-6).
struct Slots(Vec<std::sync::atomic::AtomicU64>);

// Record membership once at publication. Each draw shares immutable sorted
// slices, including the physical range proof of previously classified SRVs.
// A rewrite of one title slot invalidates just its contiguous heap shard.
#[derive(Clone, Default)]
struct IndexedCandidate {
    slot: u32,
    record: u64,
    form: u8,
    texture: u8,
    texture_kind: u32,
    buffer: u8,
    pending: bool,
    plain: Option<PlainMaterial>,
}

#[derive(Default)]
struct CandidateShard {
    rows: std::collections::BTreeMap<u32, IndexedCandidate>,
    snapshot: std::sync::OnceLock<std::sync::Arc<[IndexedCandidate]>>,
    without_sampled: std::sync::OnceLock<std::sync::Arc<[IndexedCandidate]>>,
}

struct CandidateIndex {
    width: u32,
    shards: Box<[std::sync::RwLock<CandidateShard>]>,
}

/// The retired descriptor generation of each slot. Most parts of a sparse
/// title heap never contain a retired placement, so allocate a page only
/// when its first generation is retired. Writers run under the span lock.
struct RetiredRecords(Box<[std::sync::OnceLock<Box<[std::sync::atomic::AtomicU64]>>]>);

impl RetiredRecords {
    const SLOTS: usize = 256;

    fn new(count: u32) -> Self {
        Self((0..(count as usize).div_ceil(Self::SLOTS)).map(|_| Default::default()).collect())
    }

    fn get(&self, slot: u32) -> u64 {
        let slot = slot as usize;
        self.0.get(slot / Self::SLOTS).and_then(std::sync::OnceLock::get)
            .map_or(0, |page| page[slot % Self::SLOTS].load(std::sync::atomic::Ordering::Acquire))
    }

    fn set(&self, slot: u32, key: u64) {
        let slot = slot as usize;
        let Some(entry) = self.0.get(slot / Self::SLOTS) else { return };
        let page = if key == 0 {
            let Some(page) = entry.get() else { return };
            page
        } else { entry.get_or_init(|| (0..Self::SLOTS).map(|_| Default::default()).collect()) };
        page[slot % Self::SLOTS].store(key, std::sync::atomic::Ordering::Release);
    }

    fn may(&self, slot: u32) -> bool { self.get(slot) != 0 }
}

impl CandidateIndex {
    fn new(count: u32) -> Self {
        let width = count.div_ceil(256).max(256);
        Self { width, shards: (0..count.div_ceil(width)).map(|_| Default::default()).collect() }
    }

    fn update(&self, slot: u32, update: impl FnOnce(&mut IndexedCandidate) -> bool) {
        let Some(shard) = self.shards.get((slot / self.width) as usize) else { return };
        let mut shard = shard.write().unwrap_or_else(|e| e.into_inner());
        let active = |r: &IndexedCandidate| r.texture != 0 || r.buffer != 0 || r.pending;
        let changed = match shard.rows.entry(slot) {
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let changed = update(entry.get_mut());
                if !active(entry.get()) { entry.remove(); }
                changed
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                let mut row = IndexedCandidate { slot, ..Default::default() };
                update(&mut row);
                if active(&row) { entry.insert(row); true } else { false }
            }
        };
        if changed {
            shard.snapshot.take();
            shard.without_sampled.take();
        }
    }

    fn pending(&self, slot: u32, value: bool) {
        self.update(slot, |row| std::mem::replace(&mut row.pending, value) != value);
    }

    fn buffer(&self, slot: u32, form: u8) {
        self.update(slot, |row| std::mem::replace(&mut row.buffer, form) != form);
    }

    fn observed(&self, slot: u32, record: u64, form: u8, texture_kind: u32, buffer: Option<u8>) {
        self.update(slot, |row| {
            let texture = form & (8 | 16);
            let buffer = buffer.unwrap_or(row.buffer);
            let outdated = row.plain.as_ref().is_some_and(|p| p.record != record);
            if outdated { row.plain = None; }
            let changed = (row.record, row.form, row.texture, row.texture_kind, row.buffer) != (record, form, texture, texture_kind, buffer);
            (row.record, row.form, row.texture, row.texture_kind, row.buffer) = (record, form, texture, texture_kind, buffer);
            changed || outdated
        });
    }

    fn mark_plain(&self, slot: u32, proof: &PlainMaterial) {
        self.update(slot, |row| {
            if row.texture != 8 || row.plain.as_ref().is_some_and(|p| p.record == proof.record && p.topology == proof.topology) { return false; }
            row.plain = Some(proof.clone());
            true
        });
    }

    fn known_plain(&self, slot: u32, record: u64, topology: u64) -> Option<PlainMaterial> {
        let shard = self.shards.get((slot / self.width) as usize)?.read().unwrap_or_else(|e| e.into_inner());
        shard.rows.get(&slot)?.plain.as_ref()
            .filter(|proof| proof.record == record && proof.topology == topology).cloned()
    }

    fn disjoint_buffer_at(&self, slot: u32, pass: &CandidatePass) -> bool {
        let Some(shard) = self.shards.get((slot / self.width) as usize) else { return false; };
        let shard = shard.read().unwrap_or_else(|e| e.into_inner());
        shard.rows.get(&slot).is_none_or(|row| {
            !row.pending && row.texture == 0 && pass.disjoint_buffer_form(row.form)
                && (row.buffer == 0 || pass.disjoint_buffer_form(row.buffer))
        })
    }

    fn sampled_record_at(&self, slot: u32, record: u64, kind: u32) -> bool {
        let Some(shard) = self.shards.get((slot / self.width) as usize) else { return false; };
        let shard = shard.read().unwrap_or_else(|e| e.into_inner());
        shard.rows.get(&slot).is_some_and(|row|
            row.record == record && row.form == 8 && row.texture == 8
                && row.texture_kind == kind && row.buffer == 0 && !row.pending)
    }
}

impl CandidateShard {
    fn snapshot(&self) -> std::sync::Arc<[IndexedCandidate]> {
        self.snapshot.get_or_init(|| self.rows.values().cloned().collect()).clone()
    }

    fn without_sampled(&self) -> std::sync::Arc<[IndexedCandidate]> {
        self.without_sampled.get_or_init(|| self.rows.values().filter(|r|
            r.texture != 8 || r.buffer != 0 || r.pending).cloned().collect()).clone()
    }
}

impl Slots {
    fn new(count: usize) -> Self {
        let mut bits = Vec::new();
        bits.resize_with(count.div_ceil(64), Default::default);
        Slots(bits)
    }

    fn put(&self, slot: u32) {
        if let Some(bits) = self.0.get(slot as usize / 64) {
            bits.fetch_or(1 << (slot % 64), std::sync::atomic::Ordering::AcqRel);
        }
    }

    /// Whether it was there.
    fn remove(&self, slot: u32) -> bool {
        use std::sync::atomic::Ordering::{AcqRel, Acquire};
        let bit = 1 << (slot % 64);
        self.0.get(slot as usize / 64).is_some_and(|bits| {
            bits.load(Acquire) & bit != 0 && bits.fetch_and(!bit, AcqRel) & bit != 0
        })
    }

    fn of(n: usize, mut bits: u64, out: &mut Vec<u32>) {
        while bits != 0 {
            out.push(n as u32 * 64 + bits.trailing_zeros());
            bits &= bits - 1;
        }
    }

    /// The set slots selected by the same mask in each 64-slot word, in order.
    fn all(&self, mask: u64) -> Vec<u32> {
        let mut out = Vec::new();
        for (n, bits) in self.0.iter().enumerate() {
            Self::of(n, bits.load(std::sync::atomic::Ordering::Acquire) & mask, &mut out);
        }
        out
    }

    /// One part of a round-robin scan. The pattern repeats at every word boundary.
    fn part<const PARTS: u32>(&self, part: u32) -> Vec<u32> {
        const { assert!(PARTS > 0 && u64::BITS % PARTS == 0) };
        if part >= PARTS {
            return Vec::new();
        }
        let mask = (part..u64::BITS)
            .step_by(PARTS as usize)
            .fold(0, |mask, bit| mask | (1 << bit));
        self.all(mask)
    }

    /// All of them, and none is left.
    fn take(&self) -> Vec<u32> {
        use std::sync::atomic::Ordering::{AcqRel, Acquire};
        let mut out = Vec::new();
        for (n, bits) in self.0.iter().enumerate() {
            if bits.load(Acquire) != 0 {
                Self::of(n, bits.swap(0, AcqRel), &mut out);
            }
        }
        out
    }
}

/// The record each slot of a heap was last made from, and a lock for every
/// few thousandth of the slots: whoever makes a slot holds its lock, and no
/// other.
///
/// One lock for the heap was what everything of a frame waited for once the
/// lists were translated by threads of ours: half of what those did, two
/// thirds of the heaps' records and half of the pass over the buffer views
/// (profile thin-2).
struct Made {
    keys: Vec<std::sync::atomic::AtomicU64>,
    locks: Box<[Mutex<()>]>,
}

impl Made {
    const LOCKS: usize = 1 << 14;

    fn new(count: usize) -> Self {
        let mut keys = Vec::new();
        keys.resize_with(count, Default::default);
        Made { keys, locks: (0..Self::LOCKS).map(|_| Mutex::new(())).collect() }
    }

    /// The lock slot `index` is made under. Neighbours have different ones,
    /// and so have the slots that threads which go through parts of a heap in
    /// step are at (with one of 512 by the slot's lowest bits, they stood 2 to
    /// 4 ms a frame of each in each other's way, run b5-small).
    fn lock(&self, index: u32) -> std::sync::MutexGuard<'_, ()> {
        let lock = index.wrapping_mul(0x9E37_79B1) as usize >> (32 - 14);
        self.locks[lock].lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get(&self, index: u32) -> Option<u64> {
        Some(self.keys.get(index as usize)?.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Under the slot's lock.
    fn set(&self, index: u32, key: u64) {
        if let Some(k) = self.keys.get(index as usize) {
            k.store(key, std::sync::atomic::Ordering::Release);
        }
    }
}

#[derive(Clone, Copy)]
enum PublicationCause {
    Eviction,
    NeutralView,
    TextureFlip,
    TextureRemake,
    Unmake,
    EmptyCleanup,
    ClearView,
    MirrorView,
    #[cfg(feature = "diagnostics")]
    EmptyMirrorSkipped,
    #[cfg(feature = "diagnostics")]
    UnmappedFlipSkipped,
}

struct MirrorPublication<'a> {
    slot: u32,
    shown: u64,
    handle: u64,
    key: u64,
    view: &'a View,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MirrorReason {
    Buffer,
    SampledSelection,
    SampledDimension,
    Requested,
    ChangedRecord,
    SpanOrUnmapped,
    UnknownOld,
    CandidateChanged,
    CompatibleOrUnknown,
    Pending,
    JournalFull,
}

impl MirrorReason {
    fn sampled(self) -> bool { matches!(self, Self::SampledSelection | Self::SampledDimension) }
    fn disjoint(self) -> bool { self == Self::Buffer || self.sampled() }
}

struct MirrorImpact {
    slot: u32,
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    old: u8,
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    new: u8,
    reason: MirrorReason,
}

struct MirrorNote {
    pass: std::sync::Arc<CandidatePass>,
    impact: MirrorImpact,
    handle: u64,
    key: u64,
    installed: bool,
}

impl Drop for MirrorNote {
    fn drop(&mut self) {
        if self.impact.reason.sampled() {
            if !self.installed {
                self.impact.reason = MirrorReason::Pending;
            } else if !self.pass.note_sampled_mirror(self.impact.slot, self.handle, self.key) {
                self.impact.reason = MirrorReason::JournalFull;
            }
        }
        self.pass.note_mirror(&self.impact);
    }
}

struct MirrorGuard<'a> {
    // The note retires before the shared publication pin, including early returns.
    note: Option<MirrorNote>,
    _pin: std::sync::RwLockReadGuard<'a, u64>,
}

#[cfg(feature = "diagnostics")]
thread_local! {
    static IN_INDEXED_PREPARATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(feature = "diagnostics")]
#[derive(Default)]
struct PublicationStats {
    cause: [std::sync::atomic::AtomicU64; PublicationCause::UnmappedFlipSkipped as usize + 1],
    indexed: std::sync::atomic::AtomicU64,
    reported_second: std::sync::atomic::AtomicU64,
}

#[cfg(feature = "diagnostics")]
impl PublicationStats {
    fn note(&self, cause: PublicationCause) {
        use std::sync::atomic::Ordering::Relaxed;
        if !crate::host::gpu_progress() { return; }
        self.cause[cause as usize].fetch_add(1, Relaxed);
        if !matches!(cause, PublicationCause::EmptyMirrorSkipped | PublicationCause::UnmappedFlipSkipped)
            && IN_INDEXED_PREPARATION.get()
        {
            self.indexed.fetch_add(1, Relaxed);
        }
    }

    fn report_due(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        if !crate::host::gpu_progress() { return false; }
        let second = crate::host::now_ms() / 1000;
        let previous = self.reported_second.load(Relaxed);
        second > previous && self.reported_second.compare_exchange(previous, second, Relaxed, Relaxed).is_ok()
    }

    fn report(heap: &HostHeap, version: u64, counts: [u64; PublicationCause::UnmappedFlipSkipped as usize + 1], indexed: u64) {
        use PublicationCause::*;
        let count = |cause: PublicationCause| counts[cause as usize];
        tracing::info!(heap = %format_args!("{heap:p}"), version, frame = crate::frame::current(),
            eviction = count(Eviction), neutral_view = count(NeutralView), texture_flip = count(TextureFlip),
            texture_remake = count(TextureRemake), unmake = count(Unmake), empty_cleanup = count(EmptyCleanup), clear_view = count(ClearView),
            mirror_view = count(MirrorView), from_indexed_preparation = indexed,
            empty_mirror_skipped = count(EmptyMirrorSkipped), unmapped_flip_skipped = count(UnmappedFlipSkipped),
            "indexed heap publication causes");
    }
}

#[derive(Default)]
struct CandidateObservations {
    active: Mutex<Option<std::sync::Arc<CandidatePass>>>,
    watching: std::sync::atomic::AtomicBool,
}

struct CandidatePass {
    opaque: u8,
    requested: Box<[u32]>,
    sampled_complete: bool,
    sampled_kinds: u32,
    sampled_mirrors: Mutex<Vec<(u32, u64, u64)>>,
    topology: u64,
    lifetime: u64,
    mapping: u64,
    affected: std::sync::atomic::AtomicBool,
    disjoint: std::sync::atomic::AtomicU64,
    #[cfg(feature = "diagnostics")]
    impacts: Option<Mutex<[(u64, u32, u8, u8); MirrorReason::JournalFull as usize + 1]>>,
}

impl CandidatePass {
    fn new(slots: &IndexedSlots, selected: Option<&std::collections::BTreeSet<u32>>) -> Option<Self> {
        const MAX_REQUESTED: usize = 8192;
        if slots.opaque == 0 || slots.opaque & 32 != 0 || !slots.buffer_writes.is_empty()
            || slots.exact.len() + selected.map_or(0, std::collections::BTreeSet::len) > MAX_REQUESTED
        { return None; }
        let mut requested: Vec<u32> = slots.exact.keys().copied()
            .chain(selected.into_iter().flatten().copied()).collect();
        requested.sort_unstable();
        requested.dedup();
        Some(Self { opaque: slots.opaque, requested: requested.into_boxed_slice(),
            sampled_complete: selected.is_some(), sampled_kinds: slots.texture_kinds[0],
            sampled_mirrors: Default::default(),
            topology: crate::resource::alias_generation(), lifetime: super::record_lifetime(0), mapping: crate::watch::changes(),
            affected: Default::default(), disjoint: Default::default(),
            #[cfg(feature = "diagnostics")]
            impacts: crate::host::gpu_progress().then(Default::default),
        })
    }

    fn may_ignore_slot(&self, slot: u32) -> bool { self.requested.binary_search(&slot).is_err() }

    fn disjoint_buffer_form(&self, form: u8) -> bool {
        matches!(form, 1 | 2 | 4) && self.opaque & form == 0
            && (form != 4 || self.opaque & victory_shader::emit::BUFFER_WRITE_ACCESS == 0)
    }

    fn sampled_reason(&self, slot: u32, old: u32, new: u32) -> MirrorReason {
        if !self.may_ignore_slot(slot) { return MirrorReason::Requested; }
        if self.opaque & 8 == 0 || old == 0 || new == 0 { return MirrorReason::CompatibleOrUnknown; }
        if self.sampled_complete { return MirrorReason::SampledSelection; }
        if self.sampled_kinds != 0 && self.sampled_kinds & (old | new) == 0 {
            return MirrorReason::SampledDimension;
        }
        MirrorReason::CompatibleOrUnknown
    }

    fn note_mirror(&self, impact: &MirrorImpact) {
        self.note(impact.reason.disjoint());
        #[cfg(feature = "diagnostics")]
        if let Some(impacts) = &self.impacts {
            let mut impacts = impacts.lock().unwrap_or_else(|e| e.into_inner());
            let entry = &mut impacts[impact.reason as usize];
            if entry.0 == 0 { *entry = (0, impact.slot, impact.old, impact.new); }
            entry.0 += 1;
        }
    }

    fn note_sampled_mirror(&self, slot: u32, handle: u64, key: u64) -> bool {
        const MAX_SAMPLED_MIRRORS: usize = 64;
        let mut mirrors = self.sampled_mirrors.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(previous) = mirrors.iter_mut().find(|(old, _, _)| *old == slot) {
            *previous = (slot, handle, key);
        } else if mirrors.len() < MAX_SAMPLED_MIRRORS {
            mirrors.push((slot, handle, key));
        } else {
            return false;
        }
        true
    }

    fn note(&self, disjoint: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        if disjoint {
            self.disjoint.fetch_add(1, Relaxed);
        } else {
            self.affected.store(true, Relaxed);
        }
    }

    fn accounts_for(&self, changes: u64) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        changes != 0 && self.disjoint.load(Relaxed) == changes && !self.affected.load(Relaxed)
    }

    fn sampled_mirrors_current(&self, current: impl Fn(u32) -> Option<(u64, u64)>) -> bool {
        self.sampled_mirrors.lock().unwrap_or_else(|e| e.into_inner()).iter()
            .all(|&(slot, handle, key)| current(slot) == Some((handle, key)) && record_key(handle) == key)
    }

    fn only_disjoint(&self, changes: u64, heap: &HostHeap, title: Option<&super::Heap>) -> bool {
        let title = title.filter(|title| title.host.get().and_then(Option::as_ref)
            .is_some_and(|owner| std::ptr::eq(std::sync::Arc::as_ptr(owner), heap)));
        self.accounts_for(changes) && self.topology == crate::resource::alias_generation()
            && self.mapping == crate::watch::changes() && self.lifetime == super::record_lifetime(0)
            && self.sampled_mirrors_current(|slot| Some((title?.handle(slot)?, heap.made.get(slot)?)))
    }
}

/// A single candidate pass watches publications while preparing the heap.
/// Starting, finishing and unwinding all exclude the publishers' shared pins.
pub(crate) struct CandidateObservation<'a> {
    heap: &'a HostHeap,
    before: u64,
    pass: Option<std::sync::Arc<CandidatePass>>,
}

impl CandidateObservation<'_> {
    pub(crate) fn before(&self) -> u64 { self.before }

    pub(crate) fn finish(mut self, title: Option<usize>) -> (u64, bool) {
        if let Some(title) = title
            && self.pass.as_ref().is_some_and(|pass| pass.opaque & 8 != 0)
            && let Some(result) = super::with_heap(title, |title| self.finish_pinned(Some(title)))
        {
            return result;
        }
        self.finish_pinned(None)
    }

    fn finish_pinned(&mut self, title: Option<&super::Heap>) -> (u64, bool) {
        let pass = self.pass.take();
        let (after, only_disjoint) = self.heap.stable_publication(|after| {
            let pass = pass.as_ref()?;
            self.heap.stop_candidate_observation(pass);
            Some(pass.only_disjoint(after.wrapping_sub(self.before), self.heap, title))
        });
        #[cfg(feature = "diagnostics")]
        if let Some(pass) = &pass && let Some(impacts) = &pass.impacts {
            use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
            let impacts = *impacts.lock().unwrap_or_else(|e| e.into_inner());
            if impacts.iter().any(|impact| impact.0 != 0) {
                static PASSES: AtomicU64 = AtomicU64::new(0);
                static SECOND: AtomicU64 = AtomicU64::new(0);
                let passes = PASSES.fetch_add(1, Relaxed) + 1;
                let second = crate::host::now_ms() / 1000;
                if passes.is_power_of_two() || SECOND.fetch_max(second, Relaxed) < second {
                    use MirrorReason::*;
                    let reasons = [Buffer, SampledSelection, SampledDimension, Requested, ChangedRecord,
                        SpanOrUnmapped, UnknownOld, CandidateChanged, CompatibleOrUnknown, Pending, JournalFull];
                    let examples: Vec<_> = reasons.into_iter().zip(impacts).filter(|(_, impact)| impact.0 != 0).collect();
                    tracing::info!(passes, before=self.before, after, disjoint=pass.disjoint.load(Relaxed),
                        only_disjoint=?only_disjoint, opaque=pass.opaque, sampled_complete=pass.sampled_complete,
                        sampled_kinds=pass.sampled_kinds, frame=crate::frame::current(), ?examples,
                        "indexed mirror publication impacts (reason, count, logical, old form, new form)");
                }
            }
        }
        (after, only_disjoint == Some(true))
    }
}

impl Drop for CandidateObservation<'_> {
    fn drop(&mut self) {
        if let Some(pass) = self.pass.take() {
            let _pin = self.heap.last_reader.write().unwrap_or_else(|e| e.into_inner());
            self.heap.stop_candidate_observation(&pass);
        }
    }
}

/// Slots known from root state, plus access forms that remain lane- or memory-dependent.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct IndexedSlots {
    pub opaque: u8,
    pub exact: std::collections::BTreeMap<u32, u8>,
    pub texture_kinds: [u32; 2],
    pub words: Vec<super::index_words::SampleWords>,
    pub words_only: bool,
    pub uav_words: Vec<super::index_words::SampleWords>,
    pub uav_words_only: bool,
    pub buffer_writes: Vec<crate::host::BufferWriteSelection>,
}

impl IndexedSlots {
    pub(crate) fn empty(&self) -> bool {
        self.opaque == 0 && self.exact.is_empty() && self.buffer_writes.is_empty()
    }

    pub(crate) fn merge(&mut self, other: &Self) {
        for proof in &other.buffer_writes {
            if !self.buffer_writes.contains(proof) { self.buffer_writes.push(proof.clone()); }
        }
        let bounded_uavs = (self.opaque & 16 == 0 || self.uav_words_only)
            && (other.opaque & 16 == 0 || other.uav_words_only);
        self.uav_words_only = (self.opaque | other.opaque) & 16 != 0 && bounded_uavs;
        if self.uav_words_only {
            for input in &other.uav_words {
                if !self.uav_words.contains(input) { self.uav_words.push(input.clone()); }
            }
        } else { self.uav_words.clear(); }
        let can_narrow = (self.opaque & 8 == 0 || self.words_only)
            && (other.opaque & 8 == 0 || other.words_only);
        self.words_only = (self.opaque | other.opaque) & 8 != 0 && can_narrow;
        if self.words_only {
            for input in &other.words {
                if !self.words.contains(input) { self.words.push(input.clone()); }
            }
        } else {
            self.words.clear();
        }
        self.merge_forms(other);
    }

    fn merge_forms(&mut self, other: &Self) {
        for (i, form) in [8, 16].into_iter().enumerate() {
            if other.opaque & form != 0 {
                self.texture_kinds[i] = if self.opaque & form == 0 {
                    other.texture_kinds[i]
                } else if self.texture_kinds[i] == 0 || other.texture_kinds[i] == 0 {
                    0
                } else {
                    self.texture_kinds[i] | other.texture_kinds[i]
                };
            }
        }
        self.opaque |= other.opaque;
        for (&slot, &form) in &other.exact {
            *self.exact.entry(slot).or_default() |= form;
        }
    }

    fn accepts_opaque_texture(&self, view: &View) -> bool {
        let Some((i, kind)) = texture_kind(view) else { return true; };
        self.texture_kinds[i] == 0 || self.texture_kinds[i] & kind != 0
    }
}

fn texture_kind(view: &View) -> Option<(usize,u32)> {
        // Explicit D3D12 SRV/UAV dimensions share these values for textures.
        // An implicit descriptor lets D3D12 infer its dimension from its resource.
        let (i, dimension) = match view {
            View::Shader(_, Some(d)) => (0, d[1]),
            View::Unordered(_, _, Some(d)) => (1, d[1]),
            View::Placed(unordered, _, d) => (usize::from(*unordered), d[1]),
            _ => return None,
        };
        let kind = match dimension {
            2 => 1, 3 => 6, 4 => 2, 5 => 7, 6 => 3, 7 => 8, 8 => 4, 9 => 5, 10 => 9,
            _ => return None,
        };
        Some((i, 1 << kind))
}

/// Buffer accesses that can run concurrently in one draw or dispatch.
/// Submission rechecks the same selections after late descriptor writes.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct BufferAliases {
    pub slots: IndexedSlots,
    pub root_writes: Vec<(u64, u64)>,
    pub unknown_writer: bool,
}

#[derive(Default)]
struct BufferSpans {
    slots: std::collections::BTreeMap<u32, (crate::memory::ViewSpan, u8)>,
    copied: std::collections::BTreeSet<u32>,
    writable: std::collections::BTreeSet<u32>,
}

impl BufferSpans {
    fn remove(&mut self, slot: &u32) {
        self.slots.remove(slot);
        self.copied.remove(slot);
        self.writable.remove(slot);
    }

    fn insert(&mut self, slot: u32, view: (crate::memory::ViewSpan, u8)) {
        // First materialization usually names the same guest span and role
        // already published when the title wrote this deferred descriptor.
        let old = self.slots.insert(slot, view).map_or(0, |(_, role)| role);
        if old == view.1 { return; }
        match old {
            2 => { self.copied.remove(&slot); }
            4 => { self.writable.remove(&slot); }
            _ => {}
        }
        match view.1 {
            2 => { self.copied.insert(slot); }
            4 => { self.writable.insert(slot); }
            _ => {}
        }
    }
}

impl BufferAliases {
    pub fn slot(&mut self, slot: u32, kind: Option<victory_shader::ResourceType>) {
        use victory_shader::ResourceType;
        let form = match kind {
            Some(ResourceType::Srv) => 2,
            Some(ResourceType::Uav) => victory_shader::emit::BUFFER_WRITE_ACCESS,
            None => 2 | victory_shader::emit::BUFFER_WRITE_ACCESS,
            _ => return,
        };
        *self.slots.exact.entry(slot).or_default() |= form;
    }

    pub fn needs_check(&self) -> bool {
        let forms = self.slots.exact.values().fold(self.slots.opaque, |a, b| a | b);
        forms & 2 != 0 && (forms & victory_shader::emit::BUFFER_WRITE_ACCESS != 0
            || self.unknown_writer || !self.root_writes.is_empty())
    }
}

pub(crate) struct HostHeap {
    flip_identity: std::sync::Arc<()>,
    retired_runs: Mutex<Vec<(u64, u32, u32)>>,
    has_retired_runs: std::sync::atomic::AtomicBool,
    changes_seen: [std::sync::atomic::AtomicU64; 2],
    /// Fence of the most recent native submission observing the heap. CPU
    /// publishers take the shared side, submit holds the exclusive side
    /// until both ExecuteCommandLists and its signal have been enqueued.
    pub(crate) last_reader: std::sync::RwLock<u64>,
    publication: std::sync::atomic::AtomicU64,
    candidate_observations: CandidateObservations,
    #[cfg(feature = "diagnostics")]
    publication_stats: PublicationStats,
    pub(crate) buffer_sweep: Mutex<crate::draw::BufferSweep>,
    pub(crate) buffer_frame: std::sync::atomic::AtomicU64,
    pub(crate) heap: ID3D12DescriptorHeap,
    cpu: usize,
    step: u32,
    /// Slots mirroring the title's; view heaps have `dynamic` more.
    count: u32,
    /// Dynamic slots past `count`: [`DYNAMIC`] in view heaps, none in
    /// sampler heaps. (Upstream falls back to 2^16 on a capped host heap;
    /// ours keeps 2^18 there: 2^16 and 2^17 both ran out in this title.)
    dynamic: u32,
    /// Physical beginning of the contiguous descriptor space reserved for lists.
    scratch_base: u32,
    /// Present when sparse logical title slots share a smaller physical arena.
    assigned: Vec<std::sync::atomic::AtomicU32>,
    next_title: std::sync::atomic::AtomicU32,
    free_titles: Mutex<Vec<u32>>,
    title_readers: Mutex<std::collections::HashMap<u64, IndexedSlots>>,
    evict_cursor: std::sync::atomic::AtomicU32,
    failed_buffers: Queued,
    candidates: CandidateIndex,
    // The title can stream out a placement while leaving its inline descriptor
    // in an older slot. A later title write reactivates the slot.
    superseded: RetiredRecords,
    // Span and alias role (2: separate read presentation, 4: writable buffer).
    indexed_addresses: Mutex<BufferSpans>,
    ordered_reads:
        Mutex<std::collections::HashMap<u32, (std::sync::Arc<crate::shadow::Shadow>, u64, u64)>>,
    sampler: bool,
    /// Where the raw twins of the title's slots start
    /// (`victory_shader::emit::raw_views`; 0: there are none), and which of
    /// them are not null.
    raw: u32,
    twins: Vec<std::sync::atomic::AtomicBool>,
    /// When each slot was written last (with `crate::book`; else empty).
    #[cfg(feature = "diagnostics")]
    written: Vec<std::sync::atomic::AtomicU32>,
    /// The record (view index and generation) each slot was last made from.
    made: Made,
    /// The same for a slot whose descriptor is made and stays (else 0): what
    /// [`HostHeap::mirror`] asks first, without the lock. Changed under it.
    quick: Vec<std::sync::atomic::AtomicU64>,
    /// The records of the title's heap as the pass over records copied in
    /// as bytes last saw them ([`Copied::changes`]). Made on first
    /// use.
    copied: Mutex<Vec<u8>>,
    /// New ones of them that wait for a draw ([`HostHeap::copied_wait`]),
    /// by the frame they were found in.
    waiting: Mutex<std::collections::VecDeque<(u64, Vec<u32>)>>,
    /// The slots made from a texture whose memory was found away, with what
    /// they were made from and when that was
    /// ([`HostHeap::texture_slot_gone`]).
    away: Mutex<std::collections::HashMap<u32, (u64, u64)>>,
    /// The host resource each slot views, held while the slot does: shaders
    /// index heaps directly, so a slot the title has not rewritten since it
    /// released the resource may still be read, and a freed host resource
    /// read there is a GPU page fault (the console reads stale memory).
    ///
    /// A lock for each slot: one for the heap was what the recording threads
    /// waited for most (5.6 % of their time, profile waits-1: each descriptor
    /// made for a draw takes it, and lets go of what the slot held under it).
    kept: Box<[Mutex<Option<ID3D12Resource>>]>,
    /// Where the search for free dynamic slots starts, and which are taken
    /// (by lists not yet reset).
    next: Mutex<(u32, Vec<bool>)>,
    /// How many dynamic slots are free (changed under `next`'s lock): with
    /// fewer than a table needs, the search is not made. Once the world
    /// had taken them all, every later table of the frame searched every
    /// slot before it was refused.
    free: std::sync::atomic::AtomicU32,
    /// The shortest table that found no run of free slots since slots were
    /// last given back (changed under `next`'s lock; `u32::MAX`: none).
    /// Until something is given back the free runs only shrink, so a table
    /// of that length or longer is refused without the search. Enough
    /// slots were free, but in pieces, and every such table walked the
    /// whole ring before it was refused: 0.33 ms each with 131,072 slots,
    /// 1.0 s of the recording threads in a frame that refused 3,143 (run
    /// 45), under the lock all of them need. (A free run that lies across
    /// the place the search starts from is seen by neither.)
    too_long: std::sync::atomic::AtomicU32,
    /// View heaps: the bytes per element of the buffer view at each title
    /// slot, for shaders that pick a buffer by an index they compute
    /// (`victory_shader::emit::SLOT_STRIDES`): an upload buffer of
    /// `SLOT_STRIDES_COUNT` dwords, its GPU address and its mapping.
    strides: Option<(ID3D12Resource, u64, usize)>,
    /// Title slots holding buffer views. Such a view gets its host slot
    /// (and its memory a host copy) only when something reads it: a draw
    /// through a table, or [`HostHeap::buffer_slots`] for shaders that pick
    /// buffers from the whole heap.
    buffers: Slots,
    /// Of those and of the slots that left the set: the ones written since
    /// [`HostHeap::buffer_changes`] last took them.
    changed: Slots,
    /// Title slots holding a view of a texture the title may fill itself (a
    /// placed texture, a texture object), with the frame from which they
    /// are due: such a texture is uploaded when a draw reads it through a
    /// table; one a shader picks by a computed index is brought up to date
    /// from here ([`HostHeap::texture_slots_due`]).
    textures: Mutex<TextureQueue>,
    /// Its slots ([`Queued`]).
    queued: std::sync::Arc<Queued>,
    /// Per title slot, the frame (plus one; 0: never) in which a draw last
    /// read it through a table, or made a table it lies in (with
    /// [`TABLED_READ`] set if the draw read it). The draw that
    /// reads such a slot brings its texture up to date itself and the slot
    /// then leaves `textures`; a write of a slot that was a table's within
    /// the last [`TABLED_FRAMES`] frames is due only [`TABLED_GRACE`]
    /// frames later, by when its draw has read it. A title that builds a
    /// table per draw writes thousands of texture views a frame into slots
    /// only tables read (the title: some 2,500 a frame in its
    /// opening cutscene, a few hundred textures over and over), and every
    /// one of them stood in the queue of textures picked by index, ahead
    /// of or behind the ones shaders do pick by index.
    tabled: Vec<std::sync::atomic::AtomicU32>,
    /// The title memory each placed texture view of this heap describes,
    /// by slot and by address ([`Spans`]): which slots a newer view of
    /// other memory there makes stale, and which to look at again when the
    /// runtime maps, unmaps or writes memory ([`remapped`]).
    placed: std::sync::Arc<Mutex<Spans>>,
    /// The slots that may be in `placed` (a slot noted there is put here
    /// under its lock, and taken out when it is forgotten there; a slot
    /// another's view made stale stays, which says only "may"): a write of
    /// any other view needs nothing of `placed` ([`spans_quick`]).
    spanned: Slots,
}

/// Whether a slot's write of no placed texture view leaves `placed` alone
/// when the slot is not in it ([`HostHeap::spanned`]; `VICTORY_SPANS_QUICK=0`:
/// its lock is taken for every write, as before).
///
/// Every record the pass over records copied as bytes mirrors went by that
/// one lock of the heap, from six threads, with the title's own threads and
/// the texture passes: 4 of the 16 ms a frame the submitting thread waited
/// for that pass were its threads waiting there (run city4, timeline 113).
/// What is done is the same: forgetting a slot that is not there does
/// nothing.
///
/// Run heapsync-ab3 (A/B in the street): 13,300 of 13,500 such writes a
/// frame left it alone; making views 1.7 ms a frame against 2.4, the pass
/// 11.7-12.0 ms against 13.2-13.8; the picture the same.
/// Since the last frame line: writes that left `placed` alone
/// ([`spans_quick`]), writes that took its lock.
pub(crate) static SPANS_QUICK: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

fn spans_quick() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_SPANS_QUICK") {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SPANS_QUICK").is_none_or(|v| v != "0"))
}

/// The spans of every view heap made ([`HostHeap::placed`]), for the
/// texture code, which has no heap at hand ([`next_start`],
/// [`first_viewed`]).
static PLACED_SPANS: Mutex<Vec<std::sync::Weak<Mutex<Spans>>>> = Mutex::new(Vec::new());

/// Where the next texture placed inside `[start, end)` starts, past the
/// first bytes of the one at `start` ([`HEAD`]): what is there of the
/// texture at `start` ends there, whatever its full size. `None`: no view
/// in a slot shaders pick by index describes memory in that range.
pub(crate) fn next_start(start: u64, end: u64) -> Option<u64> {
    let heaps = PLACED_SPANS.lock().unwrap_or_else(|e| e.into_inner());
    heaps
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .filter_map(|spans| spans.lock().unwrap_or_else(|e| e.into_inner()).next_start(start, end))
        .min()
}

/// The lowest level that a view of the placed texture at `start` shows,
/// over the slots shaders pick by index that hold one now. The title
/// views a texture from the first level it holds. `None`: no such slot.
pub(crate) fn first_viewed(start: u64) -> Option<u32> {
    let heaps = PLACED_SPANS.lock().unwrap_or_else(|e| e.into_inner());
    heaps
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .filter_map(|spans| spans.lock().unwrap_or_else(|e| e.into_inner()).first_viewed(start))
        .min()
}

/// Levels on demand (`crate::tiles`): where the placed textures start
/// whose first level with memory changed since a texture pass last took
/// them ([`HostHeap::texture_views_remade`]).
static RESHOWN: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// The first level with memory of the placed texture at `start` changed:
/// its slots are made again at the next texture pass.
pub(crate) fn levels_changed(start: u64) {
    RESHOWN.lock().unwrap_or_else(|e| e.into_inner()).push(start);
}

/// [`levels_changed`] once the submissions begun so far (that of the list
/// whose step calls this among them) have run.
pub(crate) fn levels_changed_after(start: u64) {
    let after = crate::host::submissions().0;
    RESHOWN_AFTER.lock().unwrap_or_else(|e| e.into_inner()).push((after, start));
}

static RESHOWN_AFTER: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// Slots that show a placed texture from another level than the title's view
/// in them says now, and are made from that view when the upload it needs
/// has run ([`HostHeap::texture_slot_flip`]).
struct Flip {
    /// The submission to wait for.
    after: u64,
    /// The heap (its address), the slot, where its record is and which
    /// record it was.
    heap: std::sync::Weak<()>,
    index: u32,
    handle: u64,
    key: u64,
    version: Option<(u64, [u8; 32])>,
    /// The frame it was asked in, of a slot that shows nothing of the
    /// texture until then ([`HostHeap::texture_slot_show_after`]).
    late: Option<u64>,
    /// When it was asked (`crate::host::now_ms`), for `crate::texlate`.
    asked: u64,
}

/// Whether a title slot shows a texture that is new to it only once the
/// submission with its upload has finished (`VICTORY_SHOW_AFTER=1`; off).
/// Shown at once, the lists already on the card read it by index while it is
/// made: all three losses whose report had the address book (n08, n11, n15)
/// were page faults in a texture made and shown in the last tenth of a
/// second.
pub(crate) fn show_after() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SHOW_AFTER").is_some_and(|v| v != "0"))
}

/// Whether a title slot that would show a texture made while submissions
/// begun before it may still run shows it only once those have finished
/// (`VICTORY_SHOW_NEW_LATER`, on; 0: at once, as before; A/B-able).
///
/// [`show_after`] left out slots made "at once" from a draw's table, the
/// texture pass's paths that are no upload of theirs, and the heap's own
/// look at buffer views: the device losses of steps-ab2, start-3 and the
/// user's play-1125-2107 (no ray tracing) were all page faults at the first
/// byte of a texture in a block made that millisecond, its slot changed "at
/// once" while lists sent before read that slot by index (its neighbours,
/// shown after their upload, did not fault).
pub(crate) fn show_new_later() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_SHOW_NEW_LATER") {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SHOW_NEW_LATER").is_none_or(|v| v != "0"))
}

/// Slots [`show_new_later`] holds back now: heap, slot, record.
static NEW_PENDING: std::sync::LazyLock<Mutex<std::collections::HashSet<(usize, u32, u64)>>> =
    std::sync::LazyLock::new(Default::default);

/// Slots held back by [`show_new_later`] since the start.
pub(crate) static NEW_LATER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The submissions begun when the host texture `view` shows was made, if
/// one begun before that may still run ([`crate::pool::begun_before`]). The
/// host texture is made here if it is not yet, as [`make`] would.
fn begun_before(view: &View) -> Option<u64> {
    // (Resolved first: the texture made here, a moment ago, is the case.)
    let shader = match view {
        View::Placed(false, resource, _) if resource[0] != 1 => placed_texture(view)?,
        View::Shader(..) => view.clone(),
        _ => return None,
    };
    let View::Shader(resource, _) = shader else {
        return None;
    };
    let texture = crate::resource::host_resource(resource as crate::object::This)?;
    crate::pool::begun_before(texture)
}

fn flip_version(key: u64) -> Option<(u64, [u8; 32])> {
    let view = view_at(key as u32, (key >> 32) as u32)?;
    let resolved = placed_texture(&view).unwrap_or(view);
    let View::Shader(resource, _) = resolved else { return None; };
    let resource = resource as crate::object::This;
    let (_, memory, len) = crate::resource::title_texture(resource)?;
    Some((crate::resource::made_copy(resource)?.serial, crate::texture::backing_identity(memory, len)))
}

/// Slots shown late ([`show_after`]), the frames they waited, the longest;
/// those that were given the neutral texture meanwhile; those not shown any
/// more when their time came.
static LATE: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// What a texture slot showed before its last write: its record (0: null or
/// the neutral texture), when (ms), and the submissions begun and finished.
#[cfg(feature = "diagnostics")]
static BEFORE: Mutex<std::collections::BTreeMap<u32, (u64, u64, (u64, u64), &'static str)>> =
    Mutex::new(std::collections::BTreeMap::new());

#[cfg(feature = "diagnostics")]
fn note_before(index: u32, was: u64, by: &'static str) {
    if crate::book::on() {
        let entry = (was, crate::host::now_ms(), crate::host::submissions(), by);
        BEFORE.lock().unwrap_or_else(|e| e.into_inner()).insert(index, entry);
    }
}

/// A 1x1 gray texture, in the place of one that is not there yet.
fn neutral() -> Option<&'static ID3D12Resource> {
    static TEXTURE: std::sync::OnceLock<Option<ID3D12Resource>> = std::sync::OnceLock::new();
    TEXTURE
        .get_or_init(|| {
            let host = host()?;
            let device = &host.device;
            let (from, data) = host.upload_buffer(256)?;
            // SAFETY: the buffer's mapping; plain creation and recording,
            // waited for before what it names goes.
            unsafe {
                std::ptr::write_bytes(data, 0x80, 256);
                let mut made: Option<ID3D12Resource> = None;
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                        D3D12_HEAP_FLAG_NONE,
                        &D3D12_RESOURCE_DESC {
                            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                            Width: 1,
                            Height: 1,
                            DepthOrArraySize: 1,
                            MipLevels: 1,
                            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            ..Default::default()
                        },
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut made,
                    )
                    .ok()?;
                let made = made?;
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_COPY, ..Default::default() })
                    .ok()?;
                let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY).ok()?;
                let list: ID3D12GraphicsCommandList =
                    device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None).ok()?;
                let to = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&made),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                };
                let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        Width: 1,
                        Height: 1,
                        Depth: 1,
                        RowPitch: 256,
                    },
                };
                let source = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&from),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
                };
                list.CopyTextureRegion(&to, 0, 0, 0, &source, None);
                list.Close().ok()?;
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
                queue.ExecuteCommandLists(&[Some(windows::core::Interface::cast(&list).ok()?)]);
                queue.Signal(&fence, 1).ok()?;
                // (Without an event: this waits.)
                fence.SetEventOnCompletion(1, windows::Win32::Foundation::HANDLE::default()).ok()?;
                Some(made)
            }
        })
        .as_ref()
}

/// Makes the descriptor of [`neutral`] for a slot of `view` at `at`, if the
/// view is a 2D texture's or an array's; else a null one of its kind.
fn neutral_for(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<ID3D12Resource> {
    const TEXTURE2D: u32 = 4;
    const TEXTURE2D_ARRAY: u32 = 5;
    let dimension = match view {
        View::Shader(_, Some(desc)) => desc[1],
        View::Placed(false, resource, desc) if resource[0] != 1 => desc[1],
        _ => 0,
    };
    let desc = match dimension {
        TEXTURE2D => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV { MipLevels: 1, ..Default::default() },
            },
        },
        TEXTURE2D_ARRAY => D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D12_TEX2D_ARRAY_SRV { MipLevels: 1, ArraySize: 1, ..Default::default() },
            },
        },
        _ => {
            null_for(view, at);
            return None;
        }
    };
    let (Some(texture), Some(host)) = (neutral(), host()) else {
        null_for(view, at);
        return None;
    };
    // SAFETY: a slot of our heap, a live texture.
    unsafe { host.device.CreateShaderResourceView(texture, Some(&desc), at) };
    Some(texture.clone())
}

static FLIPS: Mutex<Vec<Flip>> = Mutex::new(Vec::new());

fn queue_flip(flip: Flip) {
    let mut flips = FLIPS.lock().unwrap_or_else(|e| e.into_inner());
    if flip.heap.strong_count() != 0 {
        flips.push(flip);
    }
}

impl Drop for HostHeap {
    fn drop(&mut self) {
        let old = std::sync::Arc::downgrade(&self.flip_identity);
        let own = old.as_ptr() as usize;
        // Invalidate callbacks before removing their outstanding records.
        // A replacement heap can reuse any object address, but cannot own
        // this identity while a callback still holds its weak reference.
        self.flip_identity = Default::default();
        FLIPS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|f| f.heap.as_ptr() as usize != own);
        NEW_PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(h, _, _)| *h != own);
        drop(old);
    }
}

/// Makes the slots whose new texture's upload has run
/// ([`HostHeap::texture_slots_flip`]); on any thread. Not if the heap is at
/// something else: the next texture pass does.
pub(crate) fn flip_now() {
    let _ = super::with_view_heap(HostHeap::texture_slots_flip);
}

/// Since the start: slots made from their new view after its upload had run,
/// and those the title had written again by then.
pub(crate) static FLIPPED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// The key of the record at `handle`, a view's descriptor slot; 0: none.
fn record_key(handle: u64) -> u64 {
    // SAFETY: the handle is a descriptor slot in title memory.
    let [tag, i, generation] = unsafe { super::record_bytes::read(handle as *const u8) };
    if tag == VIEW_TAG { u64::from(generation) << 32 | u64::from(i) } else { 0 }
}

/// Levels on demand (`crate::tiles`): where the placed textures start
/// whose last upload left a held level without tiles (the card had no
/// room, or no heap could be had), and when (`crate::host::moment`). A
/// texture pass makes their slots due again once the card has room
/// ([`HostHeap::texture_slots_waiting`]).
static WAITING: Mutex<std::collections::BTreeMap<u64, u64>> = Mutex::new(std::collections::BTreeMap::new());

/// The last upload of the placed texture at `start` left a held level
/// without tiles. A texture picked by index is uploaded once and then
/// only when its memory is found changed, and its staging buffer goes
/// with the upload: without this the level stayed empty (the texture
/// blurred, or black if no level had tiles) for as long as the title left
/// its memory alone, where a texture refused for want of room is tried
/// again.
pub(crate) fn levels_waiting(start: u64) {
    WAITING.lock().unwrap_or_else(|e| e.into_inner()).insert(start, crate::host::moment());
}

/// Placed textures (by the start of their memory) filled while the runtime
/// was remapping with less than was mapped of them
/// (`crate::texture::fill_unlocked`), whose slots the next texture pass
/// makes due again; and how often in a row that was, which ends it: memory
/// that is shown and cannot be read would be uploaded again every frame.
static SHORT: Mutex<(std::collections::BTreeSet<u64>, std::collections::BTreeMap<u64, u32>)> =
    Mutex::new((std::collections::BTreeSet::new(), std::collections::BTreeMap::new()));

/// Fillings in a row of one texture that got less than was mapped, after
/// which its slots wait for the next notice of its memory.
const SHORT_IN_A_ROW: u32 = 8;

/// The placed texture at `start` was filled with less than was mapped.
pub(crate) fn read_short(start: u64) {
    let mut short = SHORT.lock().unwrap_or_else(|e| e.into_inner());
    let times = short.1.entry(start).or_insert(0);
    *times += 1;
    if *times <= SHORT_IN_A_ROW {
        short.0.insert(start);
    }
}

/// The placed texture at `start` was filled with all that was mapped.
pub(crate) fn read_all(start: u64) {
    let mut short = SHORT.lock().unwrap_or_else(|e| e.into_inner());
    if !short.1.is_empty() {
        short.1.remove(&start);
    }
}

/// Title memory by the placed texture views that describe it: per span of
/// memory (its start, its end at full size) the title slots holding a
/// view of a texture that takes exactly that span. A console title need
/// not clear a descriptor when it streams a texture out, so a newer view
/// that cannot be there together with an older one is all that says the
/// older one is stale ([`Rule`] says when that is). Views of one span with
/// different descriptions (the formats of one family, say) are one
/// texture's.
///
/// Spans may overlap: a texture's smallest levels lie at the start of its
/// memory, and a title that holds only those puts the next texture where
/// the larger ones would be (the title: three 512x256 textures
/// of 0x2c000 bytes each whose starts are 0x2000 and 0x4000 apart, all
/// three read by the same frame's draws, run 188). What is there of a
/// texture ends where the next one starts.
/// No host heap is needed, so it is tested without one.
#[derive(Default)]
pub(crate) struct Spans {
    longest: u64,
    /// When a newer view makes an older one stale.
    rule: Rule,
    by_start: std::collections::BTreeMap<u64, (u64, std::collections::BTreeSet<u32>)>,
    by_slot: std::collections::HashMap<u32, u64>,
    /// The level each slot's view starts at, and where what it is read from
    /// ends ([`Spans::shows`]).
    first: std::collections::HashMap<u32, (u32, u64, u64)>,
    /// The bytes a placed texture takes, by resource description: worked
    /// out once for each (a title writes the views of a few thousand
    /// descriptions, thousands of times a frame).
    sizes: std::collections::HashMap<[u32; 14], Option<u64>>,
}

/// When a view the title writes makes an older view of other memory stale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Rule {
    /// When it starts where the older one starts: at the same address
    /// with another length, or within the older one's first [`HEAD`]
    /// bytes (or the older within its own). No two textures share the
    /// block of their smallest levels. A view that only reaches into the
    /// address range of an older texture's larger levels says nothing.
    #[default]
    Start,
    /// Whenever their spans at full size overlap (the rule until run
    /// 188, where it made null the slots of textures packed as above:
    /// each view written took its neighbours out of the queue and off the
    /// card, for good). `VICTORY_STALE_TEXTURES=overlap`.
    Overlap,
    /// Never; a view that would be stale under [`Rule::Overlap`] is left
    /// out of the spans instead. `VICTORY_STALE_TEXTURES=0`, as before.
    Never,
}

/// The first bytes of a placed texture that are its own under
/// [`Rule::Start`]: the 4 KiB block its smallest levels share. (Of 129
/// pairs of overlapping neighbours in the listings of runs 188 and 189,
/// none starts closer than 0x2000 to the one before it.)
const HEAD: u64 = 4096;

impl Spans {
    /// The bytes of title memory a placed texture with the resource
    /// description `desc` takes (`crate::resource::placed_bytes`).
    fn bytes(&mut self, desc: &[u32; 14]) -> Option<u64> {
        if let Some(&known) = self.sizes.get(desc) {
            return known;
        }
        let bytes = crate::resource::placed_bytes(desc);
        self.sizes.insert(*desc, bytes);
        bytes
    }

    /// The starts of the spans that overlap `[start, end)`, each taken to
    /// end where the next one starts: the last span that starts before
    /// `start`, if it reaches past it at full size, and every span that
    /// starts inside.
    fn overlapping(&self, start: u64, end: u64) -> Vec<u64> {
        let before = self
            .by_start
            .range(..start)
            .next_back()
            .filter(|(_, span)| span.0 > start)
            .map(|(at, _)| *at);
        before.into_iter().chain(self.by_start.range(start..end).map(|(at, _)| *at)).collect()
    }

    /// Slot `index` holds a view of a texture in `[start, end)`. `newer`:
    /// the title wrote it now, so the slots whose views cannot be there
    /// with it ([`Rule`]) are stale: they are returned, and forgotten.
    /// Otherwise (a draw made the slot from what it held), and under
    /// [`Rule::Never`], a view that clashes with other spans is left out.
    fn note(&mut self, index: u32, start: u64, end: u64, newer: bool) -> Vec<u32> {
        let same = |span: Option<&(u64, std::collections::BTreeSet<u32>)>| span.is_some_and(|s| s.0 == end);
        if self.by_slot.get(&index) == Some(&start) && same(self.by_start.get(&start)) {
            return Vec::new();
        }
        self.forget(index);
        if end <= start {
            return Vec::new();
        }
        self.longest = self.longest.max(end - start);
        let rule = self.rule;
        // The spans the view cannot be there with.
        let clashing: Vec<u64> = match rule {
            Rule::Start => self
                .by_start
                .range(start.saturating_sub(HEAD - 1)..start.saturating_add(HEAD))
                .map(|(at, _)| *at)
                .collect(),
            Rule::Overlap | Rule::Never => self.overlapping(start, end),
        };
        let mut stale = Vec::new();
        for at in clashing {
            if at == start && same(self.by_start.get(&at)) {
                continue;
            }
            if !newer || rule == Rule::Never {
                return stale;
            }
            if let Some((old_end, slots)) = self.by_start.remove(&at) {
                // The first of them are said with both spans, to tell a
                // replacement from a neighbour in one launch's log.
                static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 48 {
                    tracing::info!(
                        slot = index,
                        start = format_args!("{start:#x}"),
                        end = format_args!("{end:#x}"),
                        stale_start = format_args!("{at:#x}"),
                        stale_end = format_args!("{old_end:#x}"),
                        stale_slots = slots.len(),
                        rule = ?rule,
                        "a placed texture view makes older views of other memory stale"
                    );
                }
                for slot in slots {
                    self.by_slot.remove(&slot);
                    stale.push(slot);
                }
            }
        }
        self.by_start.entry(start).or_insert_with(|| (end, Default::default())).1.insert(index);
        self.by_slot.insert(index, start);
        stale
    }

    /// The view in slot `index` starts at level `first` of its texture; the
    /// memory of that and the smaller levels ends at `end`.
    fn shows(&mut self, index: u32, first: u32, end: u64, key: u64) {
        self.first.insert(index, (first, end, key));
    }

    /// Whether slot `index`'s view is read from memory at or past `start`.
    fn reaches(&self, index: u32, start: u64) -> bool {
        self.first.get(&index).is_none_or(|&(_, end, _)| end > start)
    }

    /// The lowest level a view of the texture at `start` starts at.
    fn first_viewed(&self, start: u64) -> Option<u32> {
        self.by_start.get(&start)?.1.iter().filter_map(|slot| self.first.get(slot)).map(|&(first, _, _)| first).min()
    }

    /// The first span that starts inside `[start, end)` past the first
    /// [`HEAD`] bytes of it.
    fn next_start(&self, start: u64, end: u64) -> Option<u64> {
        self.by_start.range(start.saturating_add(HEAD)..end).next().map(|(at, _)| *at)
    }

    /// The slots that hold a view of the texture at `start`.
    fn slots_at(&self, start: u64) -> Vec<u32> {
        self.by_start.get(&start).map(|span| span.1.iter().copied().collect()).unwrap_or_default()
    }

    /// Slot `index` holds no placed texture view any more.
    fn forget(&mut self, index: u32) {
        self.first.remove(&index);
        let Some(start) = self.by_slot.remove(&index) else {
            return;
        };
        if let Some(span) = self.by_start.get_mut(&start) {
            span.1.remove(&index);
            if span.1.is_empty() {
                self.by_start.remove(&start);
            }
        }
    }

    /// The slots whose texture's memory overlaps `[start, end)`.
    fn slots_in(&self, start: u64, end: u64) -> Vec<u32> {
        self.overlapping(start, end)
            .iter()
            .filter_map(|at| self.by_start.get(at))
            .flat_map(|span| span.1.iter().copied())
            .filter(|&slot| self.reaches(slot, start))
            .collect()
    }

    /// Producer notifications cover every overlapping view, including a large
    /// texture whose nearer neighbour ends before the modified bytes.
    fn affected(&self, start: u64, end: u64) -> Vec<u32> {
        if start >= end {
            return Vec::new();
        }
        self.by_start
            .range(start.saturating_sub(self.longest)..end)
            .filter(|(_, (limit, _))| *limit > start)
            .flat_map(|(_, (_, slots))| slots.iter().copied())
            .filter(|&slot| self.reaches(slot, start))
            .collect()
    }

    /// The slots of the textures that hold bytes of `[start, end)`, for a
    /// write there: as [`Spans::slots_in`], but when a texture starts at
    /// `start` itself the one before it is left out (it ends where that
    /// one starts, and a request that fills a texture begins at its first
    /// byte: `slots_in` would name its neighbour with every such write).
    fn slots_holding(&self, start: u64, end: u64) -> Vec<u32> {
        if end <= start {
            return Vec::new();
        }
        let before = if self.by_start.contains_key(&start) {
            None
        } else {
            self.by_start.range(..start).next_back().filter(|(_, span)| span.0 > start).map(|(_, span)| span)
        };
        before
            .into_iter()
            .chain(self.by_start.range(start..end).map(|(_, span)| span))
            .flat_map(|span| span.1.iter().copied())
            .filter(|&slot| self.reaches(slot, start))
            .collect()
    }
}

/// A heap consumes its own cursor. If a slow heap falls behind the
/// retained interval, its next pass revisits every intersecting slot.
struct Changes {
    seq: u64,
    observed: u64,
    ranges: std::collections::VecDeque<(u64, u64)>,
}

impl Changes {
    const fn new() -> Self {
        Self {
            seq: 0,
            observed: 0,
            ranges: std::collections::VecDeque::new(),
        }
    }
    fn push(&mut self, lo: u64, hi: u64) {
        if self.seq != self.observed
            && let Some(last) = self.ranges.back_mut()
            && lo <= last.1
            && last.0 <= hi
        {
            last.0 = last.0.min(lo);
            last.1 = last.1.max(hi);
            return;
        }
        if self.ranges.len() == 1 << 15 {
            self.ranges.pop_front();
        }
        self.seq += 1;
        self.ranges.push_back((lo, hi));
    }
    fn take(&mut self, cursor: &std::sync::atomic::AtomicU64) -> Vec<(u64, u64)> {
        let old = cursor.swap(self.seq, std::sync::atomic::Ordering::Relaxed);
        self.observed = self.seq;
        let count = (self.seq - old) as usize;
        if count > self.ranges.len() {
            return vec![(0, u64::MAX)];
        }
        self.ranges
            .range(self.ranges.len() - count..)
            .copied()
            .collect()
    }
}
static PAGE_WRITTEN: Mutex<Changes> = Mutex::new(Changes::new());

/// Since the start: such ranges kept for a pass; ranges dropped because no
/// pass took them; slots of placed textures that held bytes of them when a
/// pass took them; of those, the slots whose texture had been uploaded
/// before the write (out of the queue, or in it after an upload): they are
/// the ones that kept the bytes from before, and the ones made due when
/// `VICTORY_PAGE_WRITES` asks for it.
pub(crate) static PAGE_WRITES: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// The bytes of a write the guards do not see are in title memory `[at,
/// at + len)`: the placed textures there changed with no descriptor
/// written and nothing mapped. Kept for the next texture pass
/// ([`HostHeap::texture_slots_page_written`]), as [`remapped`] keeps its
/// ranges; locks one mutex and calls nothing.
pub(crate) fn page_written(at: usize, len: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let (start, end) = (at as u64, at.saturating_add(len) as u64);
    if end <= SPANNED[0].load(Relaxed) || start >= SPANNED[1].load(Relaxed) {
        return;
    }
    PAGE_WRITES[0].fetch_add(1, Relaxed);
    PAGE_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(start, end);
}

/// Title memory the runtime mapped, unmapped or is about to write since a
/// texture pass last took the ranges ([`remapped`]).
static REMAPPED: Mutex<Changes> = Mutex::new(Changes::new());

/// The lowest start and the highest end of any span of [`Spans`] so far
/// (the start is `u64::MAX` until there is one): what the runtime does to
/// memory outside is none of the texture pass's business.
static SPANNED: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(u64::MAX),
    std::sync::atomic::AtomicU64::new(0),
];

/// Since the start: slots whose view a newer view of other memory there
/// made stale; ranges of memory noted as mapped, unmapped or written;
/// ranges dropped because no pass took them; slots made due by such
/// ranges; and made slots whose memory was found unmapped (made null).
pub(crate) static STALE: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// When a view the title writes makes older views of other memory stale
/// ([`Rule`]): where it starts in the block of their smallest levels, by
/// default; `VICTORY_STALE_TEXTURES=0`: never, as before;
/// `VICTORY_STALE_TEXTURES=overlap`: wherever their spans overlap, the rule
/// until run 188.
fn stale_rule() -> Rule {
    static RULE: std::sync::OnceLock<Rule> = std::sync::OnceLock::new();
    *RULE.get_or_init(|| match std::env::var("VICTORY_STALE_TEXTURES").as_deref() {
        Ok("0") => Rule::Never,
        Ok("overlap") => Rule::Overlap,
        _ => Rule::Start,
    })
}

/// The runtime mapped or unmapped title memory `[at, at + len)`, or is
/// about to write it through the kernel (`crate::watch`): the placed
/// textures there may have arrived, gone, or changed with no descriptor
/// written. The range is kept for the next texture pass
/// ([`HostHeap::texture_slots_remapped`]). Called with the runtime's own
/// locks held: this locks one mutex of its own and calls nothing.
pub(crate) fn remapped(at: usize, len: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    let (start, end) = (at as u64, at.saturating_add(len) as u64);
    if end <= SPANNED[0].load(Relaxed) || start >= SPANNED[1].load(Relaxed) {
        return;
    }
    STALE[1].fetch_add(1, Relaxed);
    REMAPPED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(start, end);
}

/// Ranges of memory, those that touch made one (DexNom/VIctory 921d8d3b:
/// several threads map and write, some 5,000 ranges a frame, the same
/// textures' over and over, and each was looked up).
fn join(ranges: &mut Vec<(u64, u64)>) {
    ranges.sort_unstable();
    ranges.dedup_by(|next, last| {
        if next.0 > last.1 {
            return false;
        }
        last.1 = last.1.max(next.1);
        true
    });
}

/// Frames for which a slot a draw read through a table counts as a table's
/// slot ([`HostHeap::tabled`]).
const TABLED_FRAMES: u64 = 240;

/// Frames a write of a table's slot waits in the queue for the draw that
/// reads it, before the slot is taken for one shaders pick by index.
/// Frames as `crate::frame::current` counts them: in its opening cutscene
/// The title begins two or three of them per frame it presents
/// (435 begun in the 353 s of run 52's, for some 140 presented).
const TABLED_GRACE: u64 = 8;

/// Frames for which a record copied into a slot a draw read through a
/// table is left to such a draw ([`HostHeap::copied_tabled`]).
const COPIED_FRAMES: u32 = 8;

/// Frames a record copied into any other slot waits for one, before it is
/// taken for one shaders pick by index ([`HostHeap::copied_wait`]).
const COPIED_GRACE: u64 = 4;

/// In [`HostHeap::tabled`]: a draw read the slot in that frame.
const TABLED_READ: u32 = 1 << 31;

/// Since the start: writes of texture views into slots that count as a
/// table's (due later), and slots that left the queue because a draw read
/// them through a table.
pub(crate) static TABLED_WRITES: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Whether slots draws read through tables leave the queue of textures
/// picked by index; `VICTORY_TABLED_TEXTURES=1` keeps them in it as
/// before.
fn tabled_out() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TABLED_TEXTURES").is_none_or(|v| v != "1"))
}

/// What the queue of textures picked by index knows of a title slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Waiting {
    /// The frame from which the slot is due.
    due: u64,
    /// The frame it was written in.
    written: u64,
    /// Which write of the queue put it here: the pass hands it back with
    /// what it did, so a slot written again meanwhile stays as written.
    write: u64,
    /// Looks in a row that found nothing to upload (its memory not mapped,
    /// or no more of it than at the last upload).
    looks: u8,
    /// Bytes of its memory that were mapped when it was last uploaded
    /// without all of it there; 0: not uploaded yet.
    mapped: u64,
    backing: Option<[u8; 32]>,
    /// For a slot uploaded whole and looked at again
    /// ([`TextureQueue::recheck`]): where its memory is (address, bytes),
    /// the fingerprint it had at the upload, how many looks found it
    /// unchanged, and the frame of the upload. A fingerprint of 0: none.
    checked: (u64, u64, u64, u8, u64),
    /// When the slot first waited for this (`crate::host::now_ms`), for
    /// `crate::texlate`: kept over writes until it is brought up to date;
    /// and whether it waited for its memory meanwhile.
    since: u64,
    waited: bool,
    /// When a look last found its memory away (or no more of it): 0 none.
    away: u64,
}

/// The queue of title slots whose texture view waits to be brought up to
/// date for shaders that pick textures by index: which are due, and in
/// which order (no host heap needed, so it is tested without one).
#[derive(Default)]
pub(crate) struct TextureQueue {
    slots: std::collections::BTreeMap<u32, Waiting>,
    writes: u64,
    /// Slots held back because the card had no room for the host copy
    /// they need ([`TextureQueue::hold`]), each with the write it was
    /// held as.
    held: std::collections::BTreeMap<u32, u64>,
    /// Which slots are in `slots`, for those who only want one out
    /// ([`Queued`]).
    queued: std::sync::Arc<Queued>,
}

/// A bit for each slot of a heap's [`TextureQueue`], changed under its lock
/// and read without it: most of the 50,000 descriptors a frame written,
/// looked at and made are not textures, and all of them took that lock to
/// take nothing out of the queue (12 ms a frame of waiting, run city1).
/// Empty: not known.
#[derive(Default)]
pub(crate) struct Queued(Box<[std::sync::atomic::AtomicU64]>);

impl Queued {
    fn new(slots: u32) -> Queued {
        Queued((0..slots.div_ceil(64)).map(|_| std::sync::atomic::AtomicU64::new(0)).collect())
    }

    fn set(&self, slot: u32, there: bool) {
        use std::sync::atomic::Ordering::SeqCst;
        if let Some(word) = self.0.get(slot as usize / 64) {
            let bit = 1 << (slot % 64);
            if there {
                word.fetch_or(bit, SeqCst);
            } else {
                word.fetch_and(!bit, SeqCst);
            }
        }
    }

    /// Whether `slot` may be in the queue.
    fn may(&self, slot: u32) -> bool {
        let word = self.0.get(slot as usize / 64);
        word.is_none_or(|w| w.load(std::sync::atomic::Ordering::SeqCst) >> (slot % 64) & 1 != 0)
    }
}

/// Slots due of a [`TextureQueue`], of those looked at before and of the
/// others: the frame each was written in, the slot and its write's number.
type Due = [Vec<(u64, u32, u64)>; 2];

impl TextureQueue {
    fn insert(&mut self, index: u32, waiting: Waiting) {
        if self.slots.insert(index, waiting).is_none() {
            self.queued.set(index, true);
        }
    }

    fn take_out(&mut self, index: u32) -> bool {
        let was = self.slots.remove(&index).is_some();
        if was {
            self.queued.set(index, false);
        }
        was
    }

    /// A texture view was written into slot `index` in `frame`, due from
    /// frame `due`. `seen`: it was not written, a draw or the pass made
    /// the host slot from a view the queue may hold already, which then
    /// stays as it is.
    fn written(&mut self, index: u32, frame: u64, due: u64, seen: bool) {
        if seen && self.slots.contains_key(&index) {
            return;
        }
        self.writes += 1;
        let write = self.writes;
        let (since, waited, away) = self.kept(index);
        self.insert(
            index,
            Waiting {
                due,
                written: frame,
                write,
                looks: 0,
                mapped: 0,
                backing: None,
                checked: (0, 0, 0, 0, 0),
                since,
                waited,
                away,
            },
        );
    }

    /// What a slot written again keeps of its wait.
    fn kept(&self, index: u32) -> (u64, bool, u64) {
        self.slots.get(&index).map_or_else(
            || (crate::host::now_ms(), false, 0),
            |w| (w.since, w.waited || w.looks > 0 || w.mapped > 0, w.away),
        )
    }

    /// When slot `index` began to wait, whether it waited for its memory, and
    /// when a look last found that away.
    fn since(&self, index: u32) -> Option<(u64, bool, u64)> {
        self.slots.get(&index).map(|w| (w.since, w.waited || w.looks > 0 || w.mapped > 0, w.away))
    }

    fn remove(&mut self, index: u32) {
        self.take_out(index);
    }

    /// Up to `max` slots due at `frame`, each with its write number, in
    /// the order to serve them:
    /// - first the ones looked at before (their memory was not mapped, or
    ///   only part of it): the look is a query, and memory that has
    ///   arrived since is what the title streamed in last;
    /// - then the others, the newest write first and the oldest next, in
    ///   turns: slot order served the low slots of a backlog of
    ///   25,000-40,000 while the textures of the shot on screen waited
    ///   (run 52: 98% of the drawn pixels had no colour texture in the
    ///   first 3D frames).
    #[cfg(test)]
    fn due(&self, frame: u64, max: usize) -> Vec<(u32, u64)> {
        Self::in_order(self.all_due(frame), max)
    }

    /// The slots due at `frame`, for [`TextureQueue::in_order`].
    fn all_due(&self, frame: u64) -> Due {
        let mut due = Due::default();
        for (&index, w) in self.slots.iter().filter(|(_, w)| w.due <= frame) {
            due[usize::from(w.looks == 0 && w.mapped == 0)].push((w.written, index, w.write));
        }
        due
    }

    /// What [`TextureQueue::due`] says of `due` (with nothing held: some
    /// ten thousand slots are sorted).
    fn in_order([mut again, mut fresh]: Due, max: usize) -> Vec<(u32, u64)> {
        // By the number of the write, counted over the run.
        let newest_first = |a: &(u64, u32, u64), b: &(u64, u32, u64)| b.2.cmp(&a.2);
        if max == 0 {
            return Vec::new();
        }
        if again.len() > max {
            again.select_nth_unstable_by(max, newest_first);
            again.truncate(max);
        }
        again.sort_unstable_by(newest_first);
        let mut order: Vec<_> = again
            .into_iter()
            .map(|(_, slot, write)| (slot, write))
            .collect();
        let remaining = (max - order.len()).min(fresh.len());
        if remaining == 0 {
            return order;
        }
        let newest = remaining.div_ceil(2);
        let oldest = remaining / 2;
        if remaining < fresh.len() {
            fresh.select_nth_unstable_by(newest, newest_first);
            let rest = &mut fresh[newest..];
            if oldest > 0 {
                rest.select_nth_unstable_by(oldest, |a, b| newest_first(b, a));
            }
            fresh.truncate(remaining);
        } else if newest < fresh.len() {
            fresh.select_nth_unstable_by(newest, newest_first);
        }
        let (new, old) = fresh.split_at_mut(newest);
        new.sort_unstable_by(newest_first);
        old.sort_unstable_by(|a, b| newest_first(b, a));
        for (i, &(_, slot, write)) in new.iter().enumerate() {
            order.push((slot, write));
            if let Some(&(_, slot, write)) = old.get(i) {
                order.push((slot, write));
            }
        }
        order
    }

    /// The entry of `index` if it is still the write the pass was handed.
    fn same(&mut self, index: u32, write: u64) -> Option<&mut Waiting> {
        self.slots.get_mut(&index).filter(|w| w.write == write)
    }

    /// Slot `index` is up to date: out of the queue until written again.
    fn done(&mut self, index: u32, write: u64) {
        if self.same(index, write).is_some() {
            self.take_out(index);
        }
    }

    /// Nothing to upload at slot `index` now: looked at again after 1, 2,
    /// 4, ... frames, at most `most`.
    fn later(&mut self, index: u32, write: u64, frame: u64, most: u64) {
        if let Some(w) = self.same(index, write) {
            w.due = frame + (1u64 << w.looks.min(16)).min(most.max(1));
            w.looks = w.looks.saturating_add(1);
            w.away = crate::host::now_ms();
        }
    }

    /// The card has no room for the host copy slot `index` needs: it is
    /// not handed out again before frame `due`, or before the card has
    /// room again ([`TextureQueue::room`]). Nothing else of it changes: it
    /// is not a slot "waiting for its memory", and when it is due it has
    /// its old place among the slots never looked at. (It stayed due, so
    /// every pass on a full card looked at the same few hundred slots
    /// again and stopped there, and the slots behind them were never
    /// reached: the ones whose host copy exists and only wants bringing up
    /// to date, and the ones whose memory went, which is what lets host
    /// copies go and makes room.)
    fn hold(&mut self, index: u32, write: u64, due: u64) {
        if let Some(w) = self.same(index, write) {
            w.due = w.due.max(due);
            self.held.insert(index, write);
        }
    }

    /// The card has room again at `frame`: the slots held back for lack of
    /// it are due now, as if they had never been held. Returns how many.
    fn room(&mut self, frame: u64) -> usize {
        let held = std::mem::take(&mut self.held);
        let mut back = 0;
        for (index, write) in held {
            if let Some(w) = self.same(index, write) {
                w.due = w.due.min(frame);
                back += 1;
            }
        }
        back
    }

    /// Slot `index` was uploaded with `mapped` bytes of its memory there,
    /// not all of it: looked at again from frame `due`.
    fn partly(&mut self, index: u32, write: u64, due: u64, mapped: u64) {
        if let Some(w) = self.same(index, write) {
            (w.due, w.looks, w.mapped) = (due, 0, mapped.max(1));
        }
    }

    /// Slot `index` was uploaded whole from memory `at` (address, bytes)
    /// with fingerprint `print`: looked at again from frame `due`, for a
    /// title that fills a texture's memory after it wrote the view (the
    /// `looks`-th such look; `uploaded` is the upload's frame).
    #[allow(clippy::too_many_arguments)]
    fn recheck(&mut self, index: u32, write: u64, due: u64, at: (u64, u64), print: u64, looks: u8, uploaded: u64) {
        if let Some(w) = self.same(index, write) {
            (w.due, w.looks, w.mapped) = (due, 1, 0);
            w.checked = (at.0, at.1, print, looks, uploaded);
        }
    }

    /// What [`TextureQueue::recheck`] noted of slot `index`, if it is
    /// waiting for such a look.
    fn checked(&self, index: u32) -> Option<(u64, u64, u64, u8, u64)> {
        self.slots.get(&index).map(|w| w.checked).filter(|c| c.2 != 0)
    }

    /// The memory slot `index`'s texture lies in was mapped, unmapped or
    /// written in `frame`: the slot is looked at from frame `due` as one
    /// written now, whatever was known of it (a pass at work on it does
    /// not take it out).
    fn changed(&mut self, index: u32, frame: u64, due: u64) {
        self.writes += 1;
        let write = self.writes;
        let due = self.slots.get(&index).map_or(due, |w| w.due.min(due));
        let (since, waited, away) = self.kept(index);
        self.insert(
            index,
            Waiting {
                due,
                written: frame,
                write,
                looks: 0,
                mapped: 0,
                backing: None,
                checked: (0, 0, 0, 0, 0),
                since,
                waited,
                away,
            },
        );
    }

    /// Bytes of slot `index`'s memory that were mapped at its last upload
    /// (0: not uploaded, or all of it was).
    fn mapped(&self, index: u32) -> u64 {
        self.slots.get(&index).map_or(0, |w| w.mapped)
    }

    /// For the frame line: slots in the queue; of them due at `frame`;
    /// waiting for their memory (looked at, never uploaded); uploaded in
    /// part; and the frames the oldest due one has waited since its write.
    #[cfg(test)]
    fn census(&self, frame: u64) -> [u64; 5] {
        self.scan_due(frame, 0).1
    }

    fn scan_due(&self, frame: u64, cap: usize) -> (Due, [u64; 5]) {
        let mut due = Due::default();
        let mut c = [self.slots.len() as u64, 0, 0, 0, 0];
        for (&slot, w) in &self.slots {
            if w.mapped > 0 {
                c[3] += 1;
            } else if w.looks > 0 {
                c[2] += 1;
            }
            if w.due <= frame {
                if cap != 0 {
                    due[usize::from(w.looks == 0 && w.mapped == 0)]
                        .push((w.written, slot, w.write));
                }
                c[1] += 1;
                c[4] = c[4].max(frame.saturating_sub(w.written));
            }
        }
        (due, c)
    }
}

/// Whether `view` is of a texture the title may fill itself: a texture it
/// placed in memory, or a texture object.
pub(crate) fn is_texture_view(view: &View) -> bool {
    match view {
        View::Placed(false, resource, _) => resource[0] != 1,
        View::Shader(r, _) => crate::resource::buffer_bytes(*r as crate::object::This).is_none(),
        _ => false,
    }
}

/// Buffer views left null because the host would refuse them
/// ([`buffer_view_ok`]), since the start.
pub(crate) static REFUSED_VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether the host device is gone, and why (the first time it is asked
/// after that): for passes that make many views, to name the one that did
/// it.
pub(crate) fn device_removed() -> Option<String> {
    let host = host()?;
    // SAFETY: a plain query.
    let reason = unsafe { host.device.GetDeviceRemovedReason() };
    reason.err().map(|e| e.to_string())
}

/// Writes of title view-heap slots with a placed texture view and with a
/// view of a texture object, since the start (diagnostic counts).
pub(crate) static PLACED_TEXTURE_SLOTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) static OBJECT_TEXTURE_SLOTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bytes per element of a buffer view, as the console's descriptor holds
/// them for indexed reads (structured: the stride; raw: 1, the index is the
/// byte; typed: the format's); `None` if `view` is not a buffer view.
fn view_stride(view: &View) -> Option<u32> {
    let (stride, raw, format) = view_elements(view)?;
    Some(if stride > 0 {
        stride
    } else if raw {
        1
    } else {
        crate::resource::element(format).map_or(0, |(bytes, _)| bytes as u32)
    })
}

/// What else the table of strides says of a typed buffer view of an integer
/// format (`victory_shader::emit::VIEW_INTEGER`); 0 for any other view.
fn view_kind(view: &View) -> u32 {
    use victory_shader::emit::{VIEW_INTEGER, VIEW_SIGNED};
    match view_elements(view) {
        Some((0, false, 3 | 7 | 12 | 17 | 25 | 30 | 36 | 42 | 50 | 57 | 62)) => VIEW_INTEGER,
        Some((0, false, 4 | 8 | 14 | 18 | 32 | 38 | 43 | 52 | 59 | 64)) => VIEW_INTEGER | VIEW_SIGNED,
        _ => 0,
    }
}

/// A buffer view's structure stride, whether it is raw, and its format;
/// `None` if `view` is not a buffer view.
fn indexed_form(view: &View) -> u8 {
    match view {
        View::Shader(_, Some(desc)) if desc[1] == D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE.0 as u32 => 32,
        View::Constant(..) => 1,
        View::Shader(..) | View::Placed(false, ..) if view_elements(view).is_some() => 2,
        View::Unordered(..) | View::Placed(true, ..) if view_elements(view).is_some() => 4,
        View::Unordered(..) | View::Placed(true, ..) => 16,
        View::Shader(..) | View::Placed(false, ..) => 8,
        _ => 0,
    }
}

pub(crate) fn explicit_null_texture(view: &View) -> bool {
    match view {
        View::Shader(0, Some(d)) => matches!(d[1], 2..=10),
        View::Unordered(0, 0, Some(d)) => matches!(d[1], 2..=5 | 8),
        _ => false,
    }
}

fn required_view(view: &View) -> bool {
    match view {
        View::Placed(_, r, _) if r[0] != 1 => true,
        View::Shader(r, _) | View::Unordered(r, _, _) if *r != 0 => true,
        _ => requires_buffer(view),
    }
}

#[derive(Clone)]
struct PlainMaterial {
    record: u64,
    topology: u64,
    ranges: std::sync::Arc<[(usize, usize)]>,
    binding: std::sync::Arc<std::sync::OnceLock<MaterialBinding>>,
}

struct MaterialBinding {
    object: usize,
    view: View,
    image: std::sync::Weak<crate::resource::HostTexture>,
}

impl PlainMaterial {
    fn after_writes(&self, writes: &Option<Vec<(usize, usize)>>) -> bool {
        writes.as_ref().is_some_and(|writes| !self.ranges.iter().any(|&(lo, hi)| {
            let i = writes.partition_point(|&(_, end)| end <= lo);
            writes.get(i).is_some_and(|&(start, _)| start < hi)
        }))
    }
}

fn placement_address(view: &View) -> Option<u64> {
    match view {
        View::Placed(_, _, words) => Some(u64::from(words[10]) | u64::from(words[11]) << 32),
        _ => None,
    }
}

fn note_indexed_change(logical: u32, previous: Option<(u64, u64, Option<u64>)>, current: (u64, u64, Option<u64>), addresses: (Option<u64>, Option<u64>), reason: &'static str, failure: Option<&'static str>) {
    static SEEN: std::sync::LazyLock<Mutex<std::collections::HashSet<(&'static str, Option<&'static str>)>>> =
        std::sync::LazyLock::new(Default::default);
    static LATE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let prepared = failure.is_none();
    let count = if prepared { LATE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1 } else { 0 };
    let first = !prepared && SEEN.lock().unwrap_or_else(|e| e.into_inner()).insert((reason, failure));
    #[cfg(feature = "diagnostics")]
    let detailed = {
        use crate::host::IndexedFailureTrace;
        static TRACE: IndexedFailureTrace<(&str, &str, u32, u64)> = IndexedFailureTrace::new();
        failure.is_some_and(|failure| TRACE.report((reason, failure, logical, current.1)))
    };
    #[cfg(not(feature = "diagnostics"))]
    let detailed = false;
    if first || detailed || count.is_power_of_two() {
        tracing::info!(logical, reason, ?previous, ?current, ?addresses, prepared, ?failure, late_sample_checks = count, "indexed texture changed after first observation");
    }
}

/// A prelude that reads Xbox memory must find the last producer of every
/// selected byte. A normal host-authored SRV reads its bound native image
/// directly, just as the original draw does in `readable_resolved`.
fn late_guest_sample_conflict(logical: u32, object: usize, copy: &crate::resource::HostTexture) -> Option<&'static str> {
    late_sample_span_conflict(logical, object, copy, crate::resource::guest_texture_span(object as _), |_| false)
}

pub(crate) fn late_sample_span_conflict(logical: u32, object: usize, copy: &crate::resource::HostTexture,
    span: Option<(usize,usize)>, list_writes: impl Fn(&[(usize, usize)]) -> bool) -> Option<&'static str> {
    let (reason, overlaps, aliases) = if let Some((at, len)) = span {
        loop {
            let generation = crate::resource::alias_generation();
            let aliases = crate::inplace::aliases_of_bytes(at, len);
            let targets = crate::resource::gpu_targets_in_aliases(object, &aliases);
            let conflict = if !targets.is_empty() { Some("GPU target overlaps guest upload bytes") }
                else if crate::texture::copy_writer_overlaps(&aliases) { Some("GPU copy or resolve overlaps guest upload bytes") }
                else if list_writes(&aliases) { Some("recorded write overlaps guest upload bytes") }
                else { None };
            if generation == crate::resource::alias_generation() {
                break (conflict?, Some(targets), Some(aliases));
            }
        }
    } else {
        ("guest upload extent unknown", None, None)
    };
    static SEEN: std::sync::LazyLock<Mutex<std::collections::HashSet<&'static str>>> = std::sync::LazyLock::new(Default::default);
    if SEEN.lock().unwrap_or_else(|e| e.into_inner()).insert(reason) {
        let copies = aliases.as_deref().map(crate::texture::copy_writer_intersections);
        tracing::warn!(logical, resource = format_args!("{object:#x}"), serial = copy.serial,
            flags = copy.flags(), ?span, targets = ?overlaps, ?copies, ?reason, "late sampled SRV requires produced guest bytes");
    }
    Some(reason)
}

fn requires_buffer(view: &View) -> bool {
    match view {
        View::Constant(address, size) => *address != 0 && *size != 0,
        View::Shader(r, _) | View::Unordered(r, _, _) => *r != 0 && view_elements(view).is_some(),
        View::Placed(_, resource, desc) => resource[0] == 1 && (desc[10] != 0 || desc[11] != 0),
        _ => false,
    }
}

fn view_elements(view: &View) -> Option<(u32, bool, u32)> {
    const BUFFER: u32 = 1;
    let (words, unordered): (&[u32; 10], bool) = match view {
        View::Shader(_, Some(d)) if d[1] == BUFFER => (d, false),
        View::Unordered(_, _, Some(d)) if d[1] == BUFFER => (d, true),
        View::Placed(unordered, resource, d) if resource[0] == BUFFER => {
            (d[..10].try_into().ok()?, *unordered)
        }
        _ => return None,
    };
    // SAFETY: buffer views' union members.
    Some(unsafe {
        if unordered {
            let d = uav_desc(words);
            let b = d.Anonymous.Buffer;
            (b.StructureByteStride, b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0, d.Format.0 as u32)
        } else {
            let d = srv_desc(words);
            let b = d.Anonymous.Buffer;
            (b.StructureByteStride, b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0, d.Format.0 as u32)
        }
    })
}

/// Whether a buffer view that begins `offset` bytes into its host copy begins
/// where no host view can: raw views begin on 16 bytes, others on a multiple
/// of their elements (`stride` of a structured one, which is made raw where it
/// can be, `element` of a typed one). The console's begin on any dword, and
/// the title puts parameter blocks at ring positions: a raw view of 128 bytes
/// at 8 (the colour table bakes'), structured ones of elements that are no
/// divisor of their place (the culling's 456-byte constants). Such a view gets
/// a copy of its own bytes ([`crate::shadow::exact`]). With no raw twin,
/// an untyped structured view also needs that copy to be 16-byte aligned.
/// A typed view of 12-byte elements also needs its own copy when its start
/// is a multiple of 16 but not a multiple of 12.
fn off_elements(offset: u64, stride: u32, raw: bool, element: u64, has_raw_twins: bool) -> bool {
    let needs_raw = stride > 0 && stride.is_multiple_of(4) && !has_raw_twins;
    if raw || needs_raw {
        !offset.is_multiple_of(16)
    } else if stride > 0 {
        !offset.is_multiple_of(16) && !offset.is_multiple_of(u64::from(stride))
    } else {
        element > 0 && !offset.is_multiple_of(element)
    }
}

/// Whether `view` has a raw twin when its host slot is made ([`raw_twins`]):
/// a buffer view or a constant buffer's.
fn twinned(view: &View) -> bool {
    matches!(view, View::Constant(..)) || view_elements(view).is_some()
}

/// Whether views off their copy's elements get a copy of their own bytes. On by
/// default; `VICTORY_VIEW_BYTES=0` leaves them null, as before.
fn view_bytes() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_BYTES").is_none_or(|v| v != "0"))
}

/// A dword-aligned raw or structured interval shares its writer's allocation.
/// The two spare stride bits encode the dwords before its first guest byte.
fn canonical_raw(offset: u64, bytes: u64, (stride, raw, format): (u32, bool, u32)) -> Option<(u64, u32, u32)> {
    if !victory_shader::emit::mapped_views() || victory_shader::emit::raw_views() != 0
        || !offset.is_multiple_of(4) || bytes == 0 || !bytes.is_multiple_of(4)
        || !(raw && stride == 0 && format == DXGI_FORMAT_R32_TYPELESS.0 as u32
            || !raw && format == 0 && stride != 0 && stride <= 2048 && stride.is_multiple_of(4))
    {
        return None;
    }
    let bias = (offset & 15) as u32;
    let width = u32::try_from(bytes).ok()?.checked_add(bias)?;
    Some(((offset & !15) / 4, width / 4, bias))
}

/// A native RGB32 float view and an exact raw twin on the same resource.
/// A 16-byte-aligned start can sit between two 12-byte elements. The
/// newer byte-offset API keeps the typed conversion and all selectors,
/// and the raw twin needs no driver-specific descriptor adjustment.
fn canonical_typed(
    offset: u64,
    bytes: u64,
    kind: (u32, bool, u32),
) -> Option<&'static super::byte_offset::Device15> {
    rgb32_window(offset, bytes, kind)?;
    // The native typed descriptor has the same format, selectors and
    // bounds in either heap layout. Only heaps with twins need a second
    // raw descriptor; its aligned byte range is checked above.
    super::byte_offset::device()
}

fn rgb32_window(offset: u64, bytes: u64, kind: (u32, bool, u32)) -> Option<(u64, u32)> {
    if kind != (0, false, DXGI_FORMAT_R32G32B32_FLOAT.0 as u32) || offset.is_multiple_of(12)
        || !offset.is_multiple_of(16) || bytes == 0 || !bytes.is_multiple_of(12) { return None; }
    Some((offset / 4, u32::try_from(bytes / 4).ok()?))
}

/// On mapped heaps a translated RGB32 load can read three raw dwords of
/// the original allocation. Its bound and identity selectors are exact.
fn mapped_rgb32(offset: u64, bytes: u64, kind: (u32, bool, u32), mapping: u32) -> Option<(u64, u32)> {
    let window = rgb32_window(offset, bytes, kind)?;
    (mapping == D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING && bytes <= u64::from(u32::MAX)
        && victory_shader::emit::mapped_views() && victory_shader::emit::raw_views() == 0
        && view_extent() && super::byte_offset::device().is_none()).then_some(window)
}

fn srv_mapping(view: &View) -> u32 {
    match view {
        View::Shader(_, Some(words)) => words[2],
        View::Placed(false, _, words) => words[2],
        _ => 0,
    }
}

fn raw_prefix(view: &View, backing: &mut ViewBacking) -> Option<u32> {
    let kind = view_elements(view)?;
    if !victory_shader::emit::mapped_views() || kind.0 == 0 && !kind.1 {
        return None;
    }
    let (start, bytes) = buffer_range(view)?;
    let (base, len) = buffer_extent(view)?;
    let shadow = backing.allocation(base, len)?;
    if shadow.offset_in(start).is_none() || bytes > shadow.left(start) {
        return None;
    }
    canonical_raw(shadow.offset(start), bytes, kind).map(|(_, _, bias)| bias)
}

/// The copy the buffer view of `[start, start + len)` is made on: `shadow`, the one
/// its buffer has, or one of the view's own bytes ([`off_elements`]).
fn view_home(
    shadow: std::sync::Arc<crate::shadow::Shadow>,
    start: u64,
    len: u64,
    (stride, raw, format): (u32, bool, u32),
    mapping: u32,
) -> std::sync::Arc<crate::shadow::Shadow> {
    let Some(offset) = shadow.offset_in(start).map(|_| shadow.offset(start)) else {
        return shadow;
    };
    if canonical_raw(offset, len, (stride, raw, format)).is_some()
        || canonical_typed(offset, len, (stride, raw, format)).is_some()
        || mapped_rgb32(offset, len, (stride, raw, format), mapping).is_some() || !view_bytes() {
        return shadow;
    }
    let element = crate::resource::element(format).map_or(0, |(bytes, _)| bytes);
    // Pick the same copy when creating the descriptor and when refreshing it,
    // even if the latter lookup comes before the title creates its view heap.
    if stride > 0 {
        probe_view_limit();
    }
    let has_raw_twins = victory_shader::emit::raw_views() != 0;
    if !off_elements(offset, stride, raw, element, has_raw_twins) {
        return shadow;
    }
    if len > shadow.left(start) {
        return shadow;
    }
    crate::shadow::exact(&shadow, start, len).unwrap_or(shadow)
}

/// For device-removed reports: what the slots of the title's view heap that
/// the words of user data may name hold now ([`crate::host::indirects`]).
#[cfg(feature = "diagnostics")]
pub(crate) fn describe_slots(words: &[u32]) -> Vec<(usize, u32, String)> {
    super::with_view_heap(|host| {
        crate::host::indirects::slots(words, host.count, 6)
            .into_iter()
            .take(24)
            .map(|(word, slot)| (word, slot, host.describe_slot(slot).unwrap_or_default()))
            .collect()
    })
    .unwrap_or_default()
}

/// For `crate::book`: the bytes of up to `n` descriptors of the view heap from
/// slot `at` on (from the first twin on, where `at` lies before it and past
/// the others), as the card's heap has them: the first slot, the bytes, and
/// the slot behind the last one worth reading.
#[cfg(feature = "diagnostics")]
pub(crate) fn shown_bytes(at: u32, n: u32) -> Option<(u32, Vec<u8>, u32)> {
    super::with_view_heap(|host| {
        let own = host.count + host.dynamic;
        let end = if host.raw == 0 { own } else { host.raw + own };
        let at = if at >= own && at < host.raw { host.raw } else { at };
        let stop = if at < own { own } else { end };
        let n = n.min(stop.saturating_sub(at));
        if n == 0 || host.step != 32 {
            return None;
        }
        let mut bytes = vec![0u8; n as usize * 32];
        crate::host::read_into(host.cpu(at).ptr as *const u8, &mut bytes).then_some((at, bytes, end))
    })?
}

/// For `crate::book`: what our books say of a slot of the view heap.
#[cfg(feature = "diagnostics")]
pub(crate) fn slot_story(slot: u32) -> String {
    use std::sync::atomic::Ordering::Relaxed;
    use windows::core::Interface;
    super::with_view_heap(|host| {
        let (index, twin) = if host.raw != 0 && slot >= host.raw { (slot - host.raw, true) } else { (slot, false) };
        let mut text = if index < host.count {
            format!("title slot {index}, made from {}", host.describe_slot(index).unwrap_or_default())
        } else {
            let taken = host.next.lock().unwrap_or_else(|e| e.into_inner()).1.get((index - host.count) as usize).copied();
            format!("dynamic slot {} (taken {taken:?})", index - host.count)
        };
        if twin {
            let made = host.twins.get(index as usize).map(|t| t.load(Relaxed));
            text.push_str(&format!(", its raw twin (made {made:?})"));
        }
        if let Some(at) = host.written.get(slot as usize) {
            text.push_str(&format!(", written last at {:.1} s", f64::from(at.load(Relaxed)) / 1e3));
        }
        let before = BEFORE.lock().unwrap_or_else(|e| e.into_inner()).get(&index).copied();
        if let Some((was, at, (started, finished), by)) = before {
            let what = match view_at(was as u32, (was >> 32) as u32) {
                _ if was == 0 => "nothing (or the neutral texture)".into(),
                None => "a view that is gone".into(),
                Some(view) => format!("{view:?}").chars().take(160).collect::<String>(),
            };
            text.push_str(&format!(
                ", changed ({by}) at {:.3} s with submissions {started} begun and {finished} finished, showed before: {what}",
                at as f64 / 1e3
            ));
        }
        let kept = host.kept_in(index);
        match kept {
            None => text.push_str(", holds nothing"),
            Some(resource) => {
                // SAFETY: a resource the slot holds.
                let d = unsafe { resource.GetDesc() };
                let gpu = if d.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
                    // SAFETY: as above.
                    unsafe { resource.GetGPUVirtualAddress() }
                } else {
                    0
                };
                text.push_str(&format!(
                    ", holds {:#x}: dimension {} {}x{}x{} levels {} f{} at {gpu:#x}, {} references",
                    resource.as_raw() as usize,
                    d.Dimension.0,
                    d.Width,
                    d.Height,
                    d.DepthOrArraySize,
                    d.MipLevels,
                    d.Format.0,
                    crate::host::references(&resource)
                ));
            }
        }
        text
    })
    .unwrap_or_default()
}

/// Host heaps' GPU ranges (start, bytes, bytes per slot, sampler), for
/// device-removed reports: a shader indexing a heap past its end faults.
static GPU_RANGES: Mutex<Vec<(u64, u64, u64, bool)>> = Mutex::new(Vec::new());

/// For device-removed reports: where host GPU address `va` lies against
/// the host descriptor heaps.
pub(crate) fn near_heaps(va: u64) -> String {
    let ranges = GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for &(start, bytes, step, sampler) in ranges.iter() {
        let kind = if sampler { "sampler" } else { "view" };
        let place = if va < start {
            format!("{:#x} below", start - va)
        } else if va < start + bytes {
            format!("inside, slot {}", (va - start) / step)
        } else {
            format!("{:#x} past its end (slot {})", va - start - bytes, (va - start) / step)
        };
        text.push_str(&format!("; {kind} heap {start:#x}+{bytes:#x}: {place}"));
    }
    text
}

/// The host's limit for shader-visible view heaps on resource binding
/// tiers 1 and 2; tier 3 hardware may take more.
const PORTABLE_VIEWS: u32 = 1_000_000;

/// The size of every host view heap on a host that takes more than
/// [`PORTABLE_VIEWS`] (AMD): the title's heap (a little over 1,000,000
/// views in the retail game) plus [`DYNAMIC`] fits with room to spare, and
/// the last slot is the null slot every view index at or past it reads.
const UNCAPPED_VIEWS: u32 = 1 << 21;

/// Where the raw twins of the title's slots start in a heap that has them
/// ([`raw_twins`]): past every slot a title's heap and the dynamic ones take.
const RAW_TWINS: u32 = 1 << 21;

/// Whether view heaps keep a raw twin of every buffer view of the title's
/// slots, which shaders read that pick buffers by an index they compute
/// (`victory_shader::emit::raw_views` says why; `VICTORY_RAW_VIEWS=0`: they
/// read the title's slots themselves, as before). Needs a host that takes
/// heaps of 2^22 views.
fn raw_twins() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RAW_VIEWS").is_none_or(|v| v != "0"))
}

/// Picks, once, the null slot at the end of every host view heap and tells
/// the translator now (`set_view_heap_limit`) instead of when the title's
/// heap is made: a shader keeps the limit it was translated with, and the
/// title makes 25 pipelines (their shaders translated at once) before it
/// makes its view heap. Called before a shader is translated and before a
/// view heap is made.
///
/// Every host clamps. A host that caps view heaps at [`PORTABLE_VIEWS`]
/// (NVIDIA) hung on a view index past the heap; one that does not (AMD
/// RDNA3, RX 7900 XT) took a page fault and removed the device on entering
/// the world: a compute program read view slot 16,777,088 (0xffff80) of a
/// heap of 1,507,328, past its end. Unclamped, that index is undefined on
/// any host.
pub(crate) fn probe_view_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(host) = host() else { return };
        let fits = |count: u32| {
            // SAFETY: plain heap creation; the heap is released at once.
            let made: windows::core::Result<ID3D12DescriptorHeap> = unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            };
            made.is_ok()
        };
        let twins = raw_twins() && fits(2 * RAW_TWINS);
        if twins {
            victory_shader::emit::set_raw_views(RAW_TWINS);
        }
        // (The twins lie past the null slot.)
        let views = if twins {
            RAW_TWINS
        } else if fits(UNCAPPED_VIEWS) {
            UNCAPPED_VIEWS
        } else {
            PORTABLE_VIEWS
        };
        tracing::info!(
            views,
            raw_twins = twins,
            "host view heaps hold {views} views; view indices are clamped to the null slot at the end from the first shader on"
        );
        victory_shader::emit::set_view_heap_limit(views - 1);
    });
}

/// Slots past the title's in a view heap, for tables a draw needs made
/// afresh (constant buffers copied per draw). A list holds the slots it
/// took until it is reset (its submissions have run) or released.
/// 2^16 ran out in the world ("no free dynamic descriptor slots", run 11;
/// upstream PR #336 takes 2^18). On a host heap capped at 1,000,000 views
/// (NVIDIA) these come out of the title's slots, whose highest seen is
/// about 720,000: 2^17 left 868,000 of them, and ran out in the opening
/// cutscene (a frame asks for up to 16,000 tables while the frame before
/// still holds its own; a draw refused its table read slots never made
/// for it, and the card stopped within frames). 2^18 leaves 737,000; a
/// table past them is made afresh in these slots like any other.
pub(crate) const DYNAMIC: u32 = victory_shader::emit::MAPPED_DYNAMIC;

/// [`DYNAMIC`] for a heap of `count` title slots whose null slot is `null`
/// (0: none): up to four times as many where the host heap has the room (a
/// host that takes more than [`PORTABLE_VIEWS`]; `VICTORY_DYNAMIC_SLOTS`
/// sets the most). 2^18 ran out in the title's bigger places (session
/// play-1125-0127: 27,000 to 35,000 tables a frame, up to 3,285 of them
/// refused, with 2,639 indirect draws and dispatches).
fn dynamic_slots(count: u32, null: u32) -> u32 {
    static MOST: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let most = *MOST.get_or_init(|| {
        std::env::var("VICTORY_DYNAMIC_SLOTS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DYNAMIC << 2)
    });
    if null < PORTABLE_VIEWS {
        return DYNAMIC;
    }
    null.saturating_sub(count).min(most).max(DYNAMIC)
}

impl HostHeap {
    pub(crate) fn new(sampler: bool, count: u32) -> Option<Self> {
        let host = host()?;
        let kind = if sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        let make = |count: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            // SAFETY: plain heap creation.
            unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            }
        };
        // A view heap ends at the null slot shaders clamp view indices to
        // (`probe_view_limit`), whatever the title's own count: the title's
        // slots, then the dynamic ones, then null up to and with that slot.
        // Sampler heaps always back the full 2,048 hardware limit and reserve
        // slot 2,047 as a permanent null point sampler so `umin(index, 2047)`
        // in `victory-shader` never reads past smaller title sampler heaps.
        const SAMPLERS: u32 = 2048;
        if !sampler {
            probe_view_limit();
        }
        let null = if sampler {
            SAMPLERS - 1
        } else {
            victory_shader::emit::view_heap_limit()
        };
        let raw = if sampler { 0 } else { victory_shader::emit::raw_views() };
        let extra = if sampler { 0 } else { dynamic_slots(count, null) };
        let (heap, fitted, size) = if sampler {
            let fitted = count.min(SAMPLERS - 1);
            match make(SAMPLERS) {
                Ok(heap) => (heap, fitted, SAMPLERS),
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        } else if null == 0 {
            match make(count + extra) {
                Ok(heap) => (heap, count, count + extra),
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        } else {
            let size = if raw == 0 { null + 1 } else { 2 * raw };
            let fitted = if count.saturating_add(extra) <= null {
                count
            } else {
                null - extra
            };
            match make(size) {
                Ok(heap) => (heap, fitted, size),
                Err(e) => {
                    tracing::warn!(error = %e, count, size, "no host view heap");
                    return None;
                }
            }
        };
        let mapped = !sampler && victory_shader::emit::mapped_views();
        let count = if mapped { count } else { fitted };
        if mapped && count > victory_shader::emit::SLOT_STRIDES_COUNT {
            tracing::warn!(count, "logical view heap exceeds the descriptor mapping");
            return None;
        }
        // SAFETY: a live heap.
        let (cpu, step) = unsafe {
            (
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                host.device.GetDescriptorHandleIncrementSize(kind),
            )
        };
        tracing::debug!(count, sampler, "host descriptor heap");
        // SAFETY: a live shader-visible heap.
        let gpu = unsafe { heap.GetGPUDescriptorHandleForHeapStart().ptr };
        GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner()).push((
            gpu,
            u64::from(size) * u64::from(step),
            u64::from(step),
            sampler,
        ));
        let strides = (!sampler)
            .then(|| {
                // The strides, then the views' title addresses
                // (`victory_shader::emit::VIEW_ADDRESSES`).
                let bytes = u64::from(if mapped {
                    victory_shader::emit::VIEW_MAPPING_BYTES
                } else {
                    victory_shader::emit::SLOT_STRIDES_BYTES
                });
                let (buffer, data) = host.upload_buffer(bytes)?;
                // SAFETY: the buffer's whole mapping; a live buffer.
                let gpu = unsafe {
                    std::ptr::write_bytes(data, 0, bytes as usize);
                    if mapped {
                        let words = data
                            .add(victory_shader::emit::VIEW_MAPPING as usize)
                            .cast::<u32>();
                        words.write(count);
                        words.add(1).write(fitted);
                        let map = std::slice::from_raw_parts_mut(
                            words.add(2),
                            victory_shader::emit::SLOT_STRIDES_COUNT as usize,
                        );
                        map.fill(null);
                        if fitted == count {
                            for (i, slot) in map[..count as usize].iter_mut().enumerate() {
                                *slot = i as u32;
                            }
                        }
                    }
                    buffer.GetGPUVirtualAddress()
                };
                crate::host::name(&buffer, || "element strides by view heap slot".to_owned());
                Some((buffer, gpu, data as usize))
            })
            .flatten();
        if mapped && strides.is_none() {
            return None;
        }
        let queued = std::sync::Arc::new(Queued::new(if sampler { 0 } else { count }));
        let h = HostHeap {
            flip_identity: Default::default(),
            retired_runs: Default::default(),
            has_retired_runs: Default::default(),
            changes_seen: Default::default(),
            last_reader: Default::default(),
            publication: Default::default(),
            candidate_observations: Default::default(),
            #[cfg(feature = "diagnostics")]
            publication_stats: Default::default(),
            buffer_sweep: Mutex::default(),
            buffer_frame: std::sync::atomic::AtomicU64::new(u64::MAX),
            heap,
            cpu,
            step,
            count,
            dynamic: extra,
            scratch_base: fitted,
            assigned: (0..if count > fitted { count } else { 0 })
                .map(|_| std::sync::atomic::AtomicU32::new(null))
                .collect(),
            next_title: Default::default(),
            free_titles: Default::default(),
            title_readers: Default::default(),
            evict_cursor: Default::default(),
            failed_buffers: Queued::new(count + extra),
            candidates: CandidateIndex::new(count),
            superseded: RetiredRecords::new(count),
            indexed_addresses: Default::default(),
            ordered_reads: Default::default(),
            sampler,
            raw,
            twins: (0..if raw == 0 { 0 } else { count + extra }).map(|_| std::sync::atomic::AtomicBool::new(false)).collect(),
            #[cfg(feature = "diagnostics")]
            written: (0..if sampler || !crate::book::on() { 0 } else { size })
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect(),
            made: Made::new(count as usize),
            quick: (0..count).map(|_| std::sync::atomic::AtomicU64::new(0)).collect(),
            copied: Mutex::new(Vec::new()),
            waiting: Mutex::default(),
            away: Mutex::default(),
            kept: (0..count + extra).map(|_| Mutex::new(None)).collect(),
            next: Mutex::new((0, vec![false; extra as usize])),
            free: std::sync::atomic::AtomicU32::new(extra),
            too_long: std::sync::atomic::AtomicU32::new(u32::MAX),
            strides,
            buffers: Slots::new(count as usize),
            changed: Slots::new(count as usize),
            textures: Mutex::new(TextureQueue { queued: queued.clone(), ..Default::default() }),
            queued,
            tabled: (0..if sampler { 0 } else { count })
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect(),
            placed: {
                let spans = std::sync::Arc::<Mutex<Spans>>::default();
                if !sampler {
                    PLACED_SPANS
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(std::sync::Arc::downgrade(&spans));
                }
                spans
            },
            spanned: Slots::new((count + extra) as usize),
        };
        // A slot never written is undefined: shaders reading one (a slot
        // the title wrote in a way we do not know, or a table entry no
        // used-resource entry names) read a wild address. Every slot starts
        // null instead.
        let started = std::time::Instant::now();
        if let Err(e) = h.fill_null(kind, size) {
            tracing::warn!(error = %e, "host descriptor heap not made null");
        }
        tracing::debug!(ms = started.elapsed().as_millis(), "host descriptor heap made null");
        Some(h)
    }

    /// Makes the first `total` slots null: one null slot in a CPU-side heap,
    /// doubled by copies to a block, the block copied over the heap.
    fn fill_null(&self, kind: D3D12_DESCRIPTOR_HEAP_TYPE, total: u32) -> windows::core::Result<()> {
        const BLOCK: u32 = 1 << 16;
        let Some(host) = host() else { return Ok(()) };
        // SAFETY: plain heap creation and copies inside live heaps.
        unsafe {
            let block: ID3D12DescriptorHeap =
                host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: BLOCK,
                    ..Default::default()
                })?;
            let start = block.GetCPUDescriptorHandleForHeapStart();
            let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + (i * self.step) as usize,
            };
            // (The twins are null buffer views.)
            let ends = if self.raw == 0 { [total, total] } else { [self.raw.min(total), total] };
            let mut done = 0;
            for (twin, end) in ends.into_iter().enumerate() {
                if done == end {
                    continue;
                }
                if self.sampler {
                    null_sampler(start);
                } else if twin == 1 {
                    null_buffer_view(start);
                } else {
                    null_view(start);
                }
                let mut filled = 1;
                while filled < BLOCK {
                    let n = filled.min(BLOCK - filled);
                    host.device.CopyDescriptorsSimple(n, at(filled), start, kind);
                    filled += n;
                }
                while done < end {
                    let n = BLOCK.min(end - done);
                    host.device
                        .CopyDescriptorsSimple(n, self.physical_cpu(done), start, kind);
                    done += n;
                }
            }
        }
        Ok(())
    }

    /// A plain point sampler at slot `index`, for sampler slots without a
    /// view.
    fn null_sampler(&self, index: u32) {
        null_sampler(self.at(index));
        self.put(index);
    }
}

/// A plain point sampler at `at`.
fn null_sampler(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        ..Default::default()
    };
    // SAFETY: a slot of a sampler heap of ours.
    unsafe { host.device.CreateSampler(&desc, at) };
}

impl HostHeap {
    /// `n` consecutive free dynamic slots (view heaps only), taken until
    /// [`HostHeap::release`]: the first's index.
    pub(crate) fn dynamic(&self, n: u32) -> Option<u32> {
        if self
            .has_retired_runs
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.reclaim_runs();
        }
        if let Some(first) = self.allocate_run(n) {
            return Some(first);
        }
        // Only leases relinquished by their recording can be recovered by
        // waiting. A live replayable recording retains its allocation.
        loop {
            let fence = self
                .retired_runs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|&(after, _, _)| after)
                .min()?;
            crate::host::wait_for_descriptor_reader(fence);
            self.reclaim_runs();
            if let Some(first) = self.allocate_run(n) {
                return Some(first);
            }
        }
    }

    fn reclaim_runs(&self) {
        let completed = crate::host::completed_descriptor_reader();
        let mut retired = self.retired_runs.lock().unwrap_or_else(|e| e.into_inner());
        retired.retain(|&(after, first, n)| {
            if after > completed {
                return true;
            }
            self.free_run(first, n);
            false
        });
        self.has_retired_runs
            .store(!retired.is_empty(), std::sync::atomic::Ordering::Release);
    }

    fn allocate_run(&self, n: u32) -> Option<u32> {
        if self.sampler || n == 0 || n > self.dynamic {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::DynamicAsked, 1);
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let (next, taken) = &mut *guard;
        let enough = self.free.load(std::sync::atomic::Ordering::Relaxed) >= n
            && n < self.too_long.load(std::sync::atomic::Ordering::Relaxed);
        let n = n as usize;
        // Search from `next` on, then from the start (extending past cursor by
        // n - 1 so contiguous runs crossing the cursor are not missed).
        let cursor = *next as usize;
        let end = self.dynamic as usize;
        let second_end = cursor.saturating_add(n.saturating_sub(1)).min(end);
        let starts = (cursor..end).chain(0..second_end);
        let mut run = 0;
        let mut found = None;
        let mut last = usize::MAX;
        for at in starts.take(if enough { usize::MAX } else { 0 }) {
            // A run cannot wrap past the end.
            if at != last.wrapping_add(1) {
                run = 0;
            }
            last = at;
            run = if taken[at] { 0 } else { run + 1 };
            if run == n {
                found = Some(at + 1 - n);
                break;
            }
        }
        let Some(first) = found else {
            if enough {
                self.too_long.fetch_min(n as u32, std::sync::atomic::Ordering::Relaxed);
            }
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                let longest = taken.split(|&busy| busy).map(|run| run.len()).max().unwrap_or(0);
                tracing::warn!(n, total=self.dynamic, free=self.free.load(std::sync::atomic::Ordering::Relaxed),
                    largest_free=longest, "no free dynamic descriptor slots");
            });
            #[cfg(feature = "diagnostics")]
            {
                use crate::copystat::{C, add};
                add(C::DynamicRefused, 1);
                add(C::DynamicNs, started.elapsed().as_nanos() as u64);
            }
            return None;
        };
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::DynamicNs, started.elapsed().as_nanos() as u64);
        taken[first..first + n].fill(true);
        self.free.fetch_sub(n as u32, std::sync::atomic::Ordering::Relaxed);
        *next = ((first + n) % end) as u32;
        if victory_shader::emit::mapped_views()
            && let Some((_, _, data)) = &self.strides
        {
            // The root can only index the contiguous run allocated for it.
            unsafe {
                (*data as *mut u8)
                    .add(victory_shader::emit::FRESH_TABLE_SIZES as usize)
                    .cast::<u32>()
                    .add(first)
                    .write_volatile(n as u32);
            }
        }
        Some(self.count + first as u32)
    }

    fn publish_views(&self, _cause: PublicationCause) -> Option<std::sync::RwLockReadGuard<'_, u64>> {
        let (pin, pass) = self.begin_publication(_cause)?;
        if let Some(pass) = pass { pass.note(false); }
        Some(pin)
    }

    fn begin_publication(
        &self, _cause: PublicationCause,
    ) -> Option<(std::sync::RwLockReadGuard<'_, u64>, Option<std::sync::Arc<CandidatePass>>)> {
        use std::sync::atomic::Ordering::{Acquire, Release};
        if self.sampler { return None; }
        let guard = self.last_reader.read().unwrap_or_else(|e| e.into_inner());
        if *guard != 0 {
            crate::host::wait_for_descriptor_reader(*guard);
        }
        let pass = self.candidate_observations.watching.load(Acquire).then(||
            self.candidate_observations.active.lock().unwrap_or_else(|e| e.into_inner()).clone()).flatten();
        self.publication.fetch_add(1, Release);
        #[cfg(feature = "diagnostics")]
        self.publication_stats.note(_cause);
        Some((guard, pass))
    }

    fn publish_mirror(&self, mirror: MirrorPublication<'_>) -> Option<MirrorGuard<'_>> {
        let (pin, pass) = self.begin_publication(PublicationCause::MirrorView)?;
        let note = pass.map(|pass| {
            let (handle, key) = (mirror.handle, mirror.key);
            let impact = self.mirror_impact(&pass, mirror);
            MirrorNote { pass, impact, handle, key, installed: false }
        });
        Some(MirrorGuard { note, _pin: pin })
    }

    /// Made and the shared publication pin are held. Only ordinary sampled
    /// views can use the new proof: placing a view may supersede other slots.
    fn mirror_impact(&self, pass: &CandidatePass, mirror: MirrorPublication<'_>) -> MirrorImpact {
        use MirrorReason::*;
        let MirrorPublication { slot, shown, handle, key, view } = mirror;
        let new = indexed_form(view);
        let mut old = 0;
        let span_bit = 1 << (slot % 64);
        let isolated = self.spanned.0.get(slot as usize / 64).is_some_and(|bits|
            bits.load(std::sync::atomic::Ordering::Acquire) & span_bit == 0)
            && !self.superseded.may(slot) && !self.unmapped(slot);
        let reason = if !pass.may_ignore_slot(slot) {
            Requested
        } else if !isolated {
            SpanOrUnmapped
        } else if shown == 0 {
            UnknownOld
        } else if let Some(previous) = view_at(shown as u32, (shown >> 32) as u32) {
            old = indexed_form(&previous);
            let proposed = if pass.disjoint_buffer_form(old) && pass.disjoint_buffer_form(new) {
                if self.candidates.disjoint_buffer_at(slot, pass) { Buffer } else { CandidateChanged }
            } else if old == 8 && new == 8 && matches!((&previous, view), (View::Shader(..), View::Shader(..))) {
                let old_kind = texture_kind(&previous).map_or(0, |(_, kind)| kind);
                let new_kind = texture_kind(view).map_or(0, |(_, kind)| kind);
                let reason = pass.sampled_reason(slot, old_kind, new_kind);
                if !reason.sampled() { reason }
                else if !self.candidates.sampled_record_at(slot, shown, old_kind) { CandidateChanged }
                else if !self.quick.get(slot as usize).is_some_and(|record|
                    record.load(std::sync::atomic::Ordering::Acquire) == shown) { Pending }
                else { reason }
            } else {
                CompatibleOrUnknown
            };
            if proposed.disjoint() && record_key(handle) != key { ChangedRecord } else { proposed }
        } else {
            UnknownOld
        };
        MirrorImpact { slot, old, new, reason }
    }

    pub(crate) fn publication_version(&self) -> u64 {
        self.publication.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Wait for all publishers already inside their shared critical section.
    pub(crate) fn stable_version(&self) -> u64 {
        self.stable_publication(|_| ()).0
    }

    fn stable_publication<R>(&self, observe: impl FnOnce(u64) -> R) -> (u64, R) {
        #[cfg(feature = "diagnostics")]
        let report = self.publication_stats.report_due();
        let alone = self.last_reader.write().unwrap_or_else(|e| e.into_inner());
        let version = self.publication_version();
        let observation = observe(version);
        #[cfg(feature = "diagnostics")]
        let counts = report.then(|| {
            use std::sync::atomic::Ordering::Relaxed;
            (std::array::from_fn(|i| self.publication_stats.cause[i].load(Relaxed)), self.publication_stats.indexed.load(Relaxed))
        });
        drop(alone);
        #[cfg(feature = "diagnostics")]
        if let Some((counts, indexed)) = counts {
            PublicationStats::report(self, version, counts, indexed);
        }
        (version, observation)
    }

    pub(crate) fn begin_candidate_observation(
        &self, slots: &IndexedSlots, selected: Option<&std::collections::BTreeSet<u32>>,
    ) -> CandidateObservation<'_> {
        let pass = CandidatePass::new(slots, selected).map(std::sync::Arc::new);
        self.stable_publication(|before| {
            let mut observation = CandidateObservation { heap: self, before, pass: None };
            if let Some(pass) = pass {
                let mut active = self.candidate_observations.active.lock().unwrap_or_else(|e| e.into_inner());
                if active.is_none() {
                    *active = Some(pass.clone());
                    self.candidate_observations.watching.store(true, std::sync::atomic::Ordering::Release);
                    observation.pass = Some(pass);
                }
            }
            observation
        }).1
    }

    /// Called with the exclusive publication pin, including on unwind.
    fn stop_candidate_observation(&self, pass: &std::sync::Arc<CandidatePass>) {
        let mut active = self.candidate_observations.active.lock().unwrap_or_else(|e| e.into_inner());
        if active.as_ref().is_some_and(|current| std::sync::Arc::ptr_eq(current, pass)) {
            *active = None;
            self.candidate_observations.watching.store(false, std::sync::atomic::Ordering::Release);
        }
    }

    /// Convert an internal title or list slot to its descriptor heap location.
    pub(crate) fn physical(&self, index: u32) -> u32 {
        if self.assigned.is_empty() {
            return index;
        }
        if let Some(entry) = self.assigned.get(index as usize) {
            entry.load(std::sync::atomic::Ordering::Acquire)
        } else {
            self.scratch_base + (index - self.count)
        }
    }

    /// Called under the logical slot's made lock. Numbering stays valid for
    /// the heap's lifetime, including lists recorded before a rewrite.
    fn assign_title(&self, index: u32) {
        use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
        let Some(entry) = self.assigned.get(index as usize) else {
            return;
        };
        let null = victory_shader::emit::view_heap_limit();
        if entry.load(Acquire) != null {
            return;
        }
        let recycled = self
            .free_titles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop();
        let Some(physical) = recycled.or_else(|| {
            self.next_title
                .fetch_update(Relaxed, Relaxed, |next| {
                    (next < self.scratch_base).then_some(next + 1)
                })
                .ok()
        }) else {
            self.candidates.pending(index, true);
            static WARN: std::sync::Once = std::sync::Once::new();
            WARN.call_once(|| {
                tracing::warn!(
                    index,
                    slots = self.scratch_base,
                    "indexed descriptor candidates exceed the resident arena"
                )
            });
            return;
        };
        self.candidates.pending(index, false);
        let (_, _, data) = self.strides.as_ref().expect("mapped view heap");
        // SAFETY: mapped and checked at heap creation, under the slot lock.
        unsafe {
            let words = (*data as *mut u8)
                .add(victory_shader::emit::VIEW_MAPPING as usize)
                .cast::<u32>();
            words.add(2 + index as usize).write_volatile(physical);
        }
        entry.store(physical, Release);
    }

    /// The slot has lost its published descriptor. The publication guard
    /// has retired all earlier native readers before its storage is reused.
    fn release_title(&self, logical: u32) {
        let Some(entry) = self.assigned.get(logical as usize) else {
            return;
        };
        self.candidates.pending(logical, false);
        let null = victory_shader::emit::view_heap_limit();
        let physical = entry.swap(null, std::sync::atomic::Ordering::AcqRel);
        if physical == null {
            return;
        }
        let (_, _, data) = self.strides.as_ref().expect("mapped view heap");
        unsafe {
            let words = (*data as *mut u8)
                .add(victory_shader::emit::VIEW_MAPPING as usize)
                .cast::<u32>();
            words.add(2 + logical as usize).write_volatile(null);
        }
        self.free_titles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(physical);
    }

    fn unmapped(&self, index: u32) -> bool {
        !self.assigned.is_empty() && self.physical(index) == victory_shader::emit::view_heap_limit()
    }

    pub(crate) fn sparse(&self) -> bool {
        !self.assigned.is_empty()
    }

    pub(crate) fn pin_titles(&self, id: u64, slots: &IndexedSlots) {
        // Eviction needs only the logical slots and view types. The closed
        // command list already owns the material buffers used for selection.
        self.title_readers.lock().unwrap_or_else(|e| e.into_inner())
            .entry(id).or_default().merge_forms(slots);
    }

    pub(crate) fn unpin_titles(&self, id: u64) {
        self.title_readers.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    }

    /// Recycle a speculative title mirror only when no live recording could
    /// read it. Already executing readers must also have retired. The logical
    /// record stays in the guest heap and can be rematerialized on demand.
    pub(crate) fn room_for_title(&self, heap: &super::Heap, slot: u32) {
        use std::sync::atomic::Ordering::Relaxed;
        if !self.unmapped(slot) || self.next_title.load(Relaxed) < self.scratch_base
            || !self.free_titles.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
        {
            return;
        }
        let start = self.evict_cursor.load(Relaxed);
        for offset in 0..self.count {
            let victim = (start + offset) % self.count;
            if victim == slot || self.unmapped(victim) { continue; }
            let _made = self.made.lock(victim);
            if self.unmapped(victim) { continue; }
            let Some(handle) = heap.handle(victim) else { continue };
            let view = super::view(handle);
            let form = view.as_ref().map_or(0, |v| indexed_form(v) | if view_span(v).is_some() { 32 } else { 0 });
            let current = !self.superseded_record(victim, || record_key(handle));
            let pinned = |readers: &std::collections::HashMap<u64, IndexedSlots>| {
                readers.values().any(|r| r.exact.contains_key(&victim) || (current && r.opaque & form != 0
                    && view.as_ref().is_some_and(|v| r.accepts_opaque_texture(v))))
            };
            if pinned(&self.title_readers.lock().unwrap_or_else(|e| e.into_inner())) { continue; }
            let _publication = self.publish_views(PublicationCause::Eviction);
            let readers = self.title_readers.lock().unwrap_or_else(|e| e.into_inner());
            if pinned(&readers) { continue; }
            self.quick_is(victim, 0);
            self.made.set(victim, 0);
            self.release_title(victim);
            self.candidates.pending(victim, true);
            self.keep(victim, None);
            self.evict_cursor.store((victim + 1) % self.count, Relaxed);
            break;
        }
    }

    fn prove_plain_material(&self, slot: u32, record: u64, topology: u64, view: &View) -> Option<PlainMaterial> {
        let View::Placed(false, resource, desc) = view else { return None };
        if !matches!(resource[8], 70..=84 | 94..=99) { return None; }
        let at = u64::from(desc[10]) | u64::from(desc[11]) << 32;
        let ranges = crate::resource::unproduced_material_ranges(usize::try_from(at).ok()?, resource)?;
        if topology != crate::resource::alias_generation() { return None; }
        let proof = PlainMaterial { record, topology, ranges: ranges.into(), binding: Default::default() };
        self.candidates.mark_plain(slot, &proof);
        Some(proof)
    }

    pub(crate) fn indexed_resident(
        &self,
        heap: &super::Heap,
        slots: &IndexedSlots,
        list: &crate::host::HostList,
        recording: bool,
        mut late: Option<&mut crate::host::LateSamples>,
        sampled_slots: Option<&std::collections::BTreeSet<u32>>,
        stage_scopes: Option<(&crate::host::StageSelections, &crate::host::StageSelections)>,
    ) -> bool {
        let stages = stage_scopes.map(|s| s.0);
        let uav_stages = stage_scopes.map(|s| s.1);
        if slots.empty() {
            return true;
        }
        #[cfg(feature = "diagnostics")]
        #[derive(Default)]
        struct Visits {
            catalogue: u64,
            requested: u64,
            was_preparing: bool,
        }
        #[cfg(feature = "diagnostics")]
        impl Drop for Visits {
            fn drop(&mut self) {
                IN_INDEXED_PREPARATION.set(self.was_preparing);
                crate::host::note_candidate_slots(self.catalogue, self.requested);
            }
        }
        #[cfg(feature = "diagnostics")]
        let mut visits = (!recording && crate::host::gpu_progress()).then(|| Visits {
            was_preparing: IN_INDEXED_PREPARATION.replace(true),
            ..Visits::default()
        });
        let reject = |logical: u32, reason: &'static str| {
            static SAID: std::sync::LazyLock<Mutex<std::collections::HashSet<&'static str>>> =
                std::sync::LazyLock::new(Default::default);
            let first = SAID.lock().unwrap_or_else(|e| e.into_inner()).insert(reason);
            #[cfg(feature = "diagnostics")]
            let detailed = {
                use crate::host::IndexedFailureTrace;
                static TRACE: IndexedFailureTrace<(&str, u32, bool)> = IndexedFailureTrace::new();
                TRACE.report((reason, logical, recording))
            };
            #[cfg(not(feature = "diagnostics"))]
            let detailed = false;
            if first || detailed {
                let view = heap.handle(logical).and_then(super::view);
                tracing::warn!(logical, reason, recording, opaque = slots.opaque, words_only = slots.words_only,
                    selection = ?sampled_slots.map(|slots| slots.len()),
                    exact = ?slots.exact.get(&logical), made = ?self.made.get(logical), ?view,
                    "indexed descriptor unavailable");
            }
            false
        };
        // Snapshots share their sorted slots across recordings. A raw guest memcpy
        // can change a slot without an API write, so its record is still checked
        // before reusing either its published class or its plain-material proof.
        let selected_only = slots.opaque == 8 && sampled_slots.is_some();
        let omit_sampled = slots.opaque & 8 == 0 || sampled_slots.is_some();
        let snapshots: [_; 256] = std::array::from_fn(|i| {
            if slots.opaque == 0 || selected_only { return None; }
            let shard = self.candidates.shards.get(i)?.read().unwrap_or_else(|e| e.into_inner());
            Some(if omit_sampled { shard.without_sampled() } else { shard.snapshot() })
        });
        let mut candidates = snapshots.iter().flatten().flat_map(|s| s.iter()).filter(|row| {
            let omitted = row.texture == 8 && sampled_slots.is_some_and(|s| !s.contains(&row.slot));
            (row.pending || slots.opaque & row.texture != 0) && !omitted
                || row.buffer != 0 && (slots.opaque & row.buffer != 0 || slots.opaque & 32 != 0)
        }).peekable();
        let mut selected = sampled_slots.filter(|_| slots.opaque & 8 != 0).into_iter().flatten().copied().peekable();
        let mut exact = slots.exact.keys().copied().peekable();
        let recorded_writes = std::cell::OnceCell::new();
        let may_defer = recording && slots.opaque & 8 != 0 && crate::texture::PICKING.get().is_none();
        let mut ready_materials = 0u32;
        loop {
            let Some(logical) = [candidates.peek().map(|r| r.slot), selected.peek().copied(), exact.peek().copied()]
                .into_iter().flatten().min() else { break };
            let row = candidates.next_if(|r| r.slot == logical);
            let _selected_slot = selected.next_if_eq(&logical).is_some();
            let is_exact = exact.next_if_eq(&logical).is_some();
            #[cfg(feature = "diagnostics")]
            if let Some(visits) = &mut visits {
                if is_exact || _selected_slot { visits.requested += 1; }
                else { visits.catalogue += 1; }
            }
            let Some(handle) = heap.handle(logical) else {
                if !is_exact { continue; }
                return reject(logical, "outside guest heap");
            };
            let mut observed = None;
            if !is_exact {
                let Some(words @ [_, index, generation]) = super::record_at(handle) else { continue };
                let record = u64::from(generation) << 32 | u64::from(index);
                if self.superseded_record(logical, || record) { continue; }
                if let Some(r) = row && r.record == record && r.form != 0
                    && slots.opaque & r.form == 0 && !(slots.opaque & 32 != 0 && r.form & 7 != 0)
                { continue; }
                // This dimension comes from the very same immutable record.
                // A stale record is also ineligible, so excluded dimensions
                // do not need to consult the resource-view table again.
                if let Some(r) = row && r.record == record && matches!(r.form, 8 | 16) && r.texture_kind != 0 {
                    let kinds = slots.texture_kinds[usize::from(r.form == 16)];
                    let scopes = if r.form == 8 { stages } else { uav_stages };
                    if kinds != 0 && kinds & r.texture_kind == 0
                        || scopes.is_some_and(|s| s.excludes(r.texture_kind, logical)) { continue; }
                }
                // An opaque buffer has no per-draw work once its native
                // descriptor is current. Ordered reads are gathered below,
                // and submit refreshes the bytes actually bound by the heap.
                if recording && row.is_some_and(|r| r.record == record && r.form & 7 != 0)
                    && self.quick.get(logical as usize).is_some_and(|k| k.load(std::sync::atomic::Ordering::Acquire) == record)
                    && self.buffer_ready(logical)
                { continue; }
                let topology = crate::resource::alias_generation();
                let known = row.and_then(|r| r.plain.as_ref()).filter(|p| p.record == record && p.topology == topology);
                if !recording && let Some(binding) = known.and_then(|p| p.binding.get())
                    && slots.opaque & 8 != 0 && slots.accepts_opaque_texture(&binding.view)
                    && sampled_slots.is_none_or(|set| set.contains(&logical))
                    && self.made.get(logical) == Some(record)
                    && let Some(image) = binding.image.upgrade()
                    && !image.retired.load(std::sync::atomic::Ordering::Acquire)
                    && self.keeps_resource(logical, &image.texture)
                    && crate::resource::alias_generation() == topology
                {
                    let slot = (self as *const Self as usize, logical);
                    if stages.is_some_and(|s| s.excludes(texture_kind(&binding.view).map_or(0, |(_, k)| k), logical))
                        || list.indexed_texture_matches(slot, (record, image.serial, None), false).is_ok()
                        || late.as_deref_mut().is_some_and(|samples| {
                            let access = crate::host::SampleAccess { heap: slot.0,
                                kind: texture_kind(&binding.view).map_or(0, |(_, k)| k), logical, stages };
                            let ready = list.prepare_late_sampled_texture(binding.object, &binding.view, &image, samples, Some(access));
                            #[cfg(feature = "diagnostics")]
                            if ready { samples.note_upload_origin(image.serial, record, &binding.view, access, is_exact, sampled_slots.is_some()); }
                            ready
                        })
                    { continue; }
                }
                let prepared_or_deferred = |proof: &PlainMaterial| {
                    if proof.after_writes(recorded_writes.get_or_init(|| list.indexed_write_ranges())) { return true; }
                    // This draw needs its original recording-time preparation.
                    // A preceding draw on the same list may already have done
                    // it for this exact mip view and still-published live image.
                    if self.made.get(logical) != Some(record) { return false; }
                    let Some(image) = list.prepared_sampled_slot(self as *const Self as usize, logical, record, false) else { return false };
                    self.keeps_resource(logical, &image.texture)
                };
                if may_defer && known.is_some_and(&prepared_or_deferred) {
                    ready_materials += 1;
                    continue;
                }
                // A previous draw prepared this exact SRV in this recording.
                // Recheck its guest record and lifetime, allocation topology
                // and published image before using the recorded preparation.
                // Submission also pins the enclosing heap's publication.
                let lifetime = super::record_lifetime(generation);
                if (may_defer || !recording) && row.is_some_and(|r| r.record == record && r.form == 8)
                    && self.made.get(logical) == Some(record) && self.buffer_ready(logical)
                    && let Some(image) = list.current_sampled_slot(self as *const Self as usize, logical, record, topology, lifetime)
                    && self.keeps_resource(logical, &image.texture)
                    && topology == crate::resource::alias_generation()
                    && lifetime == super::record_lifetime(generation)
                { continue; }
                let Some(current) = super::recorded_view(words) else { continue };
                let form = indexed_form(&current);
                if form == 16 && uav_stages.is_some_and(|s|
                    s.excludes(texture_kind(&current).map_or(0, |(_,k)| k), logical)) { continue; }
                if form == 8 && (sampled_slots.is_some_and(|s| !s.contains(&logical))
                    || stages.is_some_and(|s| s.excludes(texture_kind(&current).map_or(0, |(_, k)| k), logical))) { continue; }
                if !slots.accepts_opaque_texture(&current) { continue; }
                if slots.opaque & form == 0
                    && !(slots.opaque & 32 != 0 && view_span(&current).is_some())
                { continue; }
                // A prior overlapping or unknown writer needs the original
                // ordered preparation at this draw.
                if may_defer && known.is_none()
                    && matches!(&current, View::Placed(false, r, _) if matches!(r[8], 70..=84 | 94..=99))
                    && let Some(proof) = self.candidates.known_plain(logical, record, topology)
                        .or_else(|| self.prove_plain_material(logical, record, topology, &current))
                    && prepared_or_deferred(&proof)
                {
                    ready_materials += 1;
                    continue;
                }
                observed = Some((record, lifetime, current));
            }
            self.room_for_title(heap, logical);
            let lifetime = observed.as_ref().map(|entry| (entry.0, entry.1));
            let Some((key, view)) = self.mirror_observed_record(logical, handle, true, observed) else {
                return reject(logical, "guest record or native slot unavailable");
            };
            let form = indexed_form(&view);
            // Another shader's opaque accesses cannot satisfy a known fetch
            // of a different resource class at this exact logical slot.
            let requested = slots.exact.get(&logical).copied().unwrap_or(slots.opaque);
            if requested & form == 0 && !(requested & 32 != 0 && view_span(&view).is_some()) {
                return reject(logical, "descriptor has unexpected resource kind");
            }
            if !self.buffer_ready(logical) {
                return reject(logical, "native descriptor creation failed");
            }
            if form == 4 && slots.buffer_writes.iter().any(|proof| proof.slots().contains(&logical)) {
                let binding = if let Some((shadow, address, bytes)) = buffer_copy(&view)
                    && self.keeps_resource(logical, &shadow.buffer) {
                    (key, shadow.id(), address, bytes)
                } else if matches!(&view, View::Unordered(0, 0, Some(d)) if d[1] == 1)
                    || matches!(&view, View::Placed(true, r, d) if r[0] == 1 && d[1] == 1 && d[4] == 0) {
                    (key, 0, 0, 0)
                } else {
                    return reject(logical, "bounded buffer write has no matching native binding");
                };
                let slot = (self as *const Self as usize, logical);
                if !list.indexed_buffer_writer_matches(slot, binding, recording) {
                    return reject(logical, "bounded buffer write binding changed since recording");
                }
            }
            if matches!(form, 8 | 16) {
                let topology = crate::resource::alias_generation();
                let (resolved, placed_source) = match placed_texture_and_source(&view) {
                    Some((resolved, source)) => (resolved, Some(source)),
                    None => (view.clone(), None),
                };
                let (kind, object) = match &resolved {
                    View::Shader(object, _) => (victory_shader::ResourceType::Srv, *object),
                    View::Unordered(object, _, _) => (victory_shader::ResourceType::Uav, *object),
                    _ => return reject(logical, "placed texture unresolved"),
                };
                if record_key(handle) != key {
                    return reject(logical, "guest texture descriptor changed during materialization");
                }
                if object == 0 && explicit_null_texture(&resolved) {
                    // A late typed null needs the published descriptor, but no
                    // recorded allocation, transitions, upload, or readback.
                    if key == 0 || self.made.get(logical) != Some(key) {
                        return reject(logical, "typed null descriptor changed during publication");
                    }
                    continue;
                }
                let Some(copy) = crate::resource::host_copy(object as crate::object::This) else {
                    return reject(logical, "native texture unavailable");
                };
                let points_to_copy = || self.keeps_resource(logical, &copy.texture)
                    || (kind == victory_shader::ResourceType::Uav &&
                        (crate::resource::twin_resource(object as crate::object::This).is_some_and(|r| self.keeps_resource(logical, r))
                      || crate::resource::twin_plane_resource(object as crate::object::This).is_some_and(|r| self.keeps_resource(logical, r))));
                if !points_to_copy() {
                    let made = self.made.lock(logical);
                    self.quick_is(logical, 0);
                    self.made.set(logical, 0);
                    drop(made);
                    if self.mirror(logical, handle, true).is_none() || !self.buffer_ready(logical) || !points_to_copy() {
                        return reject(logical, "replacement native descriptor unavailable");
                    }
                }
                // Native barriers and GPU alias copies belong immediately before this draw.
                // The submit-time journal must still name that very record and host allocation.
                if self.made.get(logical) != Some(key) {
                    return reject(logical, "descriptor generation changed");
                }
                // Placement resolution already identified the producer and
                // whether this SRV directly shows it. A remap between the
                // resolution and native publication requires a fresh lookup.
                let source = if let Some(source) = placed_source && topology == crate::resource::alias_generation() {
                    source
                } else { crate::resource::alias_of(object as crate::object::This) };
                let source_serial = source.and_then(crate::resource::host_copy).map(|c| c.serial);
                if source.is_some() && source_serial.is_none() { return reject(logical, "alias producer unavailable"); }
                let version = (key, copy.serial, source_serial);
                // Keep a weak binding under the same physical mapping and
                // producer proof that allowed recording to defer this material.
                // Later lists still check the live descriptor and allocation.
                if !recording && !is_exact && source.is_none() && copy.flags() & 7 == 0
                    && matches!(&view, View::Placed(false, r, _) if matches!(r[8], 70..=84 | 94..=99))
                    && let Some(proof) = self.candidates.known_plain(logical, key, topology)
                        .or_else(|| self.prove_plain_material(logical, key, topology, &view))
                {
                    proof.binding.get_or_init(|| MaterialBinding { object, view: resolved.clone(), image: std::sync::Arc::downgrade(&copy) });
                }
                let slot = (self as *const Self as usize, logical);
                if let Err(previous) = list.indexed_texture_matches(slot, version, recording) {
                    let old_view = previous.and_then(|(key, _, _)| view_at(key as u32, (key >> 32) as u32));
                    let old_address = old_view.as_ref().and_then(placement_address);
                    let addresses = (old_address, placement_address(&view));
                    let reason = match previous {
                        None => "new indexed texture since recording",
                        _ if matches!(addresses, (Some(old), Some(new)) if old != new) => "indexed texture guest placement changed",
                        Some((_, _, old)) if old != source_serial => "indexed texture alias producer changed",
                        Some((_, old, _)) if old != copy.serial => "indexed texture native allocation changed",
                        _ => "indexed texture guest descriptor changed",
                    };
                    // A volatile sampled SRV may appear between recording and submission.
                    // CPU-owned bytes can be uploaded and transitioned on this occurrence's
                    // prelude. A writer or alias needs ordering relative to the original draw.
                    let this = object as crate::object::This;
                    let placed = matches!(view, View::Placed(false, ..));
                    let ordinary = matches!(view, View::Shader(..));
                    let cpu_owned = crate::resource::title_fills(this) && !crate::texture::host_wrote(object);
                    let same_object = matches!(old_view.as_ref(), Some(View::Shader(old, _)) if ordinary && *old == object)
                        || matches!(addresses, (Some(old), Some(new)) if placed && old == new);
                    let same_allocation = previous.is_some_and(|(_, serial, producer)| serial == copy.serial && producer.is_none())
                        && same_object;
                    let mut narrow_mips = false;
                    if !is_exact && matches!(view, View::Unordered(..)) && kind == victory_shader::ResourceType::Uav
                        && source.is_none() && crate::resource::twin(this).is_none()
                        && !crate::resource::title_fills(this) && !crate::texback::wanted(this)
                    {
                        let writes = slots.opaque & victory_shader::emit::TEXTURE_WRITE_ACCESS != 0;
                        if recording {
                            list.note_volatile_sample(slot);
                            crate::draw::readable_resolved(list, kind, &resolved, false, None, writes);
                            continue;
                        }
                        let access = crate::host::SampleAccess { heap:self as *const Self as usize,
                            kind:texture_kind(&view).map_or(0,|(_,k)|k), logical, stages:uav_stages };
                        if list.prepared_native_uav(object, &copy, writes, access) { continue; }
                    }
                    // A volatile heap slot is consumed at execution. If different
                    // draws see different SRVs, keep both preparations and check
                    // the final descriptor again on every submission, even if the
                    // title restores the first record. The prelude can then place
                    // its read before all draws or reject an incompatible writer.
                    if recording && !is_exact && kind == victory_shader::ResourceType::Srv
                        && (ordinary || placed) && previous.is_some_and(|v| v.1 != copy.serial || v.2 != source_serial)
                    {
                        list.note_volatile_sample(slot);
                        crate::draw::readable_resolved(list, kind, &resolved, placed, source, false);
                        continue;
                    }
                    let failure = if slots.exact.contains_key(&logical) { Some("exact access") }
                        else if kind == victory_shader::ResourceType::Uav {
                            Some(if source.is_some() { "changed UAV aliases a separate producer" }
                                else if let Some(twin) = crate::resource::twin(this) {
                                    if !recording {
                                        let access = crate::host::SampleAccess {
                                            heap: self as *const Self as usize,
                                            kind: texture_kind(&view).map_or(0, |(_, kind)| kind),
                                            logical,
                                            stages: uav_stages,
                                        };
                                        let image = twin.plane.as_ref().into_iter()
                                            .chain(std::iter::once(&twin.colour))
                                            .find(|image| self.keeps_resource(logical, &image.texture));
                                        if slots.opaque & victory_shader::emit::TEXTURE_WRITE_ACCESS != 0
                                            && !crate::resource::title_fills(this) && !crate::texback::wanted(this)
                                            && image.is_some_and(|image| list.prepared_depth_twin_uav(object, &twin, image, access))
                                        {
                                            continue;
                                        }
                                        #[cfg(feature = "diagnostics")]
                                        if let Some(image) = image {
                                            list.trace_late_depth_conflict(&copy, &twin, access, Some(image));
                                        }
                                    }
                                    "changed UAV needs its depth twin synchronized"
                                }
                                else if crate::resource::title_fills(this) { "changed UAV needs ordered upload of its initial texels" }
                                else if crate::texback::wanted(this) { "changed UAV needs ordered readback after shader writes" }
                                else { "changed native UAV lacks a recorded write or compatible command state" })
                        }
                        else if !(placed || ordinary) || kind != victory_shader::ResourceType::Srv { Some("not a sampled SRV") }
                        // A placed SRV can resolve directly to the producer's native
                        // image. Its render-target/UAV capabilities do not imply a
                        // separate alias copy. The prelude transitions that exact
                        // allocation after earlier lists. prepare_late_native_sample
                        // checks for incompatible states inside this recording.
                        else if let Some(producer) = source {
                            if !recording && late.as_deref_mut().is_some_and(|samples| {
                                let ready = list.prepare_late_alias_sample(this, producer, &resolved, &copy, samples);
                                #[cfg(feature = "diagnostics")]
                                if ready {
                                    let access = crate::host::SampleAccess { heap: slot.0,
                                        kind: texture_kind(&view).map_or(0, |(_, k)| k), logical, stages };
                                    samples.note_upload_origin(copy.serial, key, &view, access, is_exact, sampled_slots.is_some());
                                }
                                ready
                            }) { None }
                            else { Some("separate alias image needs ordered copy") }
                        }
                        else if !cpu_owned && placed && crate::resource::placed_span(this).is_some()
                            && late_guest_sample_conflict(logical, object, &copy).is_some()
                        { Some("separate writer overlaps sampled placement") }
                        else if !recording && let Some(twin) = crate::resource::twin(this) {
                            // The console D16S8 stencil SRV is format 120 on
                            // plane one of a Texture2D. Its copy is separate
                            // from the pass that publishes the depth plane.
                            let stencil = matches!(&resolved, View::Shader(_, Some(d))
                                if d[0] == 120 && d[1] == 4 && d[6] == 1);
                            let access = crate::host::SampleAccess {
                                heap: self as *const Self as usize,
                                kind: texture_kind(&view).map_or(0, |(_, kind)| kind),
                                logical,
                                stages,
                            };
                            if late.as_deref_mut().is_some_and(|samples| list.prepare_late_depth_sample(&copy, twin, samples, access, stencil)) { None }
                            else { Some("sampled depth image has conflicting recorded use") }
                        }
                        else if let Some(conflict) = (recording && cpu_owned).then(|| {
                            let conflict = late_guest_sample_conflict(logical, object, &copy)?;
                            // A neighbouring target can own the excluded main mip
                            // while the published compressed SRV reads the mip prefix.
                            if same_allocation && matches!(&view, View::Placed(false, resource, _)
                                if matches!(crate::resource::element(resource[8]), Some((_, 4))))
                                && old_view.as_ref().is_some_and(|old| same_placed_srv_mip_window(old, &view))
                                && let Some(prefix) = crate::resource::placed_upload_prefix(this, &resolved, &copy)
                                && late_sample_span_conflict(logical, object, &copy, Some(prefix), |_| false).is_none()
                            {
                                narrow_mips = true;
                                #[cfg(feature = "diagnostics")]
                                if crate::switch::on("VICTORY_TRACE_TEXTURE_OWNER") {
                                    tracing::info!(logical, serial=copy.serial, address=format_args!("{:#x}", prefix.0), bytes=prefix.1,
                                        "deferred republished compressed SRV with disjoint mip prefix");
                                }
                                None
                            } else { Some(conflict) }
                        }).flatten() { Some(conflict) }
                        else if recording {
                            let opaque_writer = cpu_owned
                                && slots.opaque & victory_shader::emit::TEXTURE_WRITE_ACCESS != 0;
                            // Streaming can move the mip window while several draws on
                            // the same list name the same immutable allocation. Both
                            // views contribute their required subresources to its upload.
                            let plain_placement = placed && same_allocation && copy.flags() & 7 == 0;
                            // A placed SRV's host image can carry UAV capability
                            // even when the guest description has no writer flag.
                            let deferred_mips = narrow_mips || opaque_writer && !plain_placement && same_allocation
                                && old_view.as_ref().is_some_and(|old| same_placed_srv_mip_window(old, &view));
                            #[cfg(feature = "diagnostics")]
                            if opaque_writer && same_allocation && !plain_placement {
                                static FIRST: [std::sync::Once; 2] = [const { std::sync::Once::new() }; 2];
                                FIRST[usize::from(deferred_mips)].call_once(|| tracing::info!(logical, serial=copy.serial,
                                    native_flags=copy.flags(), previous=?old_view, current=?view, deferred_mips,
                                    "checked a republished mip window on a UAV-capable sampled image"));
                            }
                            if opaque_writer && !(plain_placement || deferred_mips) {
                                Some("sampled guest bytes may be written by this recording")
                            } else if same_allocation
                                && list.advance_sampled_view(slot, previous, version)
                            {
                                // Check the execution-time upload against the actual write
                                // ranges under the pinned descriptor publication.
                                if opaque_writer || narrow_mips { list.note_volatile_sample(slot); }
                                if deferred_mips {
                                    // The final descriptor chooses the upload
                                    // range after real writer and alias checks.
                                    const SHADER_RESOURCE: u32 = 0xc0;
                                    crate::list::ensure_state(list, this, SHADER_RESOURCE);
                                } else {
                                    crate::draw::readable_resolved(list, kind, &resolved, placed, source, false);
                                }
                                None
                            } else { Some("recorded draw requires a different texture allocation") }
                        } else if !late.as_deref_mut().is_some_and(|late| {
                            let access = crate::host::SampleAccess { heap: self as *const Self as usize, kind: texture_kind(&view).map_or(0, |(_, k)| k), logical, stages };
                            if cpu_owned {
                                let ready = list.prepare_late_sampled_texture(object, &resolved, &copy, late, Some(access));
                                #[cfg(feature = "diagnostics")]
                                if ready { late.note_upload_origin(copy.serial, key, &view, access, is_exact, sampled_slots.is_some()); }
                                ready
                            }
                            else { list.prepare_late_native_sample(&copy, &resolved, late, access) }
                        }) {
                            if cpu_owned { Some("CPU-owned sampled upload or recorded SRV state unavailable") }
                            else { Some("native image lacks SRV state at a matching shader command") }
                        }
                        else { None };
                    note_indexed_change(logical, previous, version, addresses, reason, failure);
                    if failure.is_some() { return reject(logical, reason); }
                    // Submit verifies the heap publication again after preparation. A replay
                    // must prepare its then-current resource, never reuse this occurrence.
                    continue;
                }
                if recording {
                    let heap_id = self as *const Self as usize;
                    let plain = matches!(view, View::Placed(false, ..)) && source.is_none()
                        && copy.flags() & 7 == 0 && crate::texture::PICKING.get().is_none()
                        && !crate::texture::host_wrote(object);
                    let native = kind == victory_shader::ResourceType::Srv && source.is_none() && copy.flags() & 2 == 0
                        && !crate::resource::title_fills(object as _) && crate::resource::twin(object as _).is_none()
                        && crate::texture::PICKING.get().is_none();
                    if (plain || native) && list.prepared_sampled_slot(heap_id, logical, key, native)
                        .is_some_and(|image| image.serial == copy.serial) { continue; }
                    let opaque = if slots.accepts_opaque_texture(&view) { slots.opaque } else { 0 };
                    let texture_write = (requested | opaque) & victory_shader::emit::TEXTURE_WRITE_ACCESS != 0;
                    let prepared = crate::draw::readable_resolved(list, kind, &resolved, matches!(view, View::Placed(..)), source, texture_write);
                    if (plain && prepared) || native {
                        let stamp = lifetime.filter(|&(record, _)| record == key)
                            .map(|(_, lifetime)| (topology, lifetime));
                        list.note_sampled_indexed_slot(heap_id, logical, key, &copy, native, stamp);
                    }
                }
            } else if !recording || is_exact {
                if recording {
                    let kind = crate::draw::view_kind(&view).unwrap_or(victory_shader::ResourceType::Cbv);
                    crate::draw::readable(list, kind, &view);
                } else if let Some((shadow, address, len)) = buffer_copy(&view)
                    && (!shadow.is_read_presentation() || !shadow.ordered_source()) {
                    shadow.refresh(list, address, len);
                }
            }
        }
        if ready_materials != 0 {
            static DRAWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let draws = DRAWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if draws.is_power_of_two() {
                tracing::debug!(draws, ready_materials, "plain material SRVs prepared once per indexed list");
            }
        }
        if slots.opaque & 3 != 0 || slots.exact.values().any(|form| form & 3 != 0) {
            let reads: Vec<_> = {
                let ordered = self.ordered_reads.lock().unwrap_or_else(|e| e.into_inner());
                if slots.opaque & 3 != 0 {
                    ordered.values().filter(|(s, _, _)| s.ordered_source()).cloned().collect()
                } else {
                    slots.exact.iter().filter(|(_, form)| **form & 3 != 0)
                        .filter_map(|(slot, _)| ordered.get(slot))
                        .filter(|(s, _, _)| s.ordered_source()).cloned().collect()
                }
            };
            for (shadow, address, len) in reads {
                if recording {
                    list.note_indexed_read(shadow.id(), address, len);
                    shadow.refresh_read(list, address, len);
                } else if !list.has_indexed_read(shadow.id(), address, len) {
                    // This recording has no copy after a possible preceding
                    // GPU writer for the newly selected presentation.
                    return reject(u32::MAX, "recorded buffer read missing");
                }
            }
        }
        true
    }

    /// Number of advertised title slots represented by the host heap.
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// The GPU address of the table of element strides by title slot (view
    /// heaps).
    pub(crate) fn strides_gpu(&self) -> Option<u64> {
        self.strides.as_ref().map(|s| s.1)
    }

    /// The table's buffer, for a list to hold.
    pub(crate) fn strides_buffer(&self) -> Option<&ID3D12Resource> {
        self.strides.as_ref().map(|s| &s.0)
    }

    /// Up to `max` title slots whose texture view is due at `frame`, each
    /// with its write number, in the order to serve them
    /// ([`TextureQueue::due`]), and the queue's counts
    /// ([`TextureQueue::census`]).
    pub(crate) fn texture_slots_due(&self, frame: u64, max: usize) -> (Vec<(u32, u64)>, [u64; 5]) {
        let (due, census) = {
            let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            textures.scan_due(frame, max)
        };
        (TextureQueue::in_order(due, max), census)
    }

    pub(crate) fn texture_slots_ahead(&self, frame: u64, max: usize) -> Vec<(u32, u64)> {
        let due = self
            .textures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .all_due(frame);
        TextureQueue::in_order(due, max)
    }

    /// A number that changes when a slot is written or made due anew, and
    /// whether memory was noted since a pass last took the notes.
    pub(crate) fn texture_news(&self) -> (u64, bool) {
        let writes = self.textures.lock().unwrap_or_else(|e| e.into_inner()).writes;
        let noted = |ranges: &Mutex<Changes>, i: usize| ranges.lock().unwrap_or_else(|e| e.into_inner()).seq != self.changes_seen[i].load(std::sync::atomic::Ordering::Relaxed);
        (writes, noted(&REMAPPED,0) || noted(&PAGE_WRITTEN,1))
    }

    /// Every title slot of the queue that is due at `frame`, and whether
    /// it was uploaded before (in part, or whole and waiting for another
    /// look): for the card accounting line
    /// (`crate::resource::card_accounting`).
    pub(crate) fn texture_slots_all_due(&self, frame: u64) -> Vec<(u32, bool)> {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures
            .slots
            .iter()
            .filter(|(_, w)| w.due <= frame)
            .map(|(&index, w)| (index, w.mapped > 0 || w.checked.2 != 0))
            .collect()
    }

    /// The texture view of title slot `index` (as handed out with `write`)
    /// was brought up to date: due again from frame `again` (uploaded with
    /// that many bytes of its memory there, not all), or not until the
    /// slot is written again.
    pub(crate) fn texture_slot_since(&self, index: u32) -> Option<(u64, bool, u64)> {
        self.textures.lock().unwrap_or_else(|e| e.into_inner()).since(index)
    }

    /// The texture view of title slot `index` (as handed out with `write`)
    /// was brought up to date: see [`HostHeap::texture_slot_done`].
    pub(crate) fn texture_slot_done(&self, index: u32, write: u64, again: Option<(u64, u64)>) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        match again {
            Some((frame, mapped)) => textures.partly(index, write, frame, mapped),
            None => textures.done(index, write),
        }
    }

    /// Title slot `index` was uploaded whole: see [`TextureQueue::recheck`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn texture_slot_recheck(
        &self,
        index: u32,
        write: u64,
        due: u64,
        at: (u64, u64),
        print: u64,
        looks: u8,
        uploaded: u64,
    ) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures.recheck(index, write, due, at, print, looks, uploaded);
    }

    /// See [`TextureQueue::checked`].
    pub(crate) fn texture_slot_checked(&self, index: u32) -> Option<(u64, u64, u64, u8, u64)> {
        self.textures.lock().unwrap_or_else(|e| e.into_inner()).checked(index)
    }

    /// Nothing to upload at title slot `index` now (its memory is not
    /// mapped, or no more of it than before): looked at again after 1, 2,
    /// 4, ... frames, at most `most`.
    pub(crate) fn texture_slot_later(&self, index: u32, write: u64, frame: u64, most: u64) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures.later(index, write, frame, most);
    }

    /// See [`TextureQueue::hold`].
    pub(crate) fn texture_slot_held(&self, index: u32, write: u64, due: u64) {
        self.textures.lock().unwrap_or_else(|e| e.into_inner()).hold(index, write, due);
    }

    /// See [`TextureQueue::room`].
    pub(crate) fn texture_slots_room(&self, frame: u64) -> usize {
        self.textures.lock().unwrap_or_else(|e| e.into_inner()).room(frame)
    }

    /// Bytes of title slot `index`'s memory that were mapped when it was
    /// last uploaded in part (0: not uploaded, or whole).
    pub(crate) fn same_partial_backing(
        &self,
        index: u32,
        write: u64,
        address: usize,
        len: usize,
        mapped: u64,
    ) -> bool {
        let identity = crate::texture::backing_identity(address, len);
        let mut queue = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        let Some(slot) = queue.same(index, write) else {
            return false;
        };
        if slot.backing == Some(identity) {
            return slot.mapped == mapped;
        }
        slot.backing = Some(identity);
        slot.mapped = 0;
        false
    }

    pub(crate) fn texture_slot_mapped(&self, index: u32) -> u64 {
        self.textures.lock().unwrap_or_else(|e| e.into_inner()).mapped(index)
    }

    /// Takes the ranges of title memory the runtime mapped, unmapped or
    /// wrote since the last call ([`remapped`]) and makes the slots whose
    /// placed texture lies there due from the next frame (by when a write
    /// under way has landed): memory that arrived is uploaded, memory that
    /// went makes the slot null ([`HostHeap::texture_slot_gone`]), and a
    /// texture streamed into the place of another of the same description
    /// is uploaded although no descriptor says so. Slots tables read are
    /// left to their draws. Returns how many slots it made due.
    pub(crate) fn texture_slots_remapped(&self, frame: u64) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        self.texture_slots_page_written(frame);
        let mut ranges = REMAPPED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take(&self.changes_seen[0]);
        if ranges.is_empty() {
            return 0;
        }
        join(&mut ranges);
        let mut slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            ranges
                .iter()
                .flat_map(|&(start, end)| placed.affected(start, end))
                .collect()
        };
        slots.sort_unstable();
        slots.dedup();
        slots.retain(|&slot| !self.tabled_lately(slot, frame));
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        for &slot in &slots {
            textures.changed(slot, frame, frame);
        }
        STALE[3].fetch_add(slots.len() as u64, Relaxed);
        slots.len()
    }

    /// Makes the slots of the textures filled with less than was mapped
    /// ([`read_short`]) due from the next frame on. Returns how many.
    pub(crate) fn texture_slots_short(&self, frame: u64) -> usize {
        if self.sampler {
            return 0;
        }
        let starts = std::mem::take(&mut SHORT.lock().unwrap_or_else(|e| e.into_inner()).0);
        if starts.is_empty() {
            return 0;
        }
        let mut slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            starts.iter().flat_map(|&start| placed.slots_at(start)).collect()
        };
        slots.sort_unstable();
        slots.dedup();
        slots.retain(|&slot| !self.tabled_lately(slot, frame));
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        for &slot in &slots {
            textures.changed(slot, frame, frame + 1);
        }
        slots.len()
    }

    /// Make views of changed physical bytes due on this pass.
    fn texture_slots_page_written(&self, frame: u64) -> usize {
        let ranges = PAGE_WRITTEN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take(&self.changes_seen[1]);
        if ranges.is_empty() {
            return 0;
        }
        self.slots_written_under(&ranges, frame, true)[2]
    }

    /// [`HostHeap::texture_slots_page_written`] for `ranges` (start, end):
    /// the slots that held bytes of them, those of them uploaded before,
    /// and those made due (the latter, if `due`).
    fn slots_written_under(&self, ranges: &[(u64, u64)], frame: u64, due: bool) -> [usize; 3] {
        use std::sync::atomic::Ordering::Relaxed;
        let mut slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            ranges
                .iter()
                .flat_map(|&(start, end)| placed.affected(start, end))
                .collect()
        };
        slots.sort_unstable();
        slots.dedup();
        slots.retain(|&slot| !self.tabled_lately(slot, frame));
        let held = slots.len();
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        // Uploaded before: out of the queue, or in it after an upload (in
        // part, or whole and being looked at again).
        slots.retain(|slot| textures.slots.get(slot).is_none_or(|w| w.mapped > 0 || w.checked.2 != 0));
        PAGE_WRITES[2].fetch_add(held as u64, Relaxed);
        PAGE_WRITES[3].fetch_add(slots.len() as u64, Relaxed);
        if !due {
            return [held, slots.len(), 0];
        }
        for &slot in &slots {
            textures.changed(slot, frame, frame);
        }
        [held, slots.len(), slots.len()]
    }

    /// Whether host slot `index` is made from the very record the title
    /// has at `handle` now. Its host texture then exists (a slot keeps what
    /// it views), so bringing that texture up to date takes no memory of
    /// the card.
    pub(crate) fn texture_slot_made_from(&self, index: u32, handle: u64) -> bool {
        if index >= self.count {
            return false;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { super::record_bytes::read(handle as *const u8) };
        let key = u64::from(generation) << 32 | u64::from(i);
        if tag != VIEW_TAG {
            return false;
        }
        // And the slot holds a host resource: a slot is also noted as made
        // from a view the host could not make (`make` gave nothing, the
        // slot is null and keeps nothing). The host copy of such a
        // texture is held by no slot, may have been let go, and would be
        // made anew here, with the card full.
        if key == 0 || self.made.get(index) != Some(key) {
            return false;
        }
        let _made = self.made.lock(index);
        self.made.get(index) == Some(key) && self.kept_in(index).is_some()
    }

    /// Levels on demand (`crate::tiles`): makes again the host slots of the
    /// placed textures whose first level with memory changed since the
    /// last call ([`levels_changed`]), each from the view it was made
    /// from, so that it shows the texture from that level ([`make`]). Only
    /// slots that are made; a slot draws read through a table is made by
    /// its draw. Returns how many it made.
    /// shortcut: the starts are kept in one place for all heaps; the heap
    /// whose pass runs first takes them (a title has one view heap).
    /// Levels on demand (`crate::tiles`): the card has room, so the slots
    /// of up to [`Self::WAITING_A_PASS`] placed textures whose upload left
    /// a held level without tiles at least a second ago
    /// ([`levels_waiting`]) are due again from the next frame, as memory
    /// mapped under them would make them
    /// ([`HostHeap::texture_slots_remapped`]): the upload then gives the
    /// level its tiles. To be called only while the card has room: without
    /// it the upload would give nothing again. Slots tables read are left
    /// to their draws (`texture.rs` `upload` asks again for those). Returns
    /// how many slots it made due.
    /// shortcut: kept in one place for all heaps, as the starts below.
    pub(crate) fn texture_slots_waiting(&self, frame: u64) -> usize {
        if self.sampler {
            return 0;
        }
        let starts: Vec<u64> = {
            let mut waiting = WAITING.lock().unwrap_or_else(|e| e.into_inner());
            if waiting.is_empty() {
                return 0;
            }
            let starts: Vec<u64> = waiting
                .iter()
                .filter(|&(_, &since)| crate::host::long_ago(since, 1000, 2))
                .map(|(&start, _)| start)
                .take(Self::WAITING_A_PASS)
                .collect();
            for start in &starts {
                waiting.remove(start);
            }
            starts
        };
        if starts.is_empty() {
            return 0;
        }
        let mut slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            starts.iter().flat_map(|&start| placed.slots_at(start)).collect()
        };
        slots.sort_unstable();
        slots.dedup();
        slots.retain(|&slot| !self.tabled_lately(slot, frame));
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        for &slot in &slots {
            textures.changed(slot, frame, frame + 1);
        }
        crate::tiles::STATS[10].fetch_add(slots.len() as u64, std::sync::atomic::Ordering::Relaxed);
        slots.len()
    }

    /// Textures a pass asks again for their levels
    /// ([`HostHeap::texture_slots_waiting`]): the first ones given tiles
    /// may take the room the others were asked for, and an upload that
    /// gives nothing is a whole texture staged and copied for nothing.
    const WAITING_A_PASS: usize = 128;

    pub(crate) fn texture_views_remade(&self) -> usize {
        if self.sampler {
            return 0;
        }
        {
            let mut after = RESHOWN_AFTER.lock().unwrap_or_else(|e| e.into_inner());
            if !after.is_empty() {
                let finished = crate::host::submissions().1;
                let mut now = RESHOWN.lock().unwrap_or_else(|e| e.into_inner());
                after.retain(|&(at, start)| {
                    if at <= finished {
                        now.push(start);
                    }
                    at > finished
                });
            }
        }
        let starts = std::mem::take(&mut *RESHOWN.lock().unwrap_or_else(|e| e.into_inner()));
        if starts.is_empty() {
            return 0;
        }
        let slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            starts.iter().flat_map(|&start| placed.slots_at(start)).collect()
        };
        let mut remade = 0;
        for slot in slots {
            let _made = self.made.lock(slot);
            let Some(key) = self.made.get(slot).filter(|&key| key != 0) else {
                continue;
            };
            let Some(view) = view_at(key as u32, (key >> 32) as u32) else {
                continue;
            };
            let _publication = self.publish_views(PublicationCause::TextureRemake);
            if let Some(kept) = make(&view, self.at(slot)) {
                self.put(slot);
                self.keep(slot, kept);
                remade += 1;
            }
        }
        remade
    }

    /// The memory of the texture title slot `index` views is not mapped
    /// (any more): a host slot made from it becomes null again, so that
    /// the host copy can be let go (`crate::resource::let_go_idle`; a slot
    /// keeps what it views). True if the slot was made.
    ///
    /// Not at the first look: a title that moves a texture in its streaming
    /// memory unmaps it before it maps it again, and what was shown went
    /// black for the pictures in between (session play-1125-0127: faces,
    /// books, posters and sleeves for 0.3 to 2 s). Only when it is still
    /// away `AWAY_MS` after it was first found away
    /// (`VICTORY_TEXTURE_AWAY_MS`; 0: at once, as before).
    pub(crate) fn texture_slot_gone(&self, index: u32) -> bool {
        const AWAY_MS: u64 = 3000;
        static WAIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let wait = *WAIT.get_or_init(|| {
            std::env::var("VICTORY_TEXTURE_AWAY_MS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(AWAY_MS)
        });
        let _made = self.made.lock(index);
        let Some(key) = self.made.get(index).filter(|&key| key != 0) else {
            return false;
        };
        let now = crate::host::now_ms();
        {
            let mut away = self.away.lock().unwrap_or_else(|e| e.into_inner());
            match away.get(&index) {
                Some(&(was, since)) if was == key && now.saturating_sub(since) < wait => return false,
                Some(&(was, _)) if was == key => {
                    away.remove(&index);
                }
                _ if wait == 0 => {}
                _ => {
                    away.insert(index, (key, now));
                    return false;
                }
            }
        }
        self.unmake(index);
        STALE[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// Whether host slot `index` shows the placed texture the title's record
    /// at `handle` views, by an earlier view of it.
    pub(crate) fn texture_slot_shows_other(&self, index: u32, handle: u64) -> bool {
        let key = record_key(handle);
        let Some(shown) = self.made.get(index) else {
            return false;
        };
        if key == 0 || shown == 0 || shown == key {
            return false;
        }
        let view = |key: u64| view_at(key as u32, (key >> 32) as u32);
        matches!((view(shown), view(key)), (Some(a), Some(b)) if same_placed_texture(&a, &b))
    }

    /// Slot `index` is made from the record at `handle` (what it is now)
    /// when `list`, which holds the upload of what that view shows, has run:
    /// until then it shows what it showed. (Made at once, the frames on the
    /// card read the new view before its levels were filled.)
    pub(crate) fn texture_slot_flip(&self, list: &crate::host::HostList, index: u32, handle: u64) {
        self.flip(list, index, handle, None);
    }

    /// The same of a slot that shows nothing of the texture the record at
    /// `handle` views, or something else: until then it shows the neutral
    /// texture ([`show_after`]).
    pub(crate) fn texture_slot_show_after(&self, list: &crate::host::HostList, index: u32, handle: u64) {
        if self.show_later(index, handle) {
            self.flip(list, index, handle, Some(crate::frame::current()));
        }
    }

    /// What slot `index` shows until the texture the record at `handle` views
    /// is shown. False: it shows it, or there is none.
    fn show_later(&self, index: u32, handle: u64) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let key = record_key(handle);
        let view = view_at(key as u32, (key >> 32) as u32);
        let (true, Some(view)) = (key != 0, view) else {
            return false;
        };
        let stale = {
            let _made = self.made.lock(index);
            let Some(shown) = self.made.get(index) else {
                return false;
            };
            if shown == key || self.superseded_record(index, || key) {
                return false;
            }
            // (As `mirror`.)
            let _publication = self.publish_views(PublicationCause::NeutralView);
            let stale = self.note_buffer(index, Some(&view), true, key);
            if self.unmapped(index) {
                return false;
            }
            let old = view_at(shown as u32, (shown >> 32) as u32);
            let kept = old.is_some_and(|old| match kin(&old, &view) {
                Kin::Same => true,
                kin => crate::resource::room_lately() && kin.kept(keep_shown()),
            });
            if shown == 0 || !kept {
                #[cfg(feature = "diagnostics")]
                note_before(index, shown, "neutral");
                self.quick_is(index, 0);
                self.made.set(index, 0);
                let kept = neutral_for(&view, self.at(index));
                self.put(index);
                self.clear_twin(index);
                LATE[3].fetch_add(u64::from(kept.is_some()), Relaxed);
                self.keep(index, kept);
            }
            stale
        };
        self.unmake_stale(index, stale);
        true
    }

    /// [`HostHeap::texture_slot_show_after`] of a texture whose upload `job`
    /// makes: shown when that has run.
    pub(crate) fn texture_slot_show_ahead(&self, job: &crate::texture::ahead::Job, index: u32, handle: u64) {
        if !self.show_later(index, handle) {
            return;
        }
        let (heap, key) = (
            std::sync::Arc::downgrade(&self.flip_identity),
            record_key(handle),
        );
        let late = Some(crate::frame::current());
        let asked = crate::host::now_ms();
        let version = flip_version(key);
        job.then(move || {
            let flip = Flip {
                after: 0,
                heap: heap.clone(),
                index,
                handle,
                key,
                version,
                late,
                asked,
            };
            queue_flip(flip);
        });
    }

    /// [`HostHeap::mirror`] of a slot whose table a draw read: a texture made
    /// a moment ago is shown after the draw's list ([`show_after`]; the draw
    /// reads the table's dynamic slots).
    pub(crate) fn mirror_after(&self, list: &crate::host::HostList, index: u32, handle: u64) {
        if show_after()
            && index < self.count
            && !self.texture_slot_made_from(index, handle)
            && let Some(view) = super::view(handle)
            && let Some(View::Shader(resource, _)) = placed_texture(&view).or(Some(view))
            && crate::resource::buffer_bytes(resource as crate::object::This).is_none()
            && crate::resource::host_resource(resource as crate::object::This).is_some_and(crate::pool::fresh)
            && !crate::texture::ahead::there(resource as crate::object::This)
        {
            return self.texture_slot_show_after(list, index, handle);
        }
        self.mirror(index, handle, true);
    }

    fn flip(&self, list: &crate::host::HostList, index: u32, handle: u64, late: Option<u64>) {
        let (heap, key) = (
            std::sync::Arc::downgrade(&self.flip_identity),
            record_key(handle),
        );
        let asked = crate::host::now_ms();
        let version = flip_version(key);
        list.defer_texture_note(move || {
            let after = crate::host::submissions().0;
            let flip = Flip {
                after,
                heap: heap.clone(),
                index,
                handle,
                key,
                version,
                late,
                asked,
            };
            queue_flip(flip);
        });
    }

    /// Makes the slots whose new view's upload has run
    /// ([`HostHeap::texture_slot_flip`]); returns how many.
    pub(crate) fn texture_slots_flip(&self) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let finished = crate::host::submissions().1;
        let ready: Vec<Flip> = {
            let mut flips = FLIPS.lock().unwrap_or_else(|e| e.into_inner());
            if flips.is_empty() {
                return 0;
            }
            let own = std::sync::Arc::as_ptr(&self.flip_identity);
            let (ready, wait): (Vec<Flip>, Vec<Flip>) = flips
                .drain(..)
                .partition(|f| f.after <= finished && f.heap.as_ptr() == own);
            *flips = wait;
            ready
        };
        {
            let mut pending = NEW_PENDING.lock().unwrap_or_else(|e| e.into_inner());
            if !pending.is_empty() {
                for f in &ready {
                    pending.remove(&(f.heap.as_ptr() as usize, f.index, f.key));
                }
            }
        }
        let mut flipped = 0;
        for f in ready {
            let _made = self.made.lock(f.index);
            let Some(shown) = self.made.get(f.index) else {
                continue;
            };
            if shown == f.key || self.superseded_record(f.index, || f.key) {
                continue;
            }
            // Written again since, or made null: that write is in the queue.
            // (A slot shown late is null until now.)
            let view = view_at(f.key as u32, (f.key >> 32) as u32);
            let there = shown != 0 || f.late.is_some();
            let (true, true, Some(view)) = (f.key != 0 && record_key(f.handle) == f.key, there, view) else {
                FLIPPED[1].fetch_add(1, Relaxed);
                LATE[4].fetch_add(u64::from(f.late.is_some()), Relaxed);
                continue;
            };
            if f.version.is_none() || f.version != flip_version(f.key) { continue; }
            // Its memory went meanwhile: the texture pass finds that.
            if f.late.is_some()
                && let View::Placed(_, resource, desc) = &view
            {
                let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                let len = crate::resource::placed_bytes(resource).unwrap_or(1);
                if crate::texture::mapped_bytes(address as *const u8, len as usize) == 0 {
                    LATE[4].fetch_add(1, Relaxed);
                    continue;
                }
            }
            // Under Made, this slot cannot regain a physical assignment here.
            if self.unmapped(f.index) {
                #[cfg(feature = "diagnostics")]
                self.publication_stats.note(PublicationCause::UnmappedFlipSkipped);
                continue;
            }
            let _publication = self.publish_views(PublicationCause::TextureFlip);
            if let Some(kept) = make(&view, self.at(f.index)) {
                #[cfg(feature = "diagnostics")]
                note_before(f.index, shown, if f.late.is_some() { "shown after its upload" } else { "flip" });
                self.put(f.index);
                #[cfg(feature = "diagnostics")]
                let was = shown;
                self.quick_is(f.index, 0);
                self.made.set(f.index, f.key);
                self.keep(f.index, kept);
                #[cfg(feature = "diagnostics")]
                if f.late.is_some() && was != 0 {
                    same_bytes(was, f.key);
                }
                crate::texlate::note(
                    if f.late.is_some() { crate::texlate::Kind::New } else { crate::texlate::Kind::Sharper },
                    f.asked,
                );
                if let Some(since) = f.late {
                    let frames = crate::frame::current().saturating_sub(since);
                    let n = LATE[0].fetch_add(1, Relaxed) + 1;
                    let all = LATE[1].fetch_add(frames, Relaxed) + frames;
                    let most = LATE[2].fetch_max(frames, Relaxed).max(frames);
                    if n == 1 || n.is_multiple_of(5000) {
                        tracing::info!(
                            "slots shown after their texture's upload (VICTORY_SHOW_AFTER=1), since the start: {n}, {:.2} frames after they were asked for, the longest {most}; the neutral texture meanwhile in {}; {} not shown any more by then",
                            all as f64 / n as f64,
                            LATE[3].load(Relaxed),
                            LATE[4].load(Relaxed)
                        );
                    }
                } else {
                    flipped += 1;
                }
            }
        }
        FLIPPED[0].fetch_add(flipped as u64, Relaxed);
        flipped
    }

    /// The memory of the texture title slot `index` views is there (again):
    /// see [`HostHeap::texture_slot_gone`].
    pub(crate) fn texture_slot_here(&self, index: u32) {
        let mut away = self.away.lock().unwrap_or_else(|e| e.into_inner());
        if !away.is_empty() {
            away.remove(&index);
        }
    }

    fn superseded_record(&self, index: u32, key: impl FnOnce() -> u64) -> bool {
        let retired = self.superseded.get(index);
        retired != 0 && retired == key()
    }

    /// Makes host slot `index` null again, as it is before its view is
    /// made: a null view of the kind it held. Under its lock ([`Made`]).
    fn unmake(&self, index: u32) {
        let _publication = self.publish_views(PublicationCause::Unmake);
        self.quick_is(index, 0);
        self.view_metadata(index, None);
        let Some(key) = self.made.get(index) else {
            return;
        };
        self.made.set(index, 0);
        if key == 0 {
            self.keep(index, None);
            self.release_title(index);
            return;
        }
        match view_at(key as u32, (key >> 32) as u32) {
            Some(view) => {
                if let View::Placed(false, ref resource, ref desc) = view
                    && resource[0] != 1
                    && let Some(old) = self.kept_in(index)
                {
                    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                    crate::resource::streamed_view_gone(address, resource, &old);
                }
                null_for(&view, self.at(index));
            }
            None => null_view(self.at(index)),
        }
        self.put(index);
        self.clear_twin(index);
        self.keep(index, None);
        self.release_title(index);
    }

    /// The slots [`HostHeap::note_buffer`] found stale for slot `index`, with
    /// what they were made from then, become null. None of their locks is
    /// held here; one made from another record meanwhile is left as it is.
    fn unmake_stale(&self, index: u32, stale: Vec<(u32, u64)>) {
        for (slot, key) in stale {
            if slot == index {
                continue;
            }
            let _made = self.made.lock(slot);
            if self.superseded.may(slot) && self.made.get(slot) == Some(key) {
                self.unmake(slot);
            }
        }
    }

    /// Title slot `index` is a table's ([`HostHeap::tabled`]). `read`: a
    /// draw read it through the table and brought what it views up to
    /// date, so the slot leaves the queue of textures picked by index; the
    /// caller makes the host slot in place ([`HostHeap::mirror`]), as the
    /// pass would have, for a shader that picks it by index all the same.
    pub(crate) fn texture_slot_tabled(&self, index: u32, read: bool) {
        self.tabled_in(index, read, crate::frame::current());
    }

    /// [`HostHeap::texture_slot_tabled`] in frame `frame`.
    fn tabled_in(&self, index: u32, read: bool, frame: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(stamp) = self.tabled.get(index as usize) else {
            return;
        };
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        let old = stamp.load(Relaxed);
        if !read {
            if old & !TABLED_READ != now {
                stamp.store(now, Relaxed);
            }
            return;
        }
        // Once per slot and frame: tables are read by every draw.
        if old != now | TABLED_READ {
            stamp.store(now | TABLED_READ, Relaxed);
            if tabled_out() && self.queued.may(index) {
                let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
                if textures.take_out(index) {
                    TABLED_WRITES[1].fetch_add(1, Relaxed);
                }
            }
        }
    }

    /// Whether title slot `index` was a table's within the last
    /// [`TABLED_FRAMES`] frames before `frame` (never, if such slots are
    /// to be queued as any other).
    fn tabled_lately(&self, index: u32, frame: u64) -> bool {
        let stamp = self.tabled_stamp(index) & !TABLED_READ;
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        tabled_out() && stamp != 0 && u64::from(now.wrapping_sub(stamp)) <= TABLED_FRAMES
    }

    /// What `tabled` holds for title slot `index`; 0: never a table's.
    fn tabled_stamp(&self, index: u32) -> u32 {
        self.tabled
            .get(index as usize)
            .map_or(0, |s| s.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// The queue's part of a write of title slot `index` in `frame`:
    /// `texture`, whether it now holds a view of a texture the title may
    /// fill; `seen` as for [`HostHeap::note_buffer`].
    fn queue_texture(&self, index: u32, texture: bool, seen: bool, frame: u64) {
        let stamp = if tabled_out() { self.tabled_stamp(index) } else { 0 };
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        // A table's slot lately: the draw that reads it uploads its
        // texture, so its write waits for that draw.
        let lately = stamp != 0 && u64::from(now.wrapping_sub(stamp & !TABLED_READ)) <= TABLED_FRAMES;
        // Made now by a draw of this frame that read it through a table:
        // up to date.
        let read_now = seen && stamp == now | TABLED_READ;
        // (A slot is written by one thread at a time.)
        if !(texture && !read_now) && !self.queued.may(index) {
            return;
        }
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        if texture && !read_now {
            let due = if lately && !seen {
                TABLED_WRITES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                frame + TABLED_GRACE
            } else {
                0
            };
            textures.written(index, frame, due, seen);
        } else {
            textures.remove(index);
        }
    }

    /// Title slot `index` lies in a table a draw reads or had made afresh:
    /// what it holds is the draw's to make ([`HostHeap::copied_tabled`]).
    pub(crate) fn slot_tabled(&self, index: u32) {
        self.tabled_in(index, false, crate::frame::current());
    }

    /// The title slots that hold buffer views.
    pub(crate) fn buffer_slots(&self) -> Vec<u32> {
        self.buffers.all(u64::MAX)
    }

    /// Of [`HostHeap::buffer_slots`], those of one of `PARTS` parts: every
    /// slot is in the same part each time.
    pub(crate) fn buffer_slots_of<const PARTS: u32>(&self, part: u32) -> Vec<u32> {
        self.buffers.part::<PARTS>(part)
    }

    /// The title slots whose buffer view was written, or went, since the
    /// last call.
    pub(crate) fn buffer_changes(&self) -> Vec<u32> {
        self.changed.take()
    }

    /// Records what title slot `index` now holds for shaders that pick
    /// buffers from the heap: its element stride in the table, and whether
    /// it is a buffer view at all.
    /// `seen`: the slot was not written now, a draw or a pass made its
    /// host slot from what it held.
    /// Returns the title slots whose placed texture view the write made
    /// stale ([`Spans`]), each with the record its host slot is made from:
    /// out of the queue of textures already, their host slots are the
    /// caller's to make null when it has let go of its own slot's lock
    /// ([`HostHeap::unmake_stale`]).
    fn note_buffer(&self, index: u32, view: Option<&View>, seen: bool, key: u64) -> Vec<(u32, u64)> {
        self.note_buffer_backing(index, view, seen, key, &mut ViewBacking::default())
    }

    fn note_buffer_backing(&self, index: u32, view: Option<&View>, seen: bool, key: u64, backing: &mut ViewBacking) -> Vec<(u32, u64)> {
        if self.sampler {
            return Vec::new();
        }
        // Measured for the texture side of the same gap (textures a shader
        // picks by a computed index are neither made, if placed, nor
        // uploaded): slots written with a placed texture view, and with a
        // view of a texture object.
        let texture = match view {
            Some(View::Placed(false, resource, desc)) if resource[0] != 1 && (desc[10] | desc[11]) != 0 => {
                PLACED_TEXTURE_SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            Some(View::Shader(r, _)) if *r != 0 && crate::resource::buffer_bytes(*r as crate::object::This).is_none() => {
                OBJECT_TEXTURE_SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            _ => false,
        };
        let form = view.map_or(0, indexed_form);
        // The memory a placed texture view describes. Not for a slot
        // tables read (thousands of writes a frame, each read by its
        // draw): no pass looks at it, and the same texture's view in a
        // slot shaders pick by index says what it says.
        let placed_view = match view {
            Some(View::Placed(false, resource, desc))
                if resource[0] != 1 && (desc[10] | desc[11]) != 0 && !self.tabled_lately(index, crate::frame::current()) =>
            {
                // (And the level the view starts at: word 4 of a texture
                // view's description, after format, dimension, mapping
                // and a pad.)
                let at = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                let held = crate::resource::placed_bytes_from(resource, crate::resource::first_mip(desc));
                Some((resource, at, desc[4], held.map_or(u64::MAX, |held| at.saturating_add(held))))
            }
            _ => None,
        };
        // (Under the slot's lock, as every write of it: whether it is in
        // `spanned` changes only here.)
        let in_spans = |index: u32| {
            let bit = 1u64 << (index % 64);
            self.spanned.0.get(index as usize / 64).is_none_or(|w| w.load(std::sync::atomic::Ordering::Acquire) & bit != 0)
        };
        let stale = if placed_view.is_none() && !in_spans(index) && spans_quick() {
            SPANS_QUICK[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Vec::new()
        } else {
            use std::sync::atomic::Ordering::Relaxed;
            SPANS_QUICK[1].fetch_add(1, Relaxed);
            let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            if !seen || placed.first.get(&index).is_none_or(|&(_, _, old)| old != key) {
                self.superseded.set(index, 0);
            }
            let span = placed_view.and_then(|(resource, start, first, held)| {
                Some((start, start.saturating_add(placed.bytes(resource)?), first, held))
            });
            match span {
                Some((start, end, first, held)) => {
                    SPANNED[0].fetch_min(start, Relaxed);
                    SPANNED[1].fetch_max(end, Relaxed);
                    placed.rule = stale_rule();
                    self.spanned.put(index);
                    let stale = placed.note(index, start, end, !seen);
                    for &slot in &stale {
                        self.superseded.set(slot, placed.first.get(&slot).map_or(0, |&(_, _, old)| old));
                    }
                    placed.shows(index, first, held, key);
                    // (Under the lock of `placed`: a later write of one of
                    // them comes by here before it makes the slot.)
                    stale.into_iter().map(|slot| (slot, self.made.get(slot).unwrap_or(0))).collect()
                }
                None => {
                    placed.forget(index);
                    self.spanned.remove(index);
                    Vec::new()
                }
            }
        };
        self.queue_texture(index, texture && !self.superseded.may(index), seen, crate::frame::current());
        if !stale.is_empty() {
            STALE[0].fetch_add(stale.len() as u64, std::sync::atomic::Ordering::Relaxed);
            let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            for &(slot, _) in &stale {
                textures.remove(slot);
            }
        }
        if !seen || view.is_none() {
            self.ordered_reads
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&index);
        }
        if view.is_some() {
            self.assign_title(index);
        }
        let pending_buffer = (!seen || view.is_none()).then(|| view.filter(|v| requires_buffer(v)).map_or(0, indexed_form));
        self.candidates.observed(index, key, form, view.and_then(texture_kind).map_or(0, |(_, kind)| kind), pending_buffer);
        let stride = view.and_then(view_stride);
        // A guest write publishes the logical span even when its buffer is
        // deferred. Its native byte bias and copied-read role are published
        // when mirror makes the descriptor, under the same publication guard.
        self.write_view_metadata(index, view, seen && !self.unmapped(index), backing);
        if view.is_none() {
            self.release_title(index);
        }
        let was = if stride.is_some() || matches!(view, Some(View::Constant(..))) {
            self.buffers.put(index);
            true
        } else {
            self.buffers.remove(index)
        };
        if was {
            self.changed.put(index);
        }
        stale
    }

    pub(crate) fn buffer_ready(&self, index: u32) -> bool {
        self.buffer_problem(index).is_none()
    }

    pub(crate) fn buffer_problem(&self, index: u32) -> Option<&'static str> {
        if self.unmapped(index) { return Some("no native descriptor slot"); }
        self.failed_buffers.may(index).then_some("native descriptor layout or backing unavailable")
    }

    pub(crate) fn unavailable_buffer(&self, index: u32) {
        self.make_null(index);
        self.failed_buffers.set(index, true);
    }

    fn constant_bounds(&self, physical: u32, bias: u32, size: u32) {
        if !victory_shader::emit::mapped_views()
            || physical >= victory_shader::emit::view_heap_limit()
        {
            return;
        }
        if let Some((_, _, data)) = &self.strides {
            // One aligned upload word per native descriptor. Title writes
            // hold the publication guard; dynamic slots belong to their list.
            unsafe {
                let bounds = (*data as *mut u8)
                    .add(victory_shader::emit::VIEW_CONSTANT_BOUNDS as usize)
                    .cast::<u64>();
                bounds
                    .add(physical as usize)
                    .write_volatile(u64::from(size) << 32 | u64::from(bias));
            }
        }
    }

    pub(crate) fn address_candidates(&self, slots: &IndexedSlots) -> Vec<crate::memory::ViewSpan> {
        let addresses = self.indexed_addresses.lock().unwrap_or_else(|e| e.into_inner());
        if slots.opaque & 32 != 0 {
            addresses.slots.values().map(|&(span, _)| span).collect()
        } else {
            slots.exact.iter().filter(|(_, form)| **form & 32 != 0)
                .filter_map(|(slot, _)| addresses.slots.get(slot).map(|&(span, _)| span)).collect()
        }
    }

    /// Buffer UAVs a shader can store through in the pinned heap generation.
    /// Read-only views in the same heap cannot write a sampled image.
    pub(crate) fn writable_candidates(&self, slots: &IndexedSlots) -> Option<Vec<crate::memory::ViewSpan>> {
        let write = victory_shader::emit::BUFFER_WRITE_ACCESS;
        let addresses = self.indexed_addresses.lock().unwrap_or_else(|e| e.into_inner());
        if slots.opaque & write != 0 {
            Some(addresses.writable.iter().filter_map(|i| addresses.slots.get(i).map(|p| p.0)).collect())
        } else {
            slots.exact.iter().filter(|(_, form)| **form & write != 0)
                .map(|(i, _)| addresses.slots.get(i).filter(|p| p.1 == 4).map(|p| p.0)).collect()
        }
    }

    pub(crate) fn simultaneous_buffers(&self, operation: &BufferAliases) -> bool {
        let addresses = self.indexed_addresses.lock().unwrap_or_else(|e| e.into_inner());
        if addresses.copied.is_empty() { return true; }
        let collect = |role, candidates: &std::collections::BTreeSet<u32>| {
            let access = if role == 4 { victory_shader::emit::BUFFER_WRITE_ACCESS } else { role };
            let mut ranges = Vec::new();
            let mut add = |slot: &u32| {
                if let Some(&(span, kind)) = addresses.slots.get(slot)
                    && kind == role && let Some(len) = span.bytes && len != 0 {
                    ranges.push((span.at, span.at.saturating_add(len), *slot));
                }
            };
            if operation.slots.opaque & access != 0 {
                // Opaque heap indexing ranges over title slots. Dynamic
                // entries participate through their explicit table selections.
                candidates.range(..self.count).for_each(&mut add);
            }
            for (slot, &form) in &operation.slots.exact {
                if form & access != 0 { add(slot); }
            }
            ranges
        };
        let reads = collect(2, &addresses.copied);
        if reads.is_empty() { return true; }
        if operation.unknown_writer {
            #[cfg(feature = "diagnostics")]
            {
                static FIRST: std::sync::Once = std::sync::Once::new();
                FIRST.call_once(|| tracing::warn!(read_slot = reads[0].2,
                    read = ?(reads[0].0, reads[0].1), "a shader may write by address while reading a copied buffer view"));
            }
            return false;
        }
        let mut writes = collect(4, &addresses.writable);
        writes.extend(operation.root_writes.iter().map(|&(lo, hi)| (lo, hi, u32::MAX)));
        drop(addresses);
        if writes.is_empty() { return true; }
        // Two guest VAs may expose the same section bytes. A presentation
        // copied at either VA cannot observe a concurrent writer to its alias.
        let mut readable = Vec::new();
        for (start, end, slot) in reads {
            let (Ok(start), Ok(bytes)) = (usize::try_from(start), usize::try_from(end - start)) else { return false };
            readable.extend(crate::inplace::aliases_of_bytes(start, bytes).into_iter().map(|(lo, hi)| (lo, hi, slot)));
        }
        readable.sort_unstable();
        writes.sort_unstable();
        let mut at = 0;
        for (lo, hi, _slot) in readable {
            let (start, end) = (lo as u64, hi as u64);
            while at < writes.len() && writes[at].1 <= start { at += 1; }
            if at < writes.len() && writes[at].0 < end {
                #[cfg(feature = "diagnostics")]
                {
                    static FIRST: std::sync::Once = std::sync::Once::new();
                    FIRST.call_once(|| {
                        let (wlo, whi, writer) = writes[at];
                        tracing::warn!(read_slot = _slot, write_slot = writer, read = ?(lo, hi),
                            write = ?(wlo, whi), read_view = ?self.describe_slot(_slot),
                            write_view = ?(writer != u32::MAX).then(|| self.describe_slot(writer)),
                            opaque = operation.slots.opaque, "copied SRV intersects a simultaneous buffer writer");
                    });
                }
                return false;
            }
        }
        true
    }

    pub(crate) fn view_metadata(&self, index: u32, view: Option<&View>) {
        self.write_view_metadata(index, view, true, &mut ViewBacking::default());
    }

    fn write_view_metadata(&self, index: u32, view: Option<&View>, resident: bool, backing: &mut ViewBacking) {
        // Dynamic tables have their own physical slots; opaque address reads
        // through such a table must also cover those list-owned entries.
        let mut rgb32 = false;
        {
            let span = view.and_then(|v| {
                // A native null UAV discards all stores. Keep it distinct from
                // an unresolved writer whose guest address has not been found.
                let span = view_span(v).or_else(|| match v {
                    View::Unordered(0, 0, Some(d)) if d[1] == 1 => Some(crate::memory::ViewSpan { at: 0, bytes: Some(0) }),
                    View::Placed(true, r, d) if r[0] == 1 && d[1] == 1 && d[4] == 0 =>
                        Some(crate::memory::ViewSpan { at: placement_address(v)?, bytes: Some(0) }),
                    _ => None,
                })?;
                let role = match indexed_form(v) {
                    4 => 4,
                    2 if resident => {
                        let copied = backing.binding(v).is_some_and(|(s, at, len)| {
                            rgb32 = view_elements(v).is_some_and(|kind| {
                                mapped_rgb32(s.offset(at), len, kind, srv_mapping(v)).is_some()
                            });
                            s.is_read_presentation()
                        });
                        if copied { 2 } else { 0 }
                    }
                    _ => 0,
                };
                Some((span, role))
            });
            let mut addresses = self.indexed_addresses.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(span) = span {
                addresses.insert(index, span);
            } else {
                addresses.remove(&index);
            }
        }
        let index = self.physical(index);
        if let Some((_, _, data)) = &self.strides
            && index < victory_shader::emit::SLOT_STRIDES_COUNT
            && index != victory_shader::emit::view_heap_limit()
        {
            let native_view = view.filter(|_| resident);
            let prefix = native_view.and_then(|v| raw_prefix(v, backing)).unwrap_or(0) / 4;
            let word = view.and_then(view_stride).unwrap_or(0) | view.map_or(0, view_kind)
                | prefix << victory_shader::emit::VIEW_PREFIX_SHIFT
                | if rgb32 { victory_shader::emit::VIEW_RAW_RGB32 } else { 0 };
            // SAFETY: both tables have SLOT_STRIDES_COUNT physical entries.
            unsafe { (*data as *mut u32).add(index as usize).write_volatile(word) };
            write_view_address(*data, index, view);
            if victory_shader::emit::mapped_views() {
                let (bias, size) = match native_view {
                    Some(v @ View::Constant(_, size)) => backing.binding(v)
                        .map_or((0, 0), |(s, address, _)| ((s.offset(address) & 15) as u32, *size)),
                    _ => (0, 0),
                };
                self.constant_bounds(index, bias, size);
            }
        }
    }

    /// Gives back `n` dynamic slots from `first` (an index [`dynamic`]
    /// returned).
    ///
    /// [`dynamic`]: HostHeap::dynamic
    pub(crate) fn release(&self, first: u32, n: u32) {
        let guard = self.last_reader.read().unwrap_or_else(|e| e.into_inner());
        if *guard != 0 && *guard > crate::host::completed_descriptor_reader() {
            let mut retired = self.retired_runs.lock().unwrap_or_else(|e| e.into_inner());
            retired.push((*guard, first, n));
            self.has_retired_runs
                .store(true, std::sync::atomic::Ordering::Release);
        } else {
            self.free_run(first, n);
        }
    }

    fn free_run(&self, first: u32, n: u32) {
        let Some(at) = first.checked_sub(self.count) else {
            return;
        };
        let mut resources = Vec::new();
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let end = (at + n).min(self.dynamic) as usize;
        if let Some(slots) = guard.1.get_mut(at as usize..end) {
            let mut addresses = self.indexed_addresses.lock().unwrap_or_else(|e| e.into_inner());
            for slot in first..self.count + end as u32 {
                addresses.remove(&slot);
                // Only the owning recording can name a dynamic slot. Its
                // final reader has passed before this run is made reusable.
                if let Some(resource) = self.kept[slot as usize].lock().unwrap_or_else(|e| e.into_inner()).take() {
                    resources.push(resource);
                }
            }
            let given = slots.iter().filter(|taken| **taken).count() as u32;
            self.free.fetch_add(given, std::sync::atomic::Ordering::Relaxed);
            self.too_long.store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
            slots.fill(false);
        }
        drop(guard);
        drop(resources);
    }

    /// Makes slot `index` (a dynamic one) from `view`; a null view if the
    /// host cannot.
    pub(crate) fn make_at(&self, index: u32, view: &View) {
        let mut backing = ViewBacking::default();
        self.write_view_metadata(index, Some(view), true, &mut backing);
        let kept = make_twinned_backing(view, self.at(index), self.twin(index), &mut backing);
        self.failed_buffers
            .set(index, kept.is_none() && required_view(view));
        if kept.is_none() {
            null_for(view, self.at(index));
        }
        self.put(index);
        self.twin_made(index, kept.is_some() && twinned(view));
        self.keep(index, kept.flatten());
    }

    /// Makes slot `index` (a dynamic one) the view the record `words` names,
    /// from this thread's descriptor of it if the recording `recording` has
    /// made one, else as [`HostHeap::make_at`] does, and keeps that
    /// ([`ListViews`]). What is known of the record in this recording;
    /// `None` (the slot is not made): not a view kept so.
    pub(crate) fn make_listed(&self, recording: u64, words: [u32; 3], index: u32) -> Option<Listed> {
        if self.sampler {
            return None;
        }
        LIST_VIEWS.with(|views| {
            let mut views = views.borrow_mut();
            if views.is_none() {
                *views = Some(ListViews::new()?);
            }
            let views = views.as_mut()?;
            if views.recording != recording {
                views.recording = recording;
                views.made.clear();
            }
            let twins = self.twins.get(index as usize).is_some();
            if !views.made.contains_key(&words) {
                let view = super::view_at(words[1], words[2])?;
                let at = 2 * views.made.len() as u32;
                // (A constant buffer's bytes are copied for each draw.)
                if at >= ListViews::SLOTS || matches!(view, View::Constant(..)) {
                    return None;
                }
                let twin = twinned(&view);
                let kept = make_twinned(&view, views.slot(at), Some(views.slot(at + 1)))?;
                let listed = Listed {
                    kind: crate::draw::view_kind(&view),
                    texture: is_texture_view(&view),
                    ready: (0, 0, false),
                    tabled: None,
                };
                views.made.insert(words, ListView { view, at, twin, kept, listed });
            }
            let made = &views.made[&words];
            self.failed_buffers.set(index, false);
            self.view_metadata(index, Some(&made.view));
            self.copy_from(index, views.slot(made.at));
            if made.twin && twins {
                self.copy_from(self.raw + index, views.slot(made.at + 1));
                self.twins[index as usize].store(true, std::sync::atomic::Ordering::Relaxed);
            } else {
                self.clear_twin(index);
            }
            self.keep(index, made.kept.clone());
            Some(made.listed)
        })
    }

    /// As [`HostHeap::copy_to`], from the slot `from`, which the card does not
    /// see.
    fn copy_from(&self, to: u32, from: D3D12_CPU_DESCRIPTOR_HANDLE) {
        #[cfg(feature = "diagnostics")]
        if let Some(written) = self.written.get(to as usize) {
            written.store(crate::host::coarse_ms() as u32, std::sync::atomic::Ordering::Relaxed);
        }
        let Some(host) = host() else { return };
        // SAFETY: a slot of our heap, and one of this thread's heap.
        unsafe { host.device.CopyDescriptorsSimple(1, self.cpu(to), from, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) };
    }

    /// Makes slot `index` read nothing: a null view, or a plain sampler.
    pub(crate) fn make_null(&self, index: u32) {
        self.failed_buffers.set(index, false);
        self.view_metadata(index, None);
        if self.sampler {
            self.null_sampler(index);
        } else {
            null_view(self.at(index));
            self.put(index);
        }
        self.clear_twin(index);
        self.keep(index, None);
    }

    /// Where the raw twin of slot `index` is made.
    fn twin(&self, index: u32) -> Option<D3D12_CPU_DESCRIPTOR_HANDLE> {
        (index < self.twins.len() as u32).then(|| staged(self.sampler, 1).unwrap_or_else(|| self.cpu(self.raw + index)))
    }

    /// The raw twin of slot `index` was made by this thread ([`make_twinned`]),
    /// or is a null view if it was not.
    fn twin_made(&self, index: u32, made: bool) {
        if !made {
            return self.clear_twin(index);
        }
        if let Some(shown) = self.twins.get(index as usize) {
            self.put_twin(index);
            shown.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The raw twin of slot `index` is a null view again.
    fn clear_twin(&self, index: u32) {
        if let Some(made) = self.twins.get(index as usize)
            && made.swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            null_buffer_view(self.at(self.raw + index));
            self.put(self.raw + index);
        }
    }

    /// Slot `index` now views `resource` (or nothing of the host's).
    fn keep(&self, index: u32, resource: Option<ID3D12Resource>) {
        let Some(slot) = self.kept.get(index as usize) else {
            return;
        };
        let old = {
            use windows::core::Interface;
            let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
            // The same again: nothing to retire.
            if slot.as_ref().map(Interface::as_raw) == resource.as_ref().map(Interface::as_raw) {
                return;
            }
            std::mem::replace(&mut *slot, resource)
        };
        // A running submission may still read the old resource through
        // this slot.
        if let Some(old) = old {
            crate::host::retire_resource(old);
        }
    }

    /// What slot `index` views.
    pub(super) fn kept_in(&self, index: u32) -> Option<ID3D12Resource> {
        self.kept.get(index as usize)?.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(super) fn keeps_resource(&self, index: u32, resource: &ID3D12Resource) -> bool {
        use windows::core::Interface;
        self.kept.get(index as usize).is_some_and(|slot| {
            slot.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
                .is_some_and(|held| held.as_raw() == resource.as_raw())
        })
    }

    /// Makes slot `index` (a dynamic one) a raw buffer view of `len` bytes
    /// of `buffer` from byte `offset` (a multiple of 16): a root SRV with a
    /// size, whose reads past it return zeros instead of reaching whatever
    /// the host placed after the buffer.
    pub(crate) fn raw_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = host() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of our heap and a live buffer covering the range.
        unsafe {
            host.device
                .CreateShaderResourceView(buffer, Some(&desc), self.at(index))
        };
        self.put(index);
        // (Bound by its handle, not read by index.)
        self.clear_twin(index);
        self.keep(index, Some(buffer.clone()));
    }

    /// Makes slot `index` (a dynamic one) a raw buffer view that reads
    /// zeros.
    pub(crate) fn null_raw_at(&self, index: u32) {
        null_buffer_view(self.at(index));
        self.put(index);
        self.clear_twin(index);
        self.keep(index, None);
    }

    /// Makes slot `index` (a dynamic one) a raw unordered access view of
    /// `len` bytes of `buffer` from byte `offset` (a multiple of 16): a
    /// root UAV with a size, whose accesses past it do nothing instead of
    /// reaching whatever the host placed after the buffer.
    pub(crate) fn raw_uav_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = host() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    CounterOffsetInBytes: 0,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of our heap and a live buffer covering the range.
        unsafe {
            host.device.CreateUnorderedAccessView(
                buffer,
                None::<&ID3D12Resource>,
                Some(&desc),
                self.at(index),
            )
        };
        self.put(index);
        self.clear_twin(index);
        self.keep(index, Some(buffer.clone()));
    }

    /// Makes slot `index` (a dynamic one) a raw unordered access view of
    /// nothing: reads give zeros, writes are dropped.
    pub(crate) fn null_raw_uav_at(&self, index: u32) {
        null_buffer_uav(self.at(index));
        self.put(index);
        self.clear_twin(index);
        self.keep(index, None);
    }

    /// The shader-visible handle of slot `index`.
    pub(crate) fn gpu_handle(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        // SAFETY: a live shader-visible heap.
        let start = unsafe { self.heap.GetGPUDescriptorHandleForHeapStart().ptr };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: start + u64::from(self.physical(index)) * u64::from(self.step),
        }
    }

    /// Makes slot `index` (a dynamic one) a constant buffer view of host
    /// memory, `size` bytes at `gpu` of `buffer`, which the slot holds.
    ///
    /// The list holds it too, until it is reset; the slot shows it until it
    /// is made again, and a shader that picks the slot by an index it
    /// computes from wrong data reads it through there. The list's scratch
    /// chunks go back to the pool and are freed past its limit: session n02
    /// lost the device to a page fault in the memory of three freed ones.
    /// `VICTORY_SLOT_HOLDS=0`: only the list holds it, as before.
    pub(crate) fn constant_at(&self, index: u32, buffer: &ID3D12Resource, gpu: u64, size: u32) {
        static HOLDS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let holds = *HOLDS.get_or_init(|| std::env::var_os("VICTORY_SLOT_HOLDS").is_none_or(|v| v != "0"));
        let Some(host) = host() else { return };
        self.failed_buffers.set(index, false);
        if victory_shader::emit::mapped_views() {
            if let Some(bias) = raw_constants(buffer, gpu, size, self.at(index)) {
                self.constant_bounds(self.physical(index), bias, size);
            } else {
                self.failed_buffers.set(index, true);
                self.constant_bounds(self.physical(index), 0, 0);
                null_buffer_view(self.at(index));
            }
            self.put(index);
            self.keep(index, Some(buffer.clone()));
            return;
        }
        let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: gpu,
            SizeInBytes: constant_bytes(size),
        };
        // SAFETY: a slot of our heap; the memory is the caller's.
        unsafe {
            host.device
                .CreateConstantBufferView(Some(&desc), self.at(index))
        };
        self.put(index);
        // A program that reads it as raw bytes reads its twin.
        let twin = self.twin(index).filter(|_| desc.SizeInBytes > 0);
        if let Some(twin) = twin {
            // SAFETY: a live buffer that holds the range.
            let first = (gpu - unsafe { buffer.GetGPUVirtualAddress() }) / 4;
            let raw = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        FirstElement: first,
                        NumElements: desc.SizeInBytes / 4,
                        StructureByteStride: 0,
                        Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                    },
                },
            };
            // SAFETY: as above, and a slot of our heaps.
            unsafe { host.device.CreateShaderResourceView(buffer, Some(&raw), twin) };
        }
        self.twin_made(index, twin.is_some());
        // After the descriptor: what it showed is let go when nothing shows
        // it any more.
        self.keep(index, holds.then(|| buffer.clone()));
    }

    /// Renderer port 5 (`crate::port_cbv`): makes slot `index` (a dynamic
    /// one) show the constant buffer of `size` bytes at `gpu` of `buffer`
    /// (title memory in place) with nothing copied: its raw twin a raw view
    /// of those bytes, which translated shaders read; the slot itself a
    /// constant buffer view of them where that can be (on 256 bytes), else a
    /// null one. Whether the twin was made (`false`: the caller copies the
    /// bytes as before).
    pub(crate) fn constant_raw_at(&self, index: u32, buffer: &ID3D12Resource, gpu: u64, size: u32) -> bool {
        let Some(host) = host() else { return false };
        let Some(twin) = self.twin(index) else { return false };
        // SAFETY: a live buffer.
        let (base, width) = unsafe { (buffer.GetGPUVirtualAddress(), buffer.GetDesc().Width) };
        let Some(offset) = gpu.checked_sub(base).filter(|&o| o < width && crate::port_cbv::raw_ok(o)) else {
            return false;
        };
        let bytes = u64::from(constant_bytes(size)).min(width - offset) & !3;
        if bytes == 0 {
            return false;
        }
        let raw = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: offset / 4,
                    NumElements: (bytes / 4) as u32,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        let cbv = D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: if gpu.is_multiple_of(256) { gpu } else { 0 },
            SizeInBytes: if gpu.is_multiple_of(256) { constant_bytes(size).min(((width - offset) & !255) as u32) } else { 0 },
        };
        let cbv = if cbv.SizeInBytes == 0 { D3D12_CONSTANT_BUFFER_VIEW_DESC::default() } else { cbv };
        // SAFETY: slots of our heaps; the ranges lie in the live buffer (a
        // zero location makes a null constant buffer view).
        unsafe {
            host.device.CreateConstantBufferView(Some(&cbv), self.at(index));
            self.put(index);
            host.device.CreateShaderResourceView(buffer, Some(&raw), twin);
        }
        self.twin_made(index, true);
        self.keep(index, Some(buffer.clone()));
        true
    }

    /// Where a descriptor for slot `index` is made, then [`HostHeap::put`]:
    /// a slot of this thread's in a heap the card does not see
    /// ([`staged`]), or the slot itself.
    fn at(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        staged(self.sampler, 0).unwrap_or_else(|| self.cpu(index))
    }

    /// The descriptor this thread made last ([`HostHeap::at`]) goes to slot
    /// `index`.
    fn put(&self, index: u32) {
        self.copy_to(index, 0);
    }

    /// The raw twin this thread made last goes to that of slot `index`.
    fn put_twin(&self, index: u32) {
        if self.twin(index).is_some() {
            self.copy_to(self.raw + index, 1);
        }
    }

    fn copy_to(&self, to: u32, which: u32) {
        #[cfg(feature = "diagnostics")]
        if let Some(written) = self.written.get(to as usize) {
            written.store(crate::host::coarse_ms() as u32, std::sync::atomic::Ordering::Relaxed);
        }
        let (Some(from), Some(host)) = (staged(self.sampler, which), host()) else {
            return; // made in place
        };
        let kind = if self.sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        // SAFETY: a slot of our heap, and one of this thread's heap slots.
        unsafe { host.device.CopyDescriptorsSimple(1, self.cpu(to), from, kind) };
    }

    fn cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let physical = self.physical(index);
        assert!(
            !self.unmapped(index),
            "cannot publish an unresolved logical descriptor"
        );
        self.physical_cpu(physical)
    }

    fn physical_cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + index as usize * self.step as usize,
        }
    }

    /// The records last seen of the title's heap, `step` bytes each, held for
    /// threads that compare what they read of its changed pages beside one
    /// another ([`Copied::changes`]; `super::sync_heaps`: the title copies
    /// records into its heap as bytes).
    ///
    /// The page says only that something on it was written, and `mirror`
    /// by itself would take every buffer or placed-texture record on it for
    /// newly written each time (it leaves such a slot unmade until a draw
    /// or a pass uses it): the texture queue's entry for the slot would
    /// start again, and the buffer pass would look at 2,048 slots for one
    /// that changed.
    pub(crate) fn copied_begin(&self, step: usize) -> Copied<'_> {
        let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
        if copied.is_empty() {
            // Nothing there: a slot never looked at that holds nothing is
            // as it was.
            copied.resize(self.count as usize * step, 0);
        }
        let (at, len) = (copied.as_mut_ptr(), copied.len());
        Copied { at, len, step, _held: copied }
    }

    /// Whether new records of draws' tables are left to the draws
    /// (`VICTORY_HEAP_SYNC=all`: every new record is mirrored at once, as
    /// it first was).
    ///
    /// The title builds each draw's tables in its heap by copying records
    /// there as bytes: 60,000 to 210,000 a frame in its world (session
    /// play-1125-0127), 0.9 microseconds each on the submitting thread and
    /// 1.4 more in the buffer pass they were all handed to, 120 to 600 ms
    /// of every frame, and the draws make what they read of them anyway.
    pub(crate) fn copied_waits(&self) -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_SYNC").is_none_or(|v| v != "all"))
    }

    /// Whether a draw read title slot `index` through a table within the
    /// last few frames before `frame` ([`HostHeap::tabled`]): what it holds
    /// is such a draw's to make.
    pub(crate) fn copied_tabled(&self, index: u32, frame: u64) -> bool {
        if !self.copied_waits() {
            return false;
        }
        let stamp = self.tabled_stamp(index) & !TABLED_READ;
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        stamp != 0 && now.wrapping_sub(stamp) & !TABLED_READ <= COPIED_FRAMES
    }

    /// New records the title copied into slots `slots` in `frame` wait for
    /// the draw that may read them through a table.
    pub(crate) fn copied_wait(&self, frame: u64, slots: Vec<u32>) {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner()).push_back((frame, slots));
    }

    /// The slots that have waited long enough at `frame`
    /// ([`HostHeap::copied_wait`]), each once.
    pub(crate) fn copied_before(&self, frame: u64) -> Vec<u32> {
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        while waiting.front().is_some_and(|(written, _)| written + COPIED_GRACE <= frame) {
            out.extend(waiting.pop_front().into_iter().flat_map(|(_, slots)| slots));
        }
        drop(waiting);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Makes host slot `index` from the record the title has at `handle`
    /// if it changed since, and returns the view recorded there. Buffer
    /// views are made only for draws (`for_draw`), which also copy their
    /// data: a host copy of a buffer is made on first use.
    pub(crate) fn mirror(&self, index: u32, handle: u64, for_draw: bool) -> Option<View> {
        self.mirror_record(index, handle, for_draw).map(|(_, view)| view)
    }

    /// Preserve which guest record provided the decoded view, even if a streaming
    /// thread publishes another mip selection of the same native image meanwhile.
    fn mirror_record(&self, index: u32, handle: u64, for_draw: bool) -> Option<(u64, View)> {
        self.mirror_observed_record(index, handle, for_draw, None)
    }

    fn mirror_observed_record(&self, index: u32, handle: u64, for_draw: bool, mut observed: Option<(u64, u64, View)>) -> Option<(u64, View)> {
        if index >= self.count {
            // Diagnostic (NVIDIA heap cap): the highest title slot past the
            // host heap seen so far.
            static HIGHEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if HIGHEST.fetch_max(index, std::sync::atomic::Ordering::Relaxed) < index {
                tracing::warn!(
                    index,
                    count = self.count,
                    for_draw,
                    "title slot past the host heap"
                );
            }
            return None;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { super::record_bytes::read(handle as *const u8) };
        // Generations start at 1, so a made slot's key is never 0.
        let key = u64::from(generation) << 32 | u64::from(i);
        let mut decode = || {
            observed.take().filter(|(record, lifetime, _)|
                *record == key && *lifetime == super::record_lifetime(generation))
                .map(|(_, _, view)| view).or_else(|| view_at(i, generation))
        };
        // Made from this record, and nothing has changed it: most of the
        // asks, from five recording threads, behind one lock while another
        // held it to make a descriptor (15 % of a recording thread's time in
        // the opening, profile cpu-1).
        if tag == VIEW_TAG
            && quick_mirror()
            && self.quick.get(index as usize).is_some_and(|k| k.load(std::sync::atomic::Ordering::Acquire) == key)
            && let Some(view) = decode()
        {
            return Some((key, view));
        }
        let held = self.made.lock(index);
        let shown = self.made.get(index).unwrap_or(0);
        let view = (tag == VIEW_TAG).then(decode).flatten();
        let Some(view) = view else {
            if shown == 0 && !for_draw && self.assigned.is_empty() {
                #[cfg(feature = "diagnostics")]
                self.publication_stats.note(PublicationCause::EmptyMirrorSkipped);
                return None;
            }
            let _publication = self.publish_views(if shown == 0 {
                PublicationCause::EmptyCleanup
            } else {
                PublicationCause::ClearView
            });
            // Not a view of ours (never written, or of a resource the
            // title released): nothing to read.
            if shown != 0 {
                self.quick_is(index, 0);
                self.made.set(index, 0);
                self.make_null(index);
                let _ = self.note_buffer(index, None, false, 0);
            } else {
                // Left for later and stale since: out of the sets and the stride table.
                let _ = self.note_buffer(index, None, false, 0);
            }
            return None;
        };
        if shown != key {
            let mut publication = self.publish_mirror(MirrorPublication { slot: index, shown, handle, key, view: &view });
            // Views of other textures in the memory this one describes
            // are stale: their host slots read nothing from here on, and
            // let go of the host copies they held.
            // (When this slot's lock is let go: a lock at a time.)
            let mut backing = ViewBacking::default();
            let stale = self.note_buffer_backing(index, Some(&view), for_draw, key, &mut backing);
            if self.unmapped(index) {
                drop(publication);
                drop(held);
                self.unmake_stale(index, stale);
                return None;
            }
            // A view of a texture object that has no host copy yet waits
            // for it, as buffer views wait for a draw: the slot is made
            // when the texture is first used (`super::make_waiting`), or
            // here when a draw reads the slot through a table.
            let waiting = if for_draw { None } else { view.waiting_texture() };
            if let Some(texture) = waiting {
                super::wait_for(texture, handle);
            }
            let later = !for_draw && (view.is_buffer() || waiting.is_some());
            // Another view of the placed texture the slot shows already (a
            // title that streams a texture in writes its view again with
            // more mips each time some arrive): the slot goes on showing
            // the old view until a draw or the texture pass makes the new
            // one and uploads what it shows. Made null here, the texture
            // read as nothing for as long as it waited in the queue, every
            // time it got sharper.
            if later
                && keep_views()
                && let Some(old) = view_at(shown as u32, (shown >> 32) as u32)
            {
                let kin = kin(&old, &view);
                if kin == Kin::Same {
                    KEPT_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    drop(publication);
                    drop(held);
                    self.unmake_stale(index, stale);
                    return Some((key, view));
                }
                // And of another texture that can stand for it
                // ([`keep_shown`]): black, then grey for a frame or two is
                // what popped.
                if is_texture_view(&old) {
                    let kept = crate::resource::room_lately() && kin.kept(keep_shown());
                    rewritten(kin, kept);
                    if kept {
                        drop(publication);
                        drop(held);
                        self.unmake_stale(index, stale);
                        return Some((key, view));
                    }
                }
            }
            // A texture made while lists sent before it may still run: those
            // read this slot by index, and must not find it ([`show_new_later`]).
            // The slot shows what it showed (or the neutral texture), and the
            // new view when they have all run.
            if !later
                && !self.sampler
                && !for_draw
                && show_new_later()
                && is_texture_view(&view)
                && let Some(after) = begun_before(&view)
            {
                // Asked again (each draw that reads the slot, each pass)
                // while it waits: nothing more to do.
                let own = (
                    std::sync::Arc::as_ptr(&self.flip_identity) as usize,
                    index,
                    key,
                );
                if !NEW_PENDING
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(own)
                {
                    drop(publication);
                    drop(held);
                    self.unmake_stale(index, stale);
                    return Some((key, view));
                }
                let old = view_at(shown as u32, (shown >> 32) as u32);
                let kept = old.is_some_and(|old| match kin(&old, &view) {
                    Kin::Same => true,
                    kin => crate::resource::room_lately() && kin.kept(keep_shown()),
                });
                if shown == 0 || !kept {
                    #[cfg(feature = "diagnostics")]
                    note_before(index, shown, "neutral, new texture");
                    self.quick_is(index, 0);
                    self.made.set(index, 0);
                    let kept = neutral_for(&view, self.at(index));
                    self.put(index);
                    self.clear_twin(index);
                    self.keep(index, kept);
                }
                let late = Some(crate::frame::current());
                let flip = Flip {
                    after,
                    heap: std::sync::Arc::downgrade(&self.flip_identity),
                    index,
                    handle,
                    key,
                    version: flip_version(key),
                    late,
                    asked: crate::host::now_ms(),
                };
                queue_flip(flip);
                NEW_LATER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                drop(publication);
                drop(held);
                self.unmake_stale(index, stale);
                return Some((key, view));
            }
            #[cfg(feature = "diagnostics")]
            if !self.sampler && is_texture_view(&view) {
                note_before(index, shown, if later { "null, waits" } else { "at once" });
            }
            self.quick_is(index, 0);
            self.made.set(index, if later { 0 } else { key });
            let kept = if later {
                None
            } else {
                make_twinned_backing(&view, self.at(index), self.twin(index), &mut backing)
            };
            let installed = kept.is_some();
            if !later && requires_buffer(&view) {
                self.candidates.buffer(index, if kept.is_none() { indexed_form(&view) } else { 0 });
                if kept.is_some()
                    && indexed_form(&view) & 3 != 0
                    && let Some((shadow, address, len)) = backing.binding(&view)
                    && shadow.is_read_presentation()
                {
                    self.ordered_reads
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(index, (shadow, address, len));
                }
            }
            self.failed_buffers
                .set(index, !later && kept.is_none() && required_view(&view));
            if kept.is_none() && !self.sampler {
                null_for(&view, self.at(index));
            }
            if kept.is_some() || !self.sampler {
                self.put(index);
            }
            self.twin_made(index, kept.is_some() && twinned(&view));
            self.keep(index, kept.flatten());
            self.quick_is(
                index,
                if later || self.failed_buffers.may(index) {
                    0
                } else {
                    key
                },
            );
            if self.failed_buffers.may(index) {
                self.made.set(index, 0);
            }
            if let Some(note) = publication.as_mut().and_then(|guard| guard.note.as_mut()) {
                note.installed = installed && !self.failed_buffers.may(index);
            }
            drop(publication);
            drop(held);
            self.unmake_stale(index, stale);
        }
        Some((key, view))
    }

    /// Whether slot `index` was made from no record: what [`HostHeap::mirror`]
    /// would find before it did nothing for one that holds no view.
    pub(crate) fn shows_none(&self, index: u32) -> bool {
        self.made.get(index).unwrap_or(0) == 0
    }

    /// Slot `index` is made from record `key` and stays so (0: not). Under
    /// the lock of `made`.
    fn quick_is(&self, index: u32, key: u64) {
        if let Some(quick) = self.quick.get(index as usize) {
            quick.store(key, std::sync::atomic::Ordering::Release);
        }
    }

    /// Whether slot `index` is backed by a made host descriptor (tests).
    #[cfg(test)]
    pub(crate) fn made(&self, index: u32) -> bool {
        self.made.get(index).is_some_and(|key| key != 0)
    }

    /// Whether host slot `index` shows `buffer`; `None` if it shows no
    /// resource (diagnostics: a buffer view's slot must show the host copy
    /// that is brought up to date for it).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn shows(&self, index: u32, buffer: &ID3D12Resource) -> Option<bool> {
        let shown = self.kept_in(index)?;
        Some(windows::core::Interface::as_raw(&shown) == windows::core::Interface::as_raw(buffer))
    }

    /// What the queue of textures picked by index knows of title slot
    /// `index` at `frame`, in a few words (diagnostics).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn texture_slot_state(&self, index: u32, frame: u64) -> String {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        match textures.slots.get(&index) {
            None => "not queued".into(),
            Some(w) => format!(
                "queued due={} written@{} looks={} part_mapped={} recheck={}",
                if w.due <= frame { "now".to_string() } else { format!("@{}", w.due) },
                w.written,
                w.looks,
                w.mapped,
                w.checked.2 != 0
            ),
        }
    }

    /// The view host slot `index` was made from, in a few words, with the
    /// title and host addresses of a buffer view (diagnostics). "null" for
    /// a slot never made or left null.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn describe_slot(&self, index: u32) -> Option<String> {
        let key = self.made.get(index)?;
        if key == 0 {
            return Some("null".into());
        }
        let view = view_at(key as u32, (key >> 32) as u32)?;
        let mut text: String = format!("{view:?}").chars().take(120).collect();
        if let Some((address, len)) = buffer_range(&view) {
            let host = self.kept_in(index).map(|buffer| {
                // SAFETY: the published slot retains its native resource.
                unsafe { buffer.GetGPUVirtualAddress() }
            });
            text.push_str(&format!(" title {address:#x}+{len:#x} host buffer {host:#x?}"));
        }
        Some(text)
    }
}

/// Slots of this thread's own in heaps the card does not see, two of views
/// and two of samplers (`which` 0 or 1): descriptors are made there and
/// copied whole into the heaps it does see. Made in place they are seen half
/// written: an RX 7900 XTX saw a texture of no size in 4 % of its reads of
/// slots whose views a thread made over and over (the driver clears the
/// descriptor and fills it step by step), and none in 8,500 million with the
/// same descriptors copied, three times as fast
/// (`indirect::tests::descriptors_written_under_readers`). Title threads and
/// our passes rewrite slots while submissions run whose shaders pick them;
/// three device losses of 2026-11-25 (play-1125-0203, n02, n03) were reads of
/// memory no view names. `VICTORY_VIEW_COPY=0`: made in place, as before.
fn staged(sampler: bool, which: u32) -> Option<D3D12_CPU_DESCRIPTOR_HANDLE> {
    /// Threads at a time that have slots; more make views in place.
    const THREADS: u32 = 4096;
    struct Stage {
        starts: [usize; 2],
        steps: [usize; 2],
        _heaps: [ID3D12DescriptorHeap; 2],
    }
    // SAFETY: descriptor heaps are free-threaded; each thread has slots of
    // its own.
    unsafe impl Send for Stage {}
    // SAFETY: as above.
    unsafe impl Sync for Stage {}
    static STAGE: std::sync::OnceLock<Option<Stage>> = std::sync::OnceLock::new();
    /// Places given back, and how many were ever given out.
    static PLACES: Mutex<(Vec<u32>, u32)> = Mutex::new((Vec::new(), 0));
    struct Place(Option<u32>);
    impl Drop for Place {
        fn drop(&mut self) {
            if let Some(place) = self.0 {
                PLACES.lock().unwrap_or_else(|e| e.into_inner()).0.push(place);
            }
        }
    }
    thread_local! {
        static PLACE: Place = {
            let mut places = PLACES.lock().unwrap_or_else(|e| e.into_inner());
            Place(places.0.pop().or_else(|| {
                (places.1 < THREADS).then(|| {
                    places.1 += 1;
                    places.1 - 1
                })
            }))
        };
    }
    let stage = STAGE
        .get_or_init(|| {
            if std::env::var_os("VICTORY_VIEW_COPY").is_some_and(|v| v == "0") {
                return None;
            }
            let host = host()?;
            let make = |kind: D3D12_DESCRIPTOR_HEAP_TYPE| -> Option<ID3D12DescriptorHeap> {
                // SAFETY: plain heap creation.
                unsafe {
                    host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: 2 * THREADS,
                        ..Default::default()
                    })
                }
                .inspect_err(|e| tracing::warn!(error = %e, "no heap to make views in: they are made in place"))
                .ok()
            };
            let kinds = [D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER];
            let heaps = [make(kinds[0])?, make(kinds[1])?];
            // SAFETY: live heaps, plain queries.
            let (starts, steps) = unsafe {
                (
                    [0, 1].map(|i: usize| heaps[i].GetCPUDescriptorHandleForHeapStart().ptr),
                    kinds.map(|kind| host.device.GetDescriptorHandleIncrementSize(kind) as usize),
                )
            };
            Some(Stage { starts, steps, _heaps: heaps })
        })
        .as_ref()?;
    // (A thread that is ending has none any more.)
    let place = PLACE.try_with(|p| p.0).ok().flatten()?;
    let kind = usize::from(sampler);
    Some(D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: stage.starts[kind] + (2 * place + which) as usize * stage.steps[kind],
    })
}

/// Logs `what` the first time it happens.
macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A view description from its words, with the format as the host has it.
fn srv_desc(words: &[u32; 10]) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
            .read_unaligned()
    }
}

fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

/// A buffer view of title memory at `address` on its host copy: the first
/// element moves by the copy's offset.
/// Bytes per element of a buffer view.
fn element_size(desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    }
}

/// The title memory a buffer shader or unordered access view reads: its
/// address and length.
pub(crate) fn buffer_range(view: &View) -> Option<(u64, u64)> {
    let (base, words): (u64, &[u32; 10]) = match view {
        View::Shader(r, Some(desc)) => (
            crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64,
            desc,
        ),
        View::Placed(false, resource, desc) if resource[0] == 1 => (
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            desc[..10].try_into().ok()?,
        ),
        View::Unordered(r, _, Some(desc)) => {
            let base = crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64;
            return unordered_range(base, desc);
        }
        View::Placed(true, resource, desc) if resource[0] == 1 => {
            let base = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            return unordered_range(base, desc[..10].try_into().ok()?);
        }
        _ => return None,
    };
    let desc = srv_desc(words);
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    Some((
        base.checked_add(buffer.FirstElement.checked_mul(element)?)?,
        u64::from(buffer.NumElements) * element,
    ))
}

/// The address a shader finds in a descriptor of title memory: where a
/// buffer view starts, or the ray-tracing structure of a structure's view
/// (no resource: the view's description holds the address, and the shader
/// walks the structure from there itself).
pub(crate) fn view_address(view: &View) -> Option<u64> {
    view_span(view).map(|span| span.at)
}

pub(crate) fn view_span(view: &View) -> Option<crate::memory::ViewSpan> {
    use crate::memory::ViewSpan;
    const STRUCTURE: u32 = D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE.0 as u32;
    match view {
        View::Constant(at, len) => Some(ViewSpan { at: *at, bytes: Some(u64::from(*len)) }),
        View::Shader(_, Some(desc)) if desc[1] == STRUCTURE => {
            Some(ViewSpan { at: u64::from(desc[4]) | u64::from(desc[5]) << 32, bytes: None })
        }
        _ => buffer_range(view).map(|(at, len)| ViewSpan { at, bytes: Some(len) }),
    }
}

/// Enters the title address of `view` (0 for none, or a view that is no
/// buffer's or structure's) at title slot `index` of the view addresses
/// after the strides in the mapping at `data`
/// (`victory_shader::emit::VIEW_ADDRESSES`): shaders that pick a view at a
/// computed index and read its address as a pointer read it there.
fn write_view_address(data: usize, index: u32, view: Option<&View>) {
    use victory_shader::emit::{SLOT_STRIDES_COUNT, VIEW_ADDRESSES};
    if index >= SLOT_STRIDES_COUNT {
        return;
    }
    let address = view.and_then(view_address).unwrap_or(0);
    let at = (data + VIEW_ADDRESSES as usize + index as usize * 8) as *mut u64;
    // Upload mappings and each entry are eight-byte aligned. Publish the
    // whole pointer in one CPU store, including when a high word changes.
    unsafe {
        if at.is_aligned() {
            at.write_volatile(address);
        } else {
            at.write_unaligned(address);
        }
    }
}

/// The title memory the host slot of buffer view `view` is made on
/// (address, bytes): the whole buffer the view is of, which is what
/// [`make`] finds the host copy by. [`buffer_range`] is the part of it the
/// view reads.
pub(crate) fn buffer_extent(view: &View) -> Option<(u64, u64)> {
    match view {
        View::Shader(r, Some(_)) | View::Unordered(r, _, Some(_)) => {
            let (data, len) = crate::resource::buffer_bytes(*r as crate::object::This)?;
            Some((data as u64, len))
        }
        View::Placed(_, resource, desc) if resource[0] == 1 => Some((
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            u64::from(resource[4]) | u64::from(resource[5]) << 32,
        )),
        _ => None,
    }
}

/// Whether a buffer view is brought up to date in the host copy its slot
/// shows ([`buffer_copy`]). On by default; `VICTORY_VIEW_EXTENT=0` looks the
/// copy up by the view's range alone, as before.
fn view_extent() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_EXTENT").is_none_or(|v| v != "0"))
}

type BufferBinding = (std::sync::Arc<crate::shadow::Shadow>, u64, u64);

/// Keep the allocation and any read presentation together while publishing a
/// single descriptor. Its native slot, metadata and ordered reads must all use
/// the same bytes. Each new guest descriptor resolves its own backing.
#[derive(Default)]
struct ViewBacking {
    allocation: Option<(u64, u64, std::sync::Arc<crate::shadow::Shadow>)>,
    read: Option<BufferBinding>,
}

impl ViewBacking {
    fn allocation(&mut self, address: u64, bytes: u64) -> Option<std::sync::Arc<crate::shadow::Shadow>> {
        if let Some((base, len, shadow)) = &self.allocation
            && (*base, *len) == (address, bytes) { return Some(shadow.clone()); }
        let shadow = crate::shadow::find(address, bytes)?;
        self.allocation = Some((address, bytes, shadow.clone()));
        Some(shadow)
    }

    fn binding(&mut self, view: &View) -> Option<BufferBinding> {
        if let Some(read) = &self.read { return Some(read.clone()); }
        let read = resolve_buffer(view, self)?;
        self.read = Some(read.clone());
        Some(read)
    }
}

/// The host copy buffer view `view` reads, and the title range (address,
/// bytes) it reads there; `None` for other views and empty ranges.
///
/// The copy is the one the view's slot shows: [`make`] finds it by the
/// whole buffer the view is of ([`buffer_extent`]). Looked up by the range
/// the view reads, as it was, the copy is another one wherever the two
/// lookups differ: for memory outside buffer objects, a view of a part of a
/// buffer that crosses a window line (the buffer's window is two cells, the
/// part's one), and for buffer objects, a part that also lies in a smaller
/// buffer object. The slot then showed a copy nothing brought up to date,
/// and what shaders wrote through it was looked for in the other copy.
pub(crate) fn buffer_copy(view: &View) -> Option<BufferBinding> {
    resolve_buffer(view, &mut ViewBacking::default())
}

fn resolve_buffer(view: &View, backing: &mut ViewBacking) -> Option<BufferBinding> {
    if let &View::Constant(address, size) = view {
        return Some((constant_home(address, size, backing)?, address, u64::from(size)));
    }
    let (address, len) = buffer_range(view)?;
    if len == 0 {
        return None;
    }
    if view_extent()
        && let Some((base, width)) = buffer_extent(view)
    {
        let offset = address.checked_sub(base)?;
        if offset.checked_add(len)? > width {
            return None;
        }
        let shadow = backing.allocation(base, width)?;
        if len > shadow.left(address) {
            return None;
        }
        let writable = matches!(view, View::Unordered(..) | View::Placed(true, ..));
        let home = if writable {
            shadow
        } else {
            view_home(shadow, address, len, view_elements(view)?, srv_mapping(view))
        };
        return Some((home, address, len));
    }
    let shadow = backing.allocation(address, len)?;
    let writable = matches!(view, View::Unordered(..) | View::Placed(true, ..));
    let home = if writable {
        shadow
    } else {
        view_home(shadow, address, len, view_elements(view)?, srv_mapping(view))
    };
    Some((home, address, len))
}

/// [`buffer_range`] of an unordered access view of memory at `base`.
fn unordered_range(base: u64, words: &[u32; 10]) -> Option<(u64, u64)> {
    let desc = uav_desc(words);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32)?.0
    };
    Some((
        base.checked_add(buffer.FirstElement.checked_mul(element)?)?,
        u64::from(buffer.NumElements) * element,
    ))
}

/// The host resource a slot made by [`make`] views, if any.
type Kept = Option<ID3D12Resource>;

/// A structured buffer view (`first`, `count` elements of `stride` bytes,
/// `offset` bytes into its host copy) as a raw view of the same bytes:
/// its first dword and its dwords. Translated shaders read untyped buffers
/// as raw bytes at `element * stride + offset` (the stride from the
/// used-resource entry, or from the table by heap slot for a buffer picked
/// by index), and a raw read through a structured view gives zeros on
/// NVIDIA (reported upstream with PR #193; structured reads through raw
/// views were right there). So the host's view is raw whenever it can be:
/// the bytes must start on 16 (the host's raw view alignment) and be whole
/// dwords. A view that cannot stays structured.
fn raw_window(offset: u64, first: u64, count: u32, stride: u32) -> Option<(u64, u32)> {
    if stride == 0 {
        return None;
    }
    let start = first.checked_mul(u64::from(stride))?.checked_add(offset)?;
    let bytes = u64::from(count) * u64::from(stride);
    (start % 16 == 0 && bytes % 4 == 0)
        .then(|| u32::try_from(bytes / 4).ok().map(|dwords| (start / 4, dwords)))
        .flatten()
}

/// Whether the host accepts a buffer view with these fields of a buffer of
/// `width` bytes. It removes the device for one it does not (view creation
/// returns nothing), and the console is laxer: the title's heap holds
/// views past their memory's end, of no elements, of typeless formats
/// without the raw flag, with strides the host has no structured buffers
/// for. Since every buffer view of the heap is made for shaders that pick
/// them by index, one such view ended the run (2026-11-24, run 10).
fn buffer_view_ok(format: DXGI_FORMAT, first: u64, count: u32, stride: u32, raw: bool, width: u64) -> bool {
    use crate::resource::family;
    let f = format.0 as u32;
    let element = if raw {
        // raw: R32 typeless, counted in dwords, starting on 16 bytes
        if format != DXGI_FORMAT_R32_TYPELESS || stride != 0 || !first.is_multiple_of(4) {
            return false;
        }
        4
    } else if stride > 0 {
        // structured: no format, a stride the host takes
        if format != DXGI_FORMAT_UNKNOWN || stride > 2048 || !stride.is_multiple_of(4) {
            return false;
        }
        u64::from(stride)
    } else {
        // typed: a format with a size that is not typeless
        let typeless = family(f) == f && family(f + 1) == f;
        match crate::resource::element(f) {
            Some((bytes, 1)) if f != 0 && !typeless && bytes > 0 => bytes,
            _ => return false,
        }
    };
    count > 0
        && first
            .checked_add(u64::from(count))
            .and_then(|end| end.checked_mul(element))
            .is_some_and(|end| end <= width)
}

/// Raw twins ([`raw_twins`]) made since the last frame line, and those of them that
/// do not begin on 16 bytes, as a raw view must ([`move_raw`]), and those of
/// these that could not be moved there (null).
pub(crate) static TWINS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Moves the raw view at `at`, a slot of our heaps the card does not see,
/// which shows `bytes` from `start` on (GPU address), `by` bytes on: for a
/// twin whose view begins between two multiples of 16, where the host makes
/// no raw view. An AMD descriptor of a raw buffer is its address (48 bits),
/// its bytes and flags; only one that says just that is changed, and only
/// where what the handle points at is the descriptor. False: not moved.
pub(crate) fn move_raw(at: D3D12_CPU_DESCRIPTOR_HANDLE, start: u64, bytes: u32, by: u32) -> bool {
    // (Off dwords too, as the typed view it stands for begins there: a view
    // of 16 bit values at an odd one.)
    if by >= bytes {
        return false;
    }
    let mut words = [0u8; 16];
    if !crate::host::read_into(at.ptr as *const u8, &mut words) {
        return false;
    }
    let word = |i: usize| u32::from_le_bytes([words[4 * i], words[4 * i + 1], words[4 * i + 2], words[4 * i + 3]]);
    let address = u64::from(word(0)) | u64::from(word(1)) << 32;
    if address != start || word(2) != bytes || start >> 48 != 0 {
        return false;
    }
    let moved = start + u64::from(by);
    // SAFETY: this thread's own slot (it has slots of its own: the caller
    // asks only for those), read just now.
    unsafe {
        let words = at.ptr as *mut u32;
        words.write(moved as u32);
        words.add(1).write((moved >> 32) as u32);
        words.add(2).write(bytes - by);
    }
    true
}

/// The bytes a buffer view the host takes ([`buffer_view_ok`]) shows as a raw
/// view (first dword, dwords, and the bytes past that dword it begins: a raw
/// view begins on 16 bytes); of a buffer of `width` bytes.
fn twin_window(format: DXGI_FORMAT, first: u64, count: u32, stride: u32, raw: bool, width: u64) -> Option<(u64, u32, u32)> {
    use std::sync::atomic::Ordering::Relaxed;
    TWINS[0].fetch_add(1, Relaxed);
    let element = if raw {
        4
    } else if stride > 0 {
        u64::from(stride)
    } else {
        crate::resource::element(format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    let (start, end) = (first * element, ((first + u64::from(count)) * element).next_multiple_of(4));
    let end = end.min(width & !3);
    let from = start & !15;
    if end <= start {
        return None;
    }
    let dwords = u32::try_from((end - from) / 4).ok()?;
    if from != start {
        TWINS[1].fetch_add(1, Relaxed);
    }
    Some((from / 4, dwords, (start - from) as u32))
}

fn buffer_srv(
    address: u64,
    len: u64,
    mut desc: D3D12_SHADER_RESOURCE_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    twin: Option<D3D12_CPU_DESCRIPTOR_HANDLE>,
    backing: &mut ViewBacking,
) -> Option<Kept> {
    let mut shadow = backing.allocation(address, len)?;
    let mut offset = shadow.offset(address);
    let unit = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let own = unsafe { &mut desc.Anonymous.Buffer };
    let start = address.checked_add(own.FirstElement.checked_mul(unit)?)?;
    let kind = (
        own.StructureByteStride,
        own.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0,
        desc.Format.0 as u32,
    );
    let bytes = u64::from(own.NumElements).checked_mul(unit)?;
    let relative = start.checked_sub(address)?;
    if relative.checked_add(bytes)? > len || bytes > shadow.left(start) {
        return None;
    }
    let mapping = desc.Shader4ComponentMapping;
    let home = backing.read.as_ref().filter(|(_, at, len)| view_extent() && (*at, *len) == (start, bytes))
        .map_or_else(|| view_home(shadow.clone(), start, bytes, kind, mapping), |(s, _, _)| s.clone());
    if !std::sync::Arc::ptr_eq(&home, &shadow) {
        // Its own bytes: it begins where they do.
        (shadow, offset) = (home, 0);
        own.FirstElement = 0;
    }
    // A structured view as a raw view of the same bytes (see `raw_window`).
    // SAFETY: a buffer view's union member.
    let structured = unsafe { desc.Anonymous.Buffer };
    let rgb32 = mapped_rgb32(shadow.offset(start), bytes, kind, mapping);
    let window = canonical_raw(shadow.offset(start), bytes, kind)
        .map(|(first, words, _)| (first, words))
        .or(rgb32)
        .or_else(|| (desc.Format == DXGI_FORMAT_UNKNOWN)
            .then(|| raw_window(offset, structured.FirstElement, structured.NumElements, structured.StructureByteStride))
            .flatten());
    if let Some((first, count)) = window {
        desc.Format = DXGI_FORMAT_R32_TYPELESS;
        desc.Anonymous.Buffer = D3D12_BUFFER_SRV {
            FirstElement: first,
            NumElements: count,
            StructureByteStride: 0,
            Flags: D3D12_BUFFER_SRV_FLAG_RAW,
        };
    }
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let begin = shadow.offset(start);
    let native_typed = canonical_typed(begin, bytes, kind);
    if native_typed.is_some() {
        // Checked by byte range below; validate its format and count as a
        // zero-based descriptor separately from the actual native start.
        buffer.FirstElement = 0;
    } else if window.is_none() {
        if element == 0 || offset % element != 0 {
            once!(
                offset,
                element,
                "buffer view not on an element of its host copy"
            );
            return None;
        }
        buffer.FirstElement = buffer.FirstElement.checked_add(offset / element)?;
    }
    let (first, count, stride) = (buffer.FirstElement, buffer.NumElements, buffer.StructureByteStride);
    let raw = buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0;
    let width = shadow.width();
    if !buffer_view_ok(desc.Format, first, count, stride, raw, width) {
        REFUSED_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        once!(
            format = desc.Format.0,
            first,
            count,
            stride,
            raw,
            width,
            "buffer view the host would refuse left null"
        );
        return None;
    }
    // D3D12 encodes four three-bit selectors and a required bit at 12.
    // Keep explicit swizzles and zero/one selectors on typed views.
    let mapping = desc.Shader4ComponentMapping;
    if mapping >> 12 != 1 || (0..4).any(|i| ((mapping >> (3 * i)) & 7) > 5) {
        desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    if let Some(device) = native_typed {
        device.srv(&shadow.buffer, desc.Format, desc.Shader4ComponentMapping, begin, bytes, at)?;
    } else {
        unsafe { host.device.CreateShaderResourceView(&shadow.buffer, Some(&desc), at) };
    }
    #[cfg(feature = "diagnostics")]
    if rgb32.is_some() {
        static FIRST: std::sync::Once = std::sync::Once::new();
        FIRST.call_once(|| tracing::info!(start = format_args!("{start:#x}"), bytes, "RGB32 sampled buffer uses the writer's native allocation"));
    }
    if let Some(twin) = twin {
        let window = if native_typed.is_some() {
            Some((begin / 4, u32::try_from(bytes / 4).ok()?, 0))
        } else {
            twin_window(desc.Format, first, count, stride, raw, width)
        };
        if let Some((first, count, by)) = window {
            desc.Format = DXGI_FORMAT_R32_TYPELESS;
            desc.Anonymous.Buffer = D3D12_BUFFER_SRV {
                FirstElement: first,
                NumElements: count,
                StructureByteStride: 0,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
            };
            // SAFETY: a live buffer and a slot of our heap.
            let start = unsafe {
                host.device
                    .CreateShaderResourceView(&shadow.buffer, Some(&desc), twin);
                shadow.buffer.GetGPUVirtualAddress() + first * 4
            };
            if by > 0 && !(staged(false, 1).is_some_and(|s| s.ptr == twin.ptr) && move_raw(twin, start, count.saturating_mul(4), by)) {
                TWINS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                null_buffer_view(twin);
            }
        } else {
            null_buffer_view(twin);
        }
    }
    Some(Some(shadow.buffer.clone()))
}

/// An unordered access view of title buffer memory at `address` (`len`
/// bytes) on its host copy, as [`buffer_srv`]; false if the copy cannot be
/// written by shaders (upload memory).
/// shortcut: what shaders write stays in the host copy; title memory (and
/// so the CPU, and copies run on the CPU) does not see it.
fn buffer_uav(
    address: u64,
    len: u64,
    mut desc: D3D12_UNORDERED_ACCESS_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    twin: Option<D3D12_CPU_DESCRIPTOR_HANDLE>,
    backing: &mut ViewBacking,
) -> Option<Kept> {
    let shadow = backing.allocation(address, len)?;
    let unit = {
        // SAFETY: a buffer view's union member.
        let b = unsafe { desc.Anonymous.Buffer };
        if b.StructureByteStride > 0 {
            u64::from(b.StructureByteStride)
        } else if b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
            4
        } else {
            crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
        }
    };
    // SAFETY: a buffer view's union member.
    let own = unsafe { &mut desc.Anonymous.Buffer };
    let start = address.checked_add(own.FirstElement.checked_mul(unit)?)?;
    let bytes = u64::from(own.NumElements).checked_mul(unit)?;
    let relative = start.checked_sub(address)?;
    if relative.checked_add(bytes)? > len || bytes > shadow.left(start) {
        return None;
    }
    // Writable views always reference the original allocation. A separate
    // byte copy cannot preserve concurrent aliases or an atomic return.
    // SAFETY: a live buffer.
    let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
    if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
        once!("unordered access to a buffer whose host copy shaders cannot write");
        return None;
    }
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    let offset = shadow.offset(address);
    // A structured view as a raw view of the same bytes (see `raw_window`).
    let kind = (buffer.StructureByteStride, buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0, desc.Format.0 as u32);
    let begin = shadow.offset(start);
    let native_typed = canonical_typed(begin, bytes, kind);
    let window = canonical_raw(shadow.offset(start), bytes, kind)
        .map(|(first, words, _)| (first, words))
        .or_else(|| (desc.Format == DXGI_FORMAT_UNKNOWN)
            .then(|| raw_window(offset, buffer.FirstElement, buffer.NumElements, buffer.StructureByteStride))
            .flatten());
    if let Some((first, count)) = window {
        buffer.FirstElement = first;
        buffer.NumElements = count;
        buffer.StructureByteStride = 0;
        buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
    } else if native_typed.is_some() {
        buffer.FirstElement = 0;
    } else {
        if element == 0 || offset % element != 0 {
            once!(offset, element, "unordered buffer view not on an element of its host copy");
            return None;
        }
        buffer.FirstElement = buffer.FirstElement.checked_add(offset / element)?;
    }
    if buffer.CounterOffsetInBytes != 0 { return None; }
    let (first, count, stride) = (buffer.FirstElement, buffer.NumElements, buffer.StructureByteStride);
    let raw = buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0;
    if window.is_some() {
        // (Left out at first: every structured unordered access view was then
        // refused by `buffer_view_ok` and left null, runs 15 and 16.)
        desc.Format = DXGI_FORMAT_R32_TYPELESS;
    }
    // SAFETY: a live buffer.
    let width = unsafe { shadow.buffer.GetDesc() }.Width;
    if !buffer_view_ok(desc.Format, first, count, stride, raw, width) {
        REFUSED_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        once!(
            format = desc.Format.0,
            first,
            count,
            stride,
            raw,
            width,
            "unordered buffer view the host would refuse left null"
        );
        return None;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    if let Some(device) = native_typed {
        device.uav(&shadow.buffer, desc.Format, begin, bytes, at)?;
    } else {
        unsafe { host.device.CreateUnorderedAccessView(&shadow.buffer, None::<&ID3D12Resource>, Some(&desc), at) };
    }
    if let Some(twin) = twin {
        let window = if native_typed.is_some() {
            Some((begin / 4, u32::try_from(bytes / 4).ok()?, 0))
        } else {
            twin_window(desc.Format, first, count, stride, raw, width)
        };
        if let Some((first, count, by)) = window {
            desc.Format = DXGI_FORMAT_R32_TYPELESS;
            desc.Anonymous.Buffer = D3D12_BUFFER_UAV {
                FirstElement: first,
                NumElements: count,
                StructureByteStride: 0,
                CounterOffsetInBytes: 0,
                Flags: D3D12_BUFFER_UAV_FLAG_RAW,
            };
            // SAFETY: a live buffer and a slot of our heap.
            let start = unsafe {
                host.device
                    .CreateUnorderedAccessView(&shadow.buffer, None::<&ID3D12Resource>, Some(&desc), twin);
                shadow.buffer.GetGPUVirtualAddress() + first * 4
            };
            if by > 0 && !(staged(false, 1).is_some_and(|s| s.ptr == twin.ptr) && move_raw(twin, start, count.saturating_mul(4), by)) {
                TWINS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                null_buffer_view(twin);
            }
        } else {
            null_buffer_view(twin);
        }
    }
    Some(Some(shadow.buffer.clone()))
}

/// Whether the host can make a shader (or unordered access) view of
/// `texture` in `format` (none: the texture's own). The console allows any
/// view; the host refuses (and removes the device for) views without the
/// resource's permission, of typed depth, and in another format than a
/// typed resource's. A view of a typed texture in another format of its
/// family takes the texture's format.
/// shortcut: such a view (sRGB of UNORM, say) reads without its
/// conversion; cast with castable formats if colors come out wrong.
fn fits(texture: &ID3D12Resource, mut format: Option<&mut DXGI_FORMAT>, unordered: bool) -> bool {
    use crate::resource::{DEPTH_FORMATS, family};
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let allowed = if unordered {
        d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
    } else {
        d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0
    };
    let own = d.Format.0 as u32;
    if !allowed || DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
        if unordered {
            crate::resource::DEPTH_UNORDERED[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        once!(
            unordered,
            format = own,
            flags = d.Flags.0,
            "view the host does not allow"
        );
        return false;
    }
    // Every family has more than one format, its typeless one first.
    let typeless = family(own) == own && family(own + 1) == own;
    if let Some(format) = format.as_deref_mut()
        && unordered
        && let Some(&(_, linear)) = crate::resource::SRGB
            .iter()
            .find(|&&(s, _)| s == format.0 as u32)
    {
        format.0 = linear as i32;
    }
    let view = format.as_ref().map_or(0, |f| f.0 as u32);
    if view == 0 {
        return !typeless;
    }
    // A view of a target whose format the host refuses, in the title's
    // format: in the stand-in the host copy has.
    if view != own && crate::resource::target_format(view) == own {
        if let Some(format) = format {
            *format = d.Format;
        }
        return true;
    }
    if family(view) != family(own) {
        once!(view, own, "view format of another family");
        return false;
    }
    if let Some(format) = format
        && !typeless
    {
        *format = d.Format;
    }
    true
}

/// The title's unordered access view of a depth texture, as a view of the
/// texture's colour twin `twin` ([`crate::resource::Twin`]): the same mip
/// and slices, in the colour format of the twin's family for a depth or
/// typeless format (R16_UNORM for 16-bit depth, R32_FLOAT for 32-bit; the
/// bits are the same). Without a description: mip 0 of every slice.
/// The twin of a stencil plane (R8) stands for the views of that plane
/// only, which become R8_UINT views of its one plane; `None` for a view of
/// the depth plane, which the twin does not stand for. The depth plane has
/// a twin of its own, viewed as a depth texture's without stencil: a view
/// in the format of the depth plane of a depth-stencil format (21, 46)
/// takes the twin's colour format too.
fn twin_uav(
    twin: &ID3D12Resource,
    desc: Option<&D3D12_UNORDERED_ACCESS_VIEW_DESC>,
) -> Option<D3D12_UNORDERED_ACCESS_VIEW_DESC> {
    // SAFETY: a live resource.
    let d = unsafe { twin.GetDesc() };
    let mut desc = desc.cloned().unwrap_or_else(|| {
        let mut all = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        if d.DepthOrArraySize > 1 {
            all.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            all.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: u32::from(d.DepthOrArraySize),
                ..Default::default()
            };
        }
        all
    });
    // A stencil plane's twin (R8 typeless, 60): the title's view is of the
    // stencil plane if it names plane 1 or a stencil view format (22, 47).
    if d.Format.0 == 60 {
        let mut plane = 0;
        match desc.ViewDimension {
            D3D12_UAV_DIMENSION_TEXTURE2D => {
                // SAFETY: the member the dimension names.
                let mut view = unsafe { desc.Anonymous.Texture2D };
                plane = std::mem::take(&mut view.PlaneSlice);
                desc.Anonymous.Texture2D = view;
            }
            D3D12_UAV_DIMENSION_TEXTURE2DARRAY => {
                // SAFETY: the member the dimension names.
                let mut view = unsafe { desc.Anonymous.Texture2DArray };
                plane = std::mem::take(&mut view.PlaneSlice);
                desc.Anonymous.Texture2DArray = view;
            }
            _ => {}
        }
        if plane != 1 && !matches!(desc.Format.0, 22 | 47) {
            return None;
        }
        desc.Format.0 = 62;
        return Some(desc);
    }
    // None, typeless and depth formats (R16: 0, 53, 55; R32: 0, 39, 40),
    // and those of a depth-stencil format's depth plane (19 to 21, 44 to
    // 46), for the twin of such a plane.
    desc.Format.0 = match (desc.Format.0, d.Format.0) {
        (0 | 53 | 55 | 19..=21 | 44..=46, 53) => 56,
        (0 | 39 | 40 | 19..=21 | 44..=46, 39) => 41,
        (other, _) => other,
    };
    Some(desc)
}

/// The bytes of the host's constant buffer view for a title's of `size`:
/// whole 256s, and at most the 64 KiB a shader can index. The host removes
/// the device for a larger view, and the title's heap holds such sizes in
/// slots it does not use.
fn constant_home(address: u64, size: u32, backing: &mut ViewBacking) -> Option<std::sync::Arc<crate::shadow::Shadow>> {
    let mapped = victory_shader::emit::mapped_views();
    if size == 0 || !mapped && size > 64 << 10 {
        return None;
    }
    let len = u64::from(size);
    let shadow = backing.allocation(address, len)?;
    if len > shadow.left(address) {
        return None;
    }
    let direct = if mapped {
        canonical_constants(&shadow, address, size)
    } else {
        shadow.gpu(address).is_multiple_of(256)
            && shadow
                .offset(address)
                .checked_add(u64::from(constant_bytes(size)))?
                <= shadow.width()
    };
    if direct {
        Some(shadow)
    } else {
        crate::shadow::exact(&shadow, address, len)
    }
}

pub(crate) fn canonical_constants(shadow: &crate::shadow::Shadow, address: u64, size: u32) -> bool {
    raw_constant_window(shadow.offset(address), size, shadow.width()).is_some()
}

/// The raw descriptor begins at a legal host offset. The shader applies
/// the prefix and checks each dword against the original guest range.
fn raw_constant_window(offset: u64, size: u32, width: u64) -> Option<(u64, u32, u32)> {
    let bias = (offset & 15) as u32;
    let bytes = bias.checked_add(size)?;
    let start = offset - u64::from(bias);
    let words = bytes.div_ceil(4);
    (size > 0 && start.checked_add(u64::from(words) * 4)? <= width).then_some((
        start / 4,
        words,
        bias,
    ))
}

fn raw_constants(
    buffer: &ID3D12Resource,
    gpu: u64,
    size: u32,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Option<u32> {
    let host = host()?;
    // SAFETY: the caller holds the resource through descriptor publication.
    let (base, width) = unsafe { (buffer.GetGPUVirtualAddress(), buffer.GetDesc().Width) };
    let offset = gpu.checked_sub(base)?;
    let (first, words, bias) = raw_constant_window(offset, size, width)?;
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                FirstElement: first,
                NumElements: words,
                StructureByteStride: 0,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
            },
        },
    };
    unsafe {
        host.device
            .CreateShaderResourceView(buffer, Some(&desc), at)
    };
    Some(bias)
}

fn constant_bytes(size: u32) -> u32 {
    const LARGEST: u32 = 64 << 10;
    size.min(LARGEST).next_multiple_of(256)
}

/// Renderer port 5 ([`crate::port_cbv`]): the constant buffer of `size` bytes
/// at `gpu` of `buffer` as a constant buffer view at `at` where it can be (on
/// 256 bytes; else a null one) and a raw view of its bytes at `twin`, which
/// translated shaders read. `false` (nothing made): not on 16 bytes.
fn constant_twinned(
    buffer: &ID3D12Resource,
    gpu: u64,
    size: u32,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    twin: Option<D3D12_CPU_DESCRIPTOR_HANDLE>,
) -> bool {
    let Some(host) = host() else { return false };
    // SAFETY: a live buffer.
    let (base, width) = unsafe { (buffer.GetGPUVirtualAddress(), buffer.GetDesc().Width) };
    let Some(offset) = gpu.checked_sub(base).filter(|&o| o < width && crate::port_cbv::raw_ok(o)) else {
        return false;
    };
    let room = width - offset;
    let cbv = if gpu.is_multiple_of(256) && room >= 256 {
        D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: gpu,
            SizeInBytes: constant_bytes(size).min((room & !255) as u32),
        }
    } else {
        D3D12_CONSTANT_BUFFER_VIEW_DESC::default()
    };
    let bytes = u64::from(constant_bytes(size)).min(room) & !3;
    // SAFETY: slots of our heaps; the ranges lie in the live buffer.
    unsafe {
        host.device.CreateConstantBufferView(Some(&cbv), at);
        if let Some(twin) = twin {
            let raw = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        FirstElement: offset / 4,
                        NumElements: (bytes / 4) as u32,
                        StructureByteStride: 0,
                        Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                    },
                },
            };
            if bytes == 0 {
                null_buffer_view(twin);
            } else {
                host.device.CreateShaderResourceView(buffer, Some(&raw), twin);
            }
            crate::port_cbv::twinned();
        }
    }
    true
}

/// Makes the host descriptor for `view` at `at`: the host resource it
/// views, or `None` if it cannot.
fn make(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Option<Kept> {
    make_twinned(view, at, None)
}

/// [`make`]; `twin`: where the raw twin of a buffer view goes
/// ([`raw_twins`]; a constant buffer's is the same view again, other views
/// have none).
fn make_twinned(
    view: &View,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    twin: Option<D3D12_CPU_DESCRIPTOR_HANDLE>,
) -> Option<Kept> {
    make_twinned_backing(view, at, twin, &mut ViewBacking::default())
}

fn make_twinned_backing(
    view: &View,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    twin: Option<D3D12_CPU_DESCRIPTOR_HANDLE>,
    backing: &mut ViewBacking,
) -> Option<Kept> {
    let host = host()?;
    let device = &host.device;
    match view {
        View::Shader(0, Some(words)) if words[1] == 1 => {
            empty_buffer_view(false, words, at, twin);
            Some(None)
        }
        View::Unordered(0, 0, Some(words)) if words[1] == 1 => {
            empty_buffer_view(true, words, at, twin);
            Some(None)
        }
        View::Shader(0, Some(words)) if explicit_null_texture(view) => {
            let desc = srv_desc(words);
            unsafe { device.CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at) };
            Some(None)
        }
        View::Unordered(0, 0, Some(words)) if explicit_null_texture(view) => {
            let desc = uav_desc(words);
            unsafe { device.CreateUnorderedAccessView(None::<&ID3D12Resource>, None::<&ID3D12Resource>, Some(&desc), at) };
            Some(None)
        }
        &View::Constant(address, size) => {
            let (shadow, _, _) = backing.binding(view)?;
            let gpu = shadow.gpu(address);
            if victory_shader::emit::mapped_views() {
                raw_constants(&shadow.buffer, gpu, size, at)?;
                return Some(Some(shadow.buffer.clone()));
            }
            if crate::port_cbv::on() {
                // Renderer port 5: shaders read constant buffers as raw bytes
                // through the twin; a constant buffer view there would be read
                // raw, with no end on AMD.
                return constant_twinned(&shadow.buffer, gpu, size, at, twin).then(|| Some(shadow.buffer.clone()));
            }
            if gpu % 256 != 0 {
                once!(
                    address = format_args!("{address:#x}"),
                    "constant buffer not 256-byte aligned on the host"
                );
                return None;
            }
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: gpu,
                SizeInBytes: constant_bytes(size),
            };
            // SAFETY: slots of our heap; the range lies in the host copy.
            unsafe {
                device.CreateConstantBufferView(Some(&desc), at);
                if let Some(twin) = twin {
                    device.CreateConstantBufferView(Some(&desc), twin);
                }
            }
            Some(Some(shadow.buffer.clone()))
        }
        View::Shader(resource, desc) => {
            let resource = *resource as crate::object::This;
            if let Some(copy) = crate::resource::host_copy(resource) {
                let texture = &copy.texture;
                // The title's views of textures with a stencil plane, as it
                // describes them (format, dimension, component mapping, a
                // pad, then first level, levels, plane): each distinct one
                // once. They say how its shaders read the stencil plane
                // (the lighting takes each pixel's material class and its
                // "inside a room" bit from it).
                #[cfg(feature = "diagnostics")]
                {
                    // SAFETY: a live resource.
                    let own = unsafe { texture.GetDesc() }.Format.0;
                    if matches!(own, 19..=22 | 44..=47) {
                        static SEEN: Mutex<Vec<(i32, Option<[u32; 10]>)>> = Mutex::new(Vec::new());
                        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
                        if seen.len() < 64 && !seen.contains(&(own, *desc)) {
                            seen.push((own, *desc));
                            tracing::info!(
                                ?resource,
                                texture_format = own,
                                view = ?desc,
                                "shader view of a depth-stencil texture"
                            );
                        }
                    }
                }
                let mut desc = desc.as_ref().map(srv_desc);
                let packed_hdr = copy.packed_hdr_srv && desc.as_ref().is_some_and(|d| d.Format == DXGI_FORMAT_R9G9B9E5_SHAREDEXP);
                if !packed_hdr && !fits(texture, desc.as_mut().map(|d| &mut d.Format), false) {
                    return None;
                }
                if copy.base != 0 {
                    // A shortened allocation is valid only when it contains the complete
                    // original view. Relative LOD zero must still name the guest's first mip.
                    let desc = desc.as_mut()?;
                    if desc.ViewDimension != D3D12_SRV_DIMENSION_TEXTURE2D
                        || unsafe { desc.Anonymous.Texture2D.MostDetailedMip } < copy.base
                    {
                        return None;
                    }
                    from_base(desc, copy.base, u32::from(unsafe { texture.GetDesc() }.MipLevels));
                }
                // SAFETY: a live texture and a slot of our heap.
                unsafe {
                    device.CreateShaderResourceView(
                        texture,
                        desc.as_ref().map(std::ptr::from_ref),
                        at,
                    )
                };
                Some(Some(texture.clone()))
            } else if let (Some((data, len)), Some(desc)) =
                (crate::resource::buffer_bytes(resource), desc)
            {
                buffer_srv(data as u64, len, srv_desc(desc), at, twin, backing)
            } else {
                None
            }
        }
        View::Unordered(_, counter, Some(desc)) if *counter != 0 && desc[1] == 1 => None,
        View::Unordered(resource, _counter, desc) => {
            let resource = *resource as crate::object::This;
            let Some(texture) = crate::resource::host_resource(resource) else {
                return match (crate::resource::buffer_bytes(resource), desc) {
                    (Some((data, len)), Some(desc)) => {
                        buffer_uav(data as u64, len, uav_desc(desc), at, twin, backing)
                    }
                    _ => None,
                };
            };
            // The first such views of depth textures are logged as the
            // title describes them (format, dimension, then mip, slices and
            // plane): they say which planes shaders write, and as what.
            if crate::resource::depth_and_unordered(resource) {
                static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 512 {
                    tracing::info!(
                        ?resource,
                        view = ?desc,
                        "unordered access view of a depth texture"
                    );
                }
            }
            let mut desc = desc.as_ref().map(uav_desc);
            // A depth texture's unordered access is its colour twin's: the
            // host gives a depth texture none.
            let twin = crate::resource::twin_resource(resource)
                .and_then(|twin| Some((twin, twin_uav(twin, desc.as_ref())?)))
                // A view of the depth plane of a texture with a stencil
                // plane: of that plane's own twin.
                .or_else(|| {
                    let twin = crate::resource::twin_plane_resource(resource)?;
                    Some((twin, twin_uav(twin, desc.as_ref())?))
                });
            let texture = match twin {
                Some((twin, view)) => {
                    desc = Some(view);
                    twin
                }
                None => texture,
            };
            if !fits(texture, desc.as_mut().map(|d| &mut d.Format), true) {
                return None;
            }
            // SAFETY: a live texture and a slot of our heap.
            unsafe {
                device.CreateUnorderedAccessView(
                    texture,
                    None::<&ID3D12Resource>,
                    desc.as_ref().map(std::ptr::from_ref),
                    at,
                )
            };
            Some(Some(texture.clone()))
        }
        View::Sampler(words) => {
            // SAFETY: 13 words in the host's layout.
            let desc = unsafe {
                std::ptr::from_ref(words)
                    .cast::<D3D12_SAMPLER_DESC>()
                    .read_unaligned()
            };
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(None)
        }
        View::ConsoleSampler(words) => {
            let desc = crate::sampler::console(words);
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some(None)
        }
        View::Placed(unordered, resource, desc) => {
            if let Some(view) = placed_texture(view) {
                return make(&view, at);
            }
            const BUFFER: u32 = 1;
            if resource[0] != BUFFER {
                // shortcut: unordered access to a texture the title placed
                // itself is not mirrored; give it a resource as shader
                // views of such textures get when a dispatch writes one.
                // Each texture is said once (the first 64), with how many times
                // such a view was asked for so far: what shaders write through
                // them is lost.
                static SAID: Mutex<std::collections::BTreeSet<u64>> = Mutex::new(std::collections::BTreeSet::new());
                static TIMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let times = TIMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                let at = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
                if (said.len() < 64 && said.insert(at)) || times.is_power_of_two() {
                    tracing::warn!(
                        address = format_args!("{at:#x}"),
                        times,
                        "placed unordered texture view not mirrored (null: writes through it are lost): resource {resource:?}, view {:?}",
                        &desc[..10]
                    );
                }
                return None;
            }
            // The resource description's width, after the alignment.
            let len = u64::from(resource[4]) | u64::from(resource[5]) << 32;
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            // A zero-element view exposes no bytes, even when its base address
            // is a valid allocation. The title's culling dispatch binds these
            // for empty sets. Publish the null in both possible presentations.
            if words[1] == BUFFER && (if *unordered { words[4] } else { words[6] }) == 0 {
                empty_buffer_view(*unordered, words, at, twin);
                return Some(None);
            }
            if len == 0 { return None; }
            if *unordered {
                buffer_uav(address, len, uav_desc(words), at, twin, backing)
            } else {
                buffer_srv(address, len, srv_desc(words), at, twin, backing)
            }
        }
    }
}

/// The view `desc` of a texture, of a host copy of its levels from `base`
/// on (`crate::resource::held_only`). What it shows of larger ones, if any,
/// is left out.
fn from_base(desc: &mut D3D12_SHADER_RESOURCE_VIEW_DESC, base: u32, own_mips: u32) {
    let clamp_mips = |most: &mut u32, levels: &mut u32, lod: &mut f32| {
        let lost = base.saturating_sub(*most);
        *most = most.saturating_sub(base);
        if own_mips > 0 {
            *most = (*most).min(own_mips - 1);
        }
        if *levels != u32::MAX {
            *levels = levels.saturating_sub(lost).max(1);
            if own_mips > 0 {
                *levels = (*levels).min(own_mips.saturating_sub(*most)).max(1);
            }
        }
        *lod = (*lod - base as f32).max(0.0);
    };
    match desc.ViewDimension {
        D3D12_SRV_DIMENSION_TEXTURE2D => {
            // SAFETY: the member its dimension says.
            let t = unsafe { &mut desc.Anonymous.Texture2D };
            clamp_mips(&mut t.MostDetailedMip, &mut t.MipLevels, &mut t.ResourceMinLODClamp);
        }
        D3D12_SRV_DIMENSION_TEXTURE2DARRAY => {
            // SAFETY: the member its dimension says.
            let t = unsafe { &mut desc.Anonymous.Texture2DArray };
            clamp_mips(&mut t.MostDetailedMip, &mut t.MipLevels, &mut t.ResourceMinLODClamp);
        }
        _ => {}
    }
}

/// Writes of a slot with another view of the placed texture it showed,
/// since the start: the slot kept showing the old view.
pub(crate) static KEPT_VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether a slot written with another view of the placed texture it
/// shows keeps showing the old one until the new is made;
/// `VICTORY_KEEP_VIEWS=0` makes it null at once, as before.
fn keep_views() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_KEEP_VIEWS").is_none_or(|v| v != "0"))
}

/// What the texture view a slot is written with has to do with the one it
/// shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kin {
    /// A view of the same placed texture ([`same_placed_texture`]).
    Same,
    /// A texture of the same description placed elsewhere (moved, or one of
    /// its kind).
    Moved,
    /// One of the same format and shape in another size: the same texture
    /// with more or fewer levels, as a title streams it.
    Sized,
    /// A view of the same dimension, of another texture.
    Other,
    /// A view no shader that reads the one can read.
    None,
}

impl Kin {
    /// Whether the slot goes on showing what it shows, by [`keep_shown`].
    fn kept(self, keep: u8) -> bool {
        match self {
            Kin::Same => true,
            Kin::Moved | Kin::Sized => keep >= 1,
            Kin::Other => keep >= 2,
            Kin::None => false,
        }
    }
}

/// [`Kin`] of the view `old`, which a slot shows, and `new`.
fn kin(old: &View, new: &View) -> Kin {
    if same_placed_texture(old, new) {
        return Kin::Same;
    }
    if let (View::Placed(false, a, from), View::Placed(false, b, to)) = (old, new) {
        if a[0] == 1 || b[0] == 1 || from[1] != to[1] {
            return Kin::None;
        }
        // (Width, 64 bits, height, depth and levels, format: words 4 to 8.)
        if a[0] == b[0] && a[2..13] == b[2..13] {
            return Kin::Moved;
        }
        let (wide, high) = ([a[4], b[4]].map(u64::from), [a[6], b[6]].map(u64::from));
        return if from[0] == to[0] && a[0] == b[0] && wide[0] * high[1] == wide[1] * high[0] {
            Kin::Sized
        } else {
            Kin::Other
        };
    }
    let dimension = |view: &View| match view {
        View::Placed(false, resource, desc) if resource[0] != 1 => desc[1],
        View::Shader(r, Some(desc)) if crate::resource::buffer_bytes(*r as crate::object::This).is_none() => desc[1],
        _ => 0,
    };
    match (dimension(old), dimension(new)) {
        (0, _) | (_, 0) => Kin::None,
        (a, b) if a == b => Kin::Other,
        _ => Kin::None,
    }
}

/// Whether a slot that is written with a view of another texture goes on
/// showing the one it shows until the new one is made and its upload has run
/// (`VICTORY_KEEP_SHOWN`): 0: never, it reads as zeros, then as the neutral
/// texture; 1: one of the same kind in another place or size; 2: any of the
/// same dimension.
fn keep_shown() -> u8 {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_KEEP_SHOWN") {
        return if on { 2 } else { 0 };
    }
    static KEEP: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *KEEP.get_or_init(|| std::env::var("VICTORY_KEEP_SHOWN").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(1))
}

/// Slots that showed a texture and were written with a view of another,
/// since the start, by [`Kin`] (moved, sized, other, none), and those of
/// them that kept what they showed.
static REWRITTEN: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

fn rewritten(kin: Kin, kept: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    let class = match kin {
        Kin::Moved | Kin::Same => 0,
        Kin::Sized => 1,
        Kin::Other => 2,
        Kin::None => 3,
    };
    REWRITTEN[class].fetch_add(1, Relaxed);
    REWRITTEN[4].fetch_add(u64::from(kept), Relaxed);
    let n: u64 = REWRITTEN[..4].iter().map(|n| n.load(Relaxed)).sum();
    if n.is_multiple_of(5000) {
        tracing::info!(
            "slots that showed a texture and were written with another, since the start: {} of the same kind elsewhere, {} of another size, {} others, {} of another dimension; {} of them kept what they showed meanwhile (VICTORY_KEEP_SHOWN)",
            REWRITTEN[0].load(Relaxed),
            REWRITTEN[1].load(Relaxed),
            REWRITTEN[2].load(Relaxed),
            REWRITTEN[3].load(Relaxed),
            REWRITTEN[4].load(Relaxed)
        );
    }
}

/// Diagnostics: whether a texture that a slot showed until its new one came
/// holds the new one's bytes (the title moved it), by the bytes their host
/// copies were filled from: equal, different, not known.
#[cfg(feature = "diagnostics")]
fn same_bytes(old: u64, new: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    static SEEN: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];
    let view = |key: u64| view_at(key as u32, (key >> 32) as u32);
    let (Some(old), Some(new)) = (view(old), view(new)) else {
        return;
    };
    let held = |view: &View| {
        let View::Placed(false, resource, desc) = view else {
            return None;
        };
        let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
        crate::resource::texture_held(crate::resource::texture_at_made(address, resource)?)
    };
    let class = match kin(&old, &new) {
        Kin::Moved => 0,
        Kin::Same | Kin::None => return,
        _ => 3,
    };
    let which = match (held(&old), held(&new)) {
        (Some(a), Some(b)) if a == b => 0,
        (Some(a), Some(b)) if a.0 == b.0 => 1,
        _ => 2,
    };
    SEEN[class + which].fetch_add(1, Relaxed);
    let n: u64 = SEEN.iter().map(|n| n.load(Relaxed)).sum();
    if n.is_multiple_of(1000) {
        let seen: Vec<u64> = SEEN.iter().map(|n| n.load(Relaxed)).collect();
        tracing::info!(
            "slots that went on showing another texture until the new one came, since the start: of one of the same kind elsewhere {} had the new one's bytes, {} others, {} not known; of others {}, {}, {}",
            seen[0], seen[1], seen[2], seen[3], seen[4], seen[5]
        );
    }
}

/// For `crate::popwatch`, since the start: slots made null as they were
/// written with another texture, such that kept the old one, slots given the
/// neutral texture, slots made null as their memory was another's.
#[cfg(feature = "diagnostics")]
pub(crate) fn pop_causes() -> [u64; 4] {
    use std::sync::atomic::Ordering::Relaxed;
    let all: u64 = REWRITTEN[..4].iter().map(|n| n.load(Relaxed)).sum();
    let kept = REWRITTEN[4].load(Relaxed);
    [all.saturating_sub(kept), kept, LATE[3].load(Relaxed), STALE[0].load(Relaxed)]
}

/// Whether a slot already made from its record is told so without the lock
/// (`VICTORY_QUICK_MIRROR=0`: with it, as before).
fn quick_mirror() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_QUICK_MIRROR").is_none_or(|v| v != "0"))
}

/// The records last seen of a heap, held ([`HostHeap::copied_begin`]).
pub(crate) struct Copied<'a> {
    at: *mut u8,
    len: usize,
    step: usize,
    _held: std::sync::MutexGuard<'a, Vec<u8>>,
}

// SAFETY: the bytes are reached through `changes` alone, whose callers take
// parts apart from one another; the guard stays with the thread that has it.
unsafe impl Sync for Copied<'_> {}

impl Copied<'_> {
    /// The slots of `records` (as the title has them now, from slot `first`
    /// on) that are not the ones last seen, which they are from here on.
    /// (`records`: as the caller read them, through the kernel, which fails on
    /// a page the title unmapped meanwhile where a read in place would fault:
    /// the title unmaps heap pages too.)
    ///
    /// # Safety
    /// No other call beside this one has any of these slots.
    pub(crate) unsafe fn changes(&self, first: u32, records: &[u8], out: &mut Vec<u32>) {
        let at = first as usize * self.step;
        let end = (at + records.len()).min(self.len);
        if at >= end {
            return;
        }
        // SAFETY: within the records held; the caller's slots alone.
        let last = unsafe { std::slice::from_raw_parts_mut(self.at.add(at), end - at) };
        changed_records(last, &records[..end - at], self.step, first, out);
    }

    /// As [`Copied::changes`] for the `count` records of title memory at
    /// `title` (slot `first` on), 32 bytes each, read where they are, each
    /// once ([`crate::safecopy::changed`]); `bits` is room. False when a page
    /// of them could not be read: those before it are done, the others
    /// stay as last seen.
    ///
    /// # Safety
    /// As [`Copied::changes`]; `title` is title memory (read by a copy that
    /// fails where a page is not there).
    pub(crate) unsafe fn changes_in_place(&self, first: u32, title: *const u8, count: usize, bits: &mut Vec<u64>, out: &mut Vec<u32>) -> bool {
        let at = first as usize * self.step;
        let count = count.min(self.len.saturating_sub(at) / 32);
        if self.step != 32 || count == 0 {
            return true;
        }
        bits.clear();
        bits.resize(count.div_ceil(64), 0);
        // SAFETY: within the records held; the caller's slots alone.
        let whole = unsafe { crate::safecopy::changed(title, self.at.add(at), count, bits) };
        for (n, &word) in bits.iter().enumerate() {
            let mut word = word;
            while word != 0 {
                out.push(first + (n * 64) as u32 + word.trailing_zeros());
                word &= word - 1;
            }
        }
        whole
    }

    /// The `count` records last seen from slot `first` on (fewer where the
    /// heap ends).
    ///
    /// # Safety
    /// As [`Copied::changes`]: no other call beside this one has any of
    /// these slots.
    pub(crate) unsafe fn seen(&self, first: u32, count: usize) -> &[u8] {
        let at = (first as usize * self.step).min(self.len);
        let len = (count * self.step).min(self.len - at);
        // SAFETY: within the records held; the caller's slots alone.
        unsafe { std::slice::from_raw_parts(self.at.add(at), len) }
    }
}

/// The records (`step` bytes each, the first is slot `first`) that differ
/// between `last` and `records` (of one length) go into `out` and `last`.
/// Most of a changed page is as it was: it is looked at sixteen records at a
/// time.
fn changed_records(last: &mut [u8], records: &[u8], step: usize, first: u32, out: &mut Vec<u32>) {
    const AT_A_TIME: usize = 16;
    let block = AT_A_TIME * step;
    for (b, (old, new)) in last.chunks_mut(block).zip(records.chunks(block)).enumerate() {
        if old == new {
            continue;
        }
        for (n, (old, new)) in old.chunks_exact_mut(step).zip(new.chunks_exact(step)).enumerate() {
            if old != new {
                old.copy_from_slice(new);
                out.push(first + (b * AT_A_TIME + n) as u32);
            }
        }
    }
}

#[cfg(test)]
mod changed_records_tests {
    use super::changed_records;

    #[test]
    fn changed_records_are_those_that_differ_one_by_one() {
        for (step, count) in [(32usize, 128usize), (32, 2048), (16, 77), (32, 5), (32, 17)] {
            let mut seed = 7u64;
            let mut random = move || {
                seed = seed.wrapping_mul(0x5851_F42D_4C95_7F2D).wrapping_add(0x1405_7B7E_F767_814F);
                (seed >> 33) as usize
            };
            let mut last: Vec<u8> = (0..step * count).map(|_| random() as u8).collect();
            let mut now = last.clone();
            let mut differ = Vec::new();
            for slot in 0..count {
                if random() % 9 == 0 {
                    now[slot * step + random() % step] ^= 1 + (random() % 255) as u8;
                    differ.push(1000 + slot as u32);
                }
            }
            let mut out = Vec::new();
            changed_records(&mut last, &now, step, 1000, &mut out);
            assert_eq!(out, differ);
            assert_eq!(last, now);
            out.clear();
            changed_records(&mut last, &now, step, 1000, &mut out);
            assert!(out.is_empty());
        }
    }
}

/// Whether both are shader views of one texture the title placed in
/// memory: the same address and the same resource description, whatever
/// part of it each shows.
fn same_placed_texture(a: &View, b: &View) -> bool {
    match (a, b) {
        (View::Placed(false, resource, desc), View::Placed(false, other, other_desc)) => {
            // (Words 1 and 13 are the filler of the description's layout, not
            // part of what a texture is: `crate::resource::texture_at`.)
            let same = |from: usize, to: usize| resource[from..to] == other[from..to];
            resource[0] != 1 && resource[0] == other[0] && same(2, 13) && desc[10..12] == other_desc[10..12]
        }
        _ => false,
    }
}

/// The title republished only the mip window of a placed sampled view.
fn same_placed_srv_mip_window(a: &View, b: &View) -> bool {
    let (View::Placed(false, resource, old), View::Placed(false, _, new)) = (a, b) else { return false; };
    resource[12] & 7 == 0 && same_placed_texture(a, b) && old[..3] == new[..3]
        && match old[1] {
            4 => old[6] == new[6],
            5 => old[6..9] == new[6..9],
            _ => false,
        }
}

/// A shader view of a texture the title placed in memory itself, as a view
/// of the resource made for that memory (on first use).
pub(crate) fn placed_texture(view: &View) -> Option<View> {
    placed_texture_and_source(view).map(|(view, _)| view)
}

fn placed_texture_and_source(view: &View) -> Option<(View, Option<crate::object::This>)> {
    const BUFFER: u32 = 1;
    let View::Placed(unordered, resource, desc) = view else {
        return None;
    };
    if resource[0] == BUFFER {
        return None;
    }
    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let mut desc = *desc;
    if desc[0] == 0 {
        // The typed format field of the 56-byte guest resource description.
        desc[0] = resource[8];
    }
    if address == 0 {
        // An inline placed descriptor can encode a typed null with a shape
        // and no address. Preserve its dimension in the native descriptor.
        let words = Some(desc[..10].try_into().ok()?);
        let null = if *unordered { View::Unordered(0, 0, words) } else { View::Shader(0, words) };
        return explicit_null_texture(&null).then_some((null, None));
    }
    let texture = crate::resource::texture_at(address, resource)?;
    let source = crate::resource::alias_of(texture);
    let direct = source
        .filter(|&r| {
            !crate::resource::stands_in(r)
                && crate::resource::host_copy(r).is_some_and(|copy| {
                    let host = unsafe { copy.texture.GetDesc() };
                    let own = host.Format.0 as u32;
                    let view = crate::resource::pc_format(desc[0]);
                    let allowed = if *unordered {
                        host.Flags.contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS)
                    } else {
                        !host.Flags.contains(D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE)
                    };
                    allowed && (own == view || crate::resource::family(view) == own
                        || !*unordered && copy.packed_hdr_srv && view == DXGI_FORMAT_R9G9B9E5_SHAREDEXP.0 as u32)
                })
                && crate::resource::texture_memory(r).zip(crate::resource::texture_memory(texture))
                    .is_some_and(|(producer, placed)| crate::resource::placed_layout_matches(producer, placed, !*unordered))
        });
    let texture = direct.unwrap_or(texture);
    let source = if direct.is_some() { None } else { source };
    if *unordered {
        crate::resource::want_level(texture, 0);
        let copy = crate::resource::host_copy(texture)?;
        if copy.base != 0 { return None; }
        let host = &copy.texture;
        if crate::resource::placed_span(texture).is_some_and(|span| !crate::resource::gpu_target_overlaps(span).is_empty()) {
            // Separate native images cannot honor simultaneous stores/atomics
            // through incompatible views of the same guest bytes.
            return None;
        }
        // Only expose a UAV that the host allocation can actually support.
        if !unsafe { host.GetDesc() }
            .Flags
            .contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS)
        {
            return None;
        }
        return Some((View::Unordered(
            texture as usize,
            0,
            Some(desc[..10].try_into().ok()?),
        ), source));
    }
    crate::resource::want_level(texture, crate::resource::first_mip(&desc));
    Some((View::Shader(texture as usize, desc[..10].try_into().ok()), source))
}

/// A null view of the kind `view` is, for a slot whose view the host
/// cannot make: unordered access (to a buffer or a 2D texture) or a shader
/// view. Writes to it are dropped, reads give zeros.
fn null_for(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let is_buffer = |r: &usize| crate::resource::buffer_bytes(*r as crate::object::This).is_some();
    // Unordered views: whether a buffer, and the title's view dimension.
    let (buffer, dimension) = match view {
        View::Unordered(r, _, desc) => (
            is_buffer(r) || desc.is_some_and(|d| d[1] == 1),
            desc.map(|d| d[1]),
        ),
        View::Placed(true, resource, desc) => (resource[0] == 1, Some(desc[1])),
        // Shader views of buffers read nothing through a null buffer view:
        // a null 2D view where the shader expects a buffer is a mismatch
        // the hardware does not survive in general (GPU-based validation
        // reports it).
        View::Shader(r, desc) if is_buffer(r) || desc.is_some_and(|d| d[1] == 1) => {
            return null_buffer_view(at);
        }
        View::Placed(false, resource, _) if resource[0] == 1 => return null_buffer_view(at),
        // Textures: a null view of the title's dimension (1D, arrays, 3D,
        // cubes), not always 2D.
        View::Shader(_, Some(desc)) => return null_like(desc, at),
        View::Placed(false, _, desc) => {
            return null_like(desc[..10].try_into().unwrap_or(&[0; 10]), at);
        }
        View::Constant(..) => {
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC::default();
            // SAFETY: a slot of our heap; a zero location makes a null view.
            return unsafe { host.device.CreateConstantBufferView(Some(&desc), at) };
        }
        _ => return null_view(at),
    };
    let desc = if buffer {
        D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            ..Default::default()
        }
    } else {
        null_uav_desc(dimension)
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device.CreateUnorderedAccessView(
            None::<&ID3D12Resource>,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
}

/// A null texture UAV of the title's view dimension (1D, 1D or 2D arrays,
/// 3D), 2D if it names none: a 2D null view where the shader writes a 3D
/// texture is a mismatch GPU-based validation reports (the world's wind
/// field, CS_CopyWindFieldBeforeUpdateWithSmooth, 2026-11-24).
fn null_uav_desc(dimension: Option<u32>) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    match dimension.map(|d| D3D12_UAV_DIMENSION(d as i32)) {
        Some(D3D12_UAV_DIMENSION_TEXTURE1D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D;
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE1DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
            desc.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE2DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE3D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
            desc.Anonymous.Texture3D = D3D12_TEX3D_UAV {
                WSize: u32::MAX,
                ..Default::default()
            };
        }
        _ => {}
    }
    desc
}

/// A buffer view that reads zeros (raw, as translated shaders mostly read
/// buffers).
fn null_buffer_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                NumElements: 1,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

fn null_buffer_uav(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
        Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_UAV {
                NumElements: 1,
                Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap and a well-formed null raw UAV.
    unsafe { host.device.CreateUnorderedAccessView(None::<&ID3D12Resource>, None::<&ID3D12Resource>, Some(&desc), at) };
}

fn empty_buffer_view(unordered: bool, words: &[u32; 10], at: D3D12_CPU_DESCRIPTOR_HANDLE, twin: Option<D3D12_CPU_DESCRIPTOR_HANDLE>) {
    let Some(host) = host() else { return };
    if unordered {
        let mut desc = uav_desc(words);
        if desc.Format == DXGI_FORMAT_UNKNOWN || desc.Format == DXGI_FORMAT_R32_TYPELESS {
            null_buffer_uav(at);
        } else {
            desc.Anonymous.Buffer = D3D12_BUFFER_UAV { NumElements: 1, ..Default::default() };
            // SAFETY: a typed null resource in a view heap slot.
            unsafe { host.device.CreateUnorderedAccessView(None::<&ID3D12Resource>, None::<&ID3D12Resource>, Some(&desc), at) };
        }
        if let Some(twin) = twin { null_buffer_uav(twin); }
    } else {
        let mut desc = srv_desc(words);
        if desc.Format == DXGI_FORMAT_UNKNOWN || desc.Format == DXGI_FORMAT_R32_TYPELESS {
            null_buffer_view(at);
        } else {
            desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
            desc.Anonymous.Buffer = D3D12_BUFFER_SRV { NumElements: 1, ..Default::default() };
            // SAFETY: a typed null resource in a view heap slot.
            unsafe { host.device.CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at) };
        }
        if let Some(twin) = twin { null_buffer_view(twin); }
    }
}

/// A texture view that reads zeros with the dimension (and mip and array
/// counts) of the title's description `words`; 2D if it names none.
fn null_like(words: &[u32; 10], at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    const TEXTURES: std::ops::RangeInclusive<u32> = 2..=10;
    if !TEXTURES.contains(&words[1]) {
        return null_view(at);
    }
    let Some(host) = host() else { return };
    // One mip, one slice or cube, plane 0: fields a null view still checks.
    let mut w = [0u32; 10];
    w[1] = words[1];
    match words[1] {
        6 => {}
        7 => w[5] = 1,
        3 | 5 | 10 => (w[5], w[7]) = (1, 1),
        _ => w[5] = 1,
    }
    let mut desc = srv_desc(&w);
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A view that reads zeros, for slots whose view the host cannot make.
/// shortcut: slots with no view at all (kind unknown) get this null 2D
/// view; null buffer views there did not stop the world's low-address
/// page faults (2026-11-24, run w10), so they are not the cause.
fn null_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_publication_requires_disjoint_forms_and_accounts_for_every_change() {
        let slots = IndexedSlots { opaque: 8 | 2 | victory_shader::emit::BUFFER_WRITE_ACCESS,
            exact: [(17, 1)].into(), ..Default::default() };
        let pass = CandidatePass::new(&slots, Some(&[22].into())).unwrap();
        assert!(!pass.may_ignore_slot(17));
        assert!(!pass.may_ignore_slot(22));
        assert!(pass.may_ignore_slot(23));
        assert!(pass.disjoint_buffer_form(1));
        for form in [0, 2, 4, 8, 16, 32] { assert!(!pass.disjoint_buffer_form(form)); }
        assert!(CandidatePass::new(&IndexedSlots { opaque: 32, ..slots }, None).is_none());

        let index = CandidateIndex::new(1024);
        index.observed(23, 1, 1, 0, None);
        assert!(index.disjoint_buffer_at(23, &pass));
        index.pending(23, true);
        assert!(!index.disjoint_buffer_at(23, &pass));
        index.pending(23, false);
        index.observed(23, 2, 8, 4, None);
        assert!(!index.disjoint_buffer_at(23, &pass));
        pass.note(true);
        assert!(pass.accounts_for(1));
        assert!(!pass.accounts_for(2));
        pass.note(false);
        assert!(!pass.accounts_for(1));
    }

    #[test]
    fn indexed_sampled_mirrors_require_both_dimensions_or_a_complete_selection() {
        use MirrorReason::*;
        let slots = IndexedSlots { opaque: 8 | 2, texture_kinds: [1 << 2, 0],
            exact: [(17, 2)].into(), ..Default::default() };
        let dimensions = CandidatePass::new(&slots, None).unwrap();
        let one = 1 << 1;
        let two = 1 << 2;
        let cube = 1 << 4;
        assert_eq!(dimensions.sampled_reason(23, one, cube), SampledDimension);
        assert_eq!(dimensions.sampled_reason(23, two, cube), CompatibleOrUnknown);
        assert_eq!(dimensions.sampled_reason(23, cube, two), CompatibleOrUnknown);
        assert_eq!(dimensions.sampled_reason(23, 0, cube), CompatibleOrUnknown);
        assert_eq!(dimensions.sampled_reason(17, one, cube), Requested);
        let selected = CandidatePass::new(&slots, Some(&[22].into())).unwrap();
        assert_eq!(selected.sampled_reason(23, two, two), SampledSelection);
        assert_eq!(selected.sampled_reason(22, one, cube), Requested);
        let unknown = CandidatePass::new(&IndexedSlots { texture_kinds: [0, 0], ..slots }, None).unwrap();
        assert_eq!(unknown.sampled_reason(23, one, cube), CompatibleOrUnknown);

        let index = CandidateIndex::new(64);
        index.observed(23, 13, 8, cube, None);
        assert!(index.sampled_record_at(23, 13, cube));
        assert!(!index.sampled_record_at(23, 14, cube));
        assert!(!index.sampled_record_at(23, 13, two));
        index.pending(23, true);
        assert!(!index.sampled_record_at(23, 13, cube));
        index.pending(23, false);
        index.buffer(23, 2);
        assert!(!index.sampled_record_at(23, 13, cube));
    }

    #[test]
    fn indexed_sampled_mirror_journal_rechecks_latest_raw_and_native_record() {
        let pass = std::sync::Arc::new(CandidatePass::new(&IndexedSlots { opaque: 8, ..Default::default() },
            Some(&Default::default())).unwrap());
        let mut words = [VIEW_TAG, 19u32, 1, 0];
        let handle = words.as_mut_ptr() as u64;
        let key = (1u64 << 32) | 19;
        let mirror = |key, installed| MirrorNote { pass: pass.clone(), handle, key, installed,
            impact: MirrorImpact { slot: 23, old: 8, new: 8, reason: MirrorReason::SampledSelection } };
        drop(mirror(key - 1, true));
        drop(mirror(key, true));
        assert!(pass.accounts_for(2));
        let current = |slot| (slot == 23).then_some((handle, key));
        assert!(pass.sampled_mirrors_current(current));
        assert!(!pass.sampled_mirrors_current(|_| None));
        assert!(!pass.sampled_mirrors_current(|_| Some((handle, key - 1))));
        // Model a raw title copy, which does not advance the host publication counter.
        unsafe { super::super::record_bytes::write(words.as_mut_ptr().cast(), [VIEW_TAG, 20, 1]); }
        assert!(!pass.sampled_mirrors_current(current));
        drop(mirror(key + 1, false));
        assert!(!pass.accounts_for(3));
        for slot in 24..87 { assert!(pass.note_sampled_mirror(slot, handle, key)); }
        assert!(!pass.note_sampled_mirror(87, handle, key));
    }

    #[test]
    fn an_abandoned_indexed_publication_observation_releases_the_heap() {
        let Some(heap) = HostHeap::new(false, 64) else { return; };
        let slots = IndexedSlots { opaque: 8, ..Default::default() };
        let observing = &heap.candidate_observations.watching;
        use std::sync::atomic::Ordering::Acquire;
        let first = heap.begin_candidate_observation(&slots, None);
        assert!(observing.load(Acquire));
        drop(heap.begin_candidate_observation(&slots, None));
        assert!(observing.load(Acquire), "a non-owning observation must leave the first in place");
        drop(first);
        assert!(!observing.load(Acquire));
        let first = heap.begin_candidate_observation(&slots, None);
        {
            let _publication = heap.publish_views(PublicationCause::TextureRemake);
        }
        let before = first.before();
        assert_eq!(first.finish(None), (before + 1, false));
        assert!(!observing.load(Acquire));
    }

    #[test]
    fn indexed_candidates_share_unchanged_shards_and_preserve_old_membership() {
        let index = CandidateIndex::new(4096);
        let snapshot = |i: usize| index.shards[i].read().unwrap().snapshot();
        let other_kinds = |i: usize| index.shards[i].read().unwrap().without_sampled();
        index.observed(13, 7, 8, 4, None);
        assert!(other_kinds(0).is_empty());
        index.pending(13, true);
        assert_eq!(other_kinds(0)[0].slot, 13);
        index.buffer(500, 4);
        assert_eq!(other_kinds(1)[0].slot, 500);
        let before = snapshot(0);
        let other = snapshot(1);
        index.observed(13, 7, 8, 4, None);
        assert!(std::sync::Arc::ptr_eq(&before, &snapshot(0)));
        index.observed(13, 9, 0, 0, None);
        assert!(snapshot(0)[0].pending);
        index.pending(13, false);
        assert!(snapshot(0).is_empty());
        assert_eq!(before[0].texture, 8);
        assert_eq!(before[0].texture_kind, 4);
        assert!(std::sync::Arc::ptr_eq(&other, &snapshot(1)));
    }

    #[test]
    fn material_deferral_checks_all_physical_aliases_and_exclusive_ends() {
        let material = PlainMaterial { record: 7, topology: 1, ranges: [(100, 120), (300, 320)].into(), binding: Default::default() };
        assert!(material.after_writes(&Some(vec![(0, 100), (120, 300), (320, 330)])));
        assert!(!material.after_writes(&Some(vec![(319, 320)])));
        assert!(!material.after_writes(&Some(vec![(99, 101)])));
        assert!(!material.after_writes(&None));
    }

    /// Every host sampler heap backs all 2,048 hardware descriptors and
    /// reserves slot 2,047 as a null sentinel so `umin(index, 2047)` in
    /// `victory-shader` never reads past smaller title sampler heaps.
    #[test]
    fn sampler_heaps_back_2048_slots_and_reserve_null_sentinel() {
        let Some(small) = HostHeap::new(true, 16) else { return };
        // SAFETY: live heap query.
        assert_eq!(unsafe { small.heap.GetDesc().NumDescriptors }, 2048);
        assert_eq!(small.count, 16);
        let Some(full) = HostHeap::new(true, 2048) else { return };
        // SAFETY: live heap query.
        assert_eq!(unsafe { full.heap.GetDesc().NumDescriptors }, 2048);
        assert_eq!(full.count, 2047);
    }

    /// A view of a host copy that begins at a later level shows the same
    /// levels of it.
    #[test]
    fn views_count_from_the_first_level_of_the_host_copy() {
        let view = |first, levels, clamp| {
            let mut desc = srv_desc(&[98, 4, 5768, 0, first, levels, 0, 0, 0, 0]);
            desc.Anonymous.Texture2D.ResourceMinLODClamp = clamp;
            from_base(&mut desc, 3, 10);
            // SAFETY: the member its dimension says.
            let t = unsafe { desc.Anonymous.Texture2D };
            (t.MostDetailedMip, t.MipLevels, t.ResourceMinLODClamp)
        };
        assert_eq!(view(3, 4, 0.0), (0, 4, 0.0));
        assert_eq!(view(5, u32::MAX, 5.5), (2, u32::MAX, 2.5));
        // (Two levels the copy has not.)
        assert_eq!(view(1, 4, 0.0), (0, 2, 0.0));
    }

    /// 2D textures and arrays have a neutral stand-in, others a null view.
    #[test]
    fn a_texture_not_there_yet_has_a_neutral_stand_in() {
        let Some(host) = host() else { return };
        // SAFETY: plain heap creation and queries.
        unsafe {
            let heap: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 1,
                    ..Default::default()
                })
                .unwrap();
            let at = heap.GetCPUDescriptorHandleForHeapStart();
            let view = |dimension| View::Shader(0, Some([98, dimension, 5768, 0, 0, 1, 0, 1, 0, 0]));
            assert!(neutral_for(&view(4), at).is_some());
            assert!(neutral_for(&view(5), at).is_some());
            assert!(neutral_for(&view(9), at).is_none());
            assert!(neutral_for(&view(8), at).is_none());
            assert!(host.device.GetDeviceRemovedReason().is_ok());
        }
    }

    /// Memory past the levels a slot's view shows is not its texture's.
    #[test]
    fn a_view_of_smaller_levels_is_not_told_of_memory_past_them() {
        let mut spans = Spans::default();
        spans.note(1, 0x10000, 0x50000, true);
        spans.shows(1, 3, 0x12000, 1);
        spans.note(2, 0x10000, 0x50000, true);
        spans.shows(2, 0, 0x50000, 2);
        assert_eq!(spans.slots_holding(0x11000, 0x11800), vec![1, 2]);
        assert_eq!(spans.slots_holding(0x20000, 0x21000), vec![2]);
        assert_eq!(spans.slots_in(0x20000, 0x21000), vec![2]);
        spans.forget(2);
        assert!(spans.slots_in(0x20000, 0x21000).is_empty());
        assert_eq!(spans.slots_in(0x0, 0x10001), vec![1]);
    }

    /// A texture filled with less than was mapped is looked at again, but
    /// not for ever: memory that is shown and cannot be read ends it, and a
    /// filling that got everything starts the count anew.
    #[test]
    fn a_short_filling_is_looked_at_again_a_few_times() {
        // An address no other test names.
        let start = 0x7fee_1234_0000u64;
        let pending = || super::SHORT.lock().unwrap().0.remove(&start);
        for _ in 0..super::SHORT_IN_A_ROW {
            super::read_short(start);
            assert!(pending());
        }
        super::read_short(start);
        assert!(!pending());
        super::read_all(start);
        super::read_short(start);
        assert!(pending());
        super::read_all(start);
    }

    /// A null texture UAV keeps the title's dimension (3D for the world's
    /// wind field), and is 2D when the title names none.
    #[test]
    fn null_uavs_keep_the_titles_dimension() {
        let d = null_uav_desc(Some(8));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE3D);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture3D.WSize }, u32::MAX);
        let d = null_uav_desc(Some(5));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture2DArray.ArraySize }, 1);
        let dimension = |d| null_uav_desc(d).ViewDimension;
        assert_eq!(dimension(Some(2)), D3D12_UAV_DIMENSION_TEXTURE1D);
        assert_eq!(dimension(Some(4)), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(None), D3D12_UAV_DIMENSION_TEXTURE2D);
        // Multisampled UAVs need a cap; they stay 2D.
        assert_eq!(dimension(Some(6)), D3D12_UAV_DIMENSION_TEXTURE2D);
    }

    /// What the texture code asks of the spans (levels on demand): where
    /// the next texture starts inside a span, the lowest level a view of a
    /// texture shows, and a texture's slots.
    #[test]
    fn spans_say_the_next_start_and_the_lowest_level_viewed() {
        let mut spans = Spans::default();
        // A 512x512 texture of 0x56000 bytes viewed from levels 3 and 1,
        // and the next one 0x6000 after it (run 188).
        spans.note(1, 0x1_0000, 0x1_0000 + 0x5_6000, true);
        spans.shows(1, 3, u64::MAX, 1);
        spans.note(2, 0x1_0000, 0x1_0000 + 0x5_6000, true);
        spans.shows(2, 1, u64::MAX, 2);
        spans.note(3, 0x1_6000, 0x1_6000 + 0x5_6000, true);
        assert_eq!(spans.next_start(0x1_0000, 0x1_0000 + 0x5_6000), Some(0x1_6000));
        assert_eq!(spans.next_start(0x1_6000, 0x1_6000 + 0x5_6000), None);
        assert_eq!(spans.first_viewed(0x1_0000), Some(1));
        assert_eq!(spans.first_viewed(0x1_6000), None);
        assert_eq!(spans.slots_at(0x1_0000), vec![1, 2]);
        // The view from level 1 goes: the texture is viewed from level 3.
        spans.forget(2);
        assert_eq!(spans.first_viewed(0x1_0000), Some(3));
    }

    /// Views of one placed texture are told from views of others by the
    /// address and the resource description, not by what they show.
    #[test]
    fn views_of_one_placed_texture_are_told_by_address_and_description() {
        let texture = [3, 0, 0, 0, 512, 0, 512, 0x8_0001, 98, 1, 0, 0x105, 0, 0];
        let view = |resource: [u32; 14], first_mip: u32, address: u32| {
            let mut desc = [0u32; 16];
            (desc[0], desc[1], desc[4], desc[5], desc[10], desc[11]) = (98, 4, first_mip, 8 - first_mip, address, 0xb);
            View::Placed(false, resource, desc)
        };
        let low = view(texture, 3, 0x1000_0000);
        // more mips of the same texture
        assert!(same_placed_texture(&low, &view(texture, 0, 0x1000_0000)));
        // another address, another description, a buffer, an unordered view
        assert!(!same_placed_texture(&low, &view(texture, 3, 0x2000_0000)));
        let mut wider = texture;
        wider[4] = 1024;
        assert!(!same_placed_texture(&low, &view(wider, 3, 0x1000_0000)));
        let mut buffer = texture;
        buffer[0] = 1;
        assert!(!same_placed_texture(&view(buffer, 3, 0x1000_0000), &view(buffer, 3, 0x1000_0000)));
        let View::Placed(_, resource, desc) = low.clone() else { unreachable!() };
        assert!(!same_placed_texture(&low, &View::Placed(true, resource, desc)));
        assert!(!same_placed_texture(&low, &View::Shader(0, None)));
    }

    #[test]
    fn placed_srv_mip_window_preserves_the_sampled_type_and_layers() {
        let texture = [3, 0, 0, 0, 256, 0, 256, 0x7_0002, 28, 1, 0, 266, 0x300000, 0];
        let old = [28, 5, 5768, 0, 3, 4, 0, 2, 0, 0, 0x0b80_0000, 11, 0, 0, 0, 3];
        let mut new = old;
        new[4] = 2;
        new[5] = 5;
        let a = View::Placed(false, texture, old);
        assert!(same_placed_srv_mip_window(&a, &View::Placed(false, texture, new)));
        let mut authored = texture;
        authored[12] |= 4;
        assert!(!same_placed_srv_mip_window(&View::Placed(false, authored, old), &View::Placed(false, authored, new)));
        new[6] = 1;
        assert!(!same_placed_srv_mip_window(&a, &View::Placed(false, texture, new)));
        new[6] = 0;
        new[8] = 1;
        assert!(!same_placed_srv_mip_window(&a, &View::Placed(false, texture, new)));
    }

    #[test]
    fn ranges_that_touch_are_one() {
        let mut ranges = vec![(40, 50), (30, 40), (35, 40), (10, 20), (45, 60), (60, 61), (1, 1)];
        join(&mut ranges);
        assert_eq!(ranges, [(1, 1), (10, 20), (30, 61)]);
        join(&mut ranges);
        assert_eq!(ranges.len(), 3);
    }

    /// A view's title address goes after the strides, at two dwords per
    /// slot; a view that is no buffer's, and none, enter 0.
    #[test]
    fn view_addresses_follow_the_strides() {
        use victory_shader::emit::{SLOT_STRIDES_BYTES, SLOT_STRIDES_COUNT, VIEW_ADDRESSES};
        let mut table = vec![0xdead_beefu32; SLOT_STRIDES_BYTES as usize / 4];
        let data = table.as_mut_ptr() as usize;
        // A ray-tracing structure's view: its address in words 4 and 5.
        const STRUCTURE: u32 = D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE.0 as u32;
        let mut desc = [0u32; 10];
        (desc[1], desc[4], desc[5]) = (STRUCTURE, 0x1234_5600, 0x42);
        let view = View::Shader(0, Some(desc));
        assert_eq!(view_address(&view), Some(0x42_1234_5600));
        write_view_address(data, 7, Some(&view));
        write_view_address(data, 8, None);
        write_view_address(data, SLOT_STRIDES_COUNT - 1, Some(&view));
        // Past the table: nothing written.
        write_view_address(data, SLOT_STRIDES_COUNT, Some(&view));
        let at = VIEW_ADDRESSES as usize / 4;
        assert_eq!(table[at + 14..at + 18], [0x1234_5600, 0x42, 0, 0]);
        assert_eq!(table[at + 12..at + 14], [0xdead_beef; 2], "slot 6 untouched");
        let last = at + 2 * (SLOT_STRIDES_COUNT as usize - 1);
        assert_eq!(table[last..], [0x1234_5600, 0x42]);
        assert_eq!(table[..at].iter().filter(|&&w| w != 0xdead_beef).count(), 0, "the strides untouched");
    }

    /// Typed buffer views of integer formats are marked in the table of
    /// strides; float, normalized, raw and structured ones are not.
    #[test]
    fn integer_typed_buffer_views_are_marked_in_the_table_of_strides() {
        use victory_shader::emit::{VIEW_INTEGER, VIEW_SIGNED};
        // Format, dimension (buffer), first element, count, stride, counter, flags.
        let uav = |format, stride, flags| View::Unordered(0, 0, Some([format, 1, 0, 0, 64, stride, 0, 0, flags, 0]));
        assert_eq!(view_kind(&uav(12, 0, 0)), VIEW_INTEGER, "R16G16B16A16_UINT");
        assert_eq!(view_kind(&uav(59, 0, 0)), VIEW_INTEGER | VIEW_SIGNED, "R16_SINT");
        assert_eq!(view_kind(&uav(10, 0, 0)), 0, "R16G16B16A16_FLOAT");
        assert_eq!(view_kind(&uav(58, 0, 0)), 0, "R16_SNORM");
        assert_eq!(view_kind(&uav(39, 0, 1)), 0, "raw");
        assert_eq!(view_kind(&uav(0, 16, 0)), 0, "structured");
        assert_eq!(view_stride(&uav(12, 0, 0)), Some(8));
    }

    /// What may stand for a texture that is not there yet: one of its kind in
    /// another place or size; another one only if asked; nothing a shader
    /// cannot read in its place.
    #[test]
    fn a_slot_shows_on_what_can_stand_for_its_new_texture() {
        let texture = [3, 0, 0, 0, 512, 0, 256, 0x8_0001, 98, 1, 0, 0x105, 0, 0];
        let view = |resource: [u32; 14], format: u32, dimension: u32, address: u32| {
            let mut desc = [0u32; 16];
            (desc[0], desc[1], desc[10], desc[11]) = (format, dimension, address, 0xb);
            View::Placed(false, resource, desc)
        };
        let old = view(texture, 98, 4, 0x1000_0000);
        let mut twice = texture;
        (twice[4], twice[6], twice[7]) = (1024, 512, 0x9_0001);
        let mut square = texture;
        square[6] = 512;
        let mut buffer = texture;
        buffer[0] = 1;
        assert_eq!(kin(&old, &view(texture, 98, 4, 0x1000_0000)), Kin::Same);
        assert_eq!(kin(&old, &view(texture, 98, 4, 0x2000_0000)), Kin::Moved);
        assert_eq!(kin(&old, &view(twice, 98, 4, 0x2000_0000)), Kin::Sized);
        assert_eq!(kin(&old, &view(square, 98, 4, 0x2000_0000)), Kin::Other);
        assert_eq!(kin(&old, &view(twice, 83, 4, 0x2000_0000)), Kin::Other);
        assert_eq!(kin(&old, &view(texture, 98, 9, 0x2000_0000)), Kin::None);
        assert_eq!(kin(&old, &view(buffer, 98, 1, 0x2000_0000)), Kin::None);
        for (kin, kept) in [(Kin::Same, [true; 3]), (Kin::Moved, [false, true, true]), (Kin::Sized, [false, true, true]), (Kin::Other, [false, false, true]), (Kin::None, [false; 3])] {
            assert_eq!([0, 1, 2].map(|keep| kin.kept(keep)), kept, "{kin:?}");
        }
    }

    /// A constant buffer view larger than the host takes is cut to what it
    /// takes: the device, which such a view removes, stays.
    #[test]
    fn an_oversized_constant_buffer_view_leaves_the_device_alive() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        // SAFETY: a plain device query.
        let alive = || unsafe { host.device.GetDeviceRemovedReason() }.is_ok();
        // Made first: an allocation that fails on a removed device must not
        // end the test early.
        let (buffer, _) = host.upload_buffer(1 << 20).expect("an upload buffer");
        let memory = vec![0u8; 2 << 20];
        let at = (memory.as_ptr() as u64).next_multiple_of(64 << 10);
        for size in [969_472u32, 896_256] {
            assert!(make(&View::Constant(at, size), heap.cpu(0)).is_some());
            assert!(alive(), "a view of {size} bytes");
        }
        // SAFETY: a live upload buffer.
        heap.constant_at(1, &buffer, unsafe { buffer.GetGPUVirtualAddress() }, 969_472);
        assert!(alive(), "a view of the list's own memory");
        assert_eq!(constant_bytes(480), 512);
        assert_eq!(constant_bytes(64 << 10), 64 << 10);
        assert_eq!(constant_bytes(u32::MAX), 64 << 10);
    }

    /// A null 3D UAV made on the real device leaves it alive.
    #[test]
    fn null_3d_uav_is_accepted_by_the_device() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let view = View::Unordered(0, 0, Some([28, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
        null_for(&view, heap.cpu(0));
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Dynamic slots stay taken until given back, and runs do not wrap.
    #[test]
    fn a_structured_view_made_raw_passes_the_host_check() {
        // 100 elements of 20 bytes from element 8, in a copy that starts 32
        // bytes before the view's memory: bytes 192..2192.
        let (first, count) = raw_window(32, 8, 100, 20).unwrap();
        assert_eq!((first, count), (48, 500));
        assert!(buffer_view_ok(DXGI_FORMAT_R32_TYPELESS, first, count, 0, true, 4096));
        // the format a raw view must name; the structured one's is refused
        assert!(!buffer_view_ok(DXGI_FORMAT_UNKNOWN, first, count, 0, true, 4096));
        // past the buffer, not on 16 bytes, no elements
        assert!(!buffer_view_ok(DXGI_FORMAT_R32_TYPELESS, first, count, 0, true, 2000));
        assert!(raw_window(4, 0, 100, 20).is_none());
        assert!(!buffer_view_ok(DXGI_FORMAT_R32_TYPELESS, 0, 0, 0, true, 4096));
        // a structured view the host takes as it is
        assert!(buffer_view_ok(DXGI_FORMAT_UNKNOWN, 1, 10, 12, false, 4096));
    }

    /// The raw twin of a view shows the same bytes: typed and structured ones
    /// that begin on 16 bytes, whole dwords, none past the buffer.
    #[test]
    fn raw_twins_show_the_views_bytes() {
        let (u16s, u32s, none) = (DXGI_FORMAT(57), DXGI_FORMAT(42), DXGI_FORMAT_UNKNOWN);
        assert_eq!(twin_window(u32s, 4, 10, 0, false, 4096), Some((4, 10, 0)));
        assert_eq!(twin_window(u16s, 8, 7, 0, false, 4096), Some((4, 4, 0)));
        // the last dword would reach past the buffer
        assert_eq!(twin_window(u16s, 8, 7, 0, false, 30), Some((4, 3, 0)));
        assert_eq!(twin_window(none, 2, 3, 24, false, 4096), Some((12, 18, 0)));
        assert_eq!(twin_window(DXGI_FORMAT_R32_TYPELESS, 8, 5, 0, true, 4096), Some((8, 5, 0)));
        // where no raw view begins: from the 16 bytes below
        assert_eq!(twin_window(none, 1, 3, 24, false, 4096), Some((4, 20, 8)));
        assert_eq!(twin_window(u32s, 1, 10, 0, false, 4096), Some((0, 11, 4)));
        assert_eq!(twin_window(u32s, 1, 10, 0, false, 4), None);
    }

    /// What a slot is made as is made beside the heap and copied in whole: the
    /// slot's descriptor is the one the view has, and each thread makes its
    /// own.
    #[test]
    fn views_are_made_beside_the_heap_and_copied_in() {
        let Some(host) = host() else {
            return; // no host GPU here
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let (Some(a), Some(b)) = (staged(false, 0), staged(false, 1)) else {
            return;
        };
        assert_ne!(a.ptr, b.ptr);
        assert_ne!(Some(a.ptr), std::thread::spawn(|| staged(false, 0).map(|h| h.ptr)).join().unwrap());
        let (buffer, _) = host.shared_buffer(1 << 16).unwrap();
        let index = heap.dynamic(1).unwrap();
        let bytes = |at: D3D12_CPU_DESCRIPTOR_HANDLE| {
            let mut bytes = [0u8; 32];
            crate::host::read_into(at.ptr as *const u8, &mut bytes).then_some(bytes)
        };
        // Descriptors are the driver's own: a copy holds the bytes made
        // beside the heap (WARP's point to an object of their own, made anew
        // each time), or those of the same view made in place (AMD's name
        // the slot they are in, and change as they are copied in).
        let copied_as_made = |made_in_place: &dyn Fn()| {
            let (slot, beside) = (bytes(heap.cpu(index)), bytes(staged(false, 0).unwrap()));
            made_in_place();
            slot == beside || slot == bytes(heap.cpu(index))
        };
        heap.raw_at(index, &buffer, 64, 1024);
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: 64 / 4,
                    NumElements: 1024 / 4,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of our heap and a live buffer covering the range.
        let raw = || unsafe { host.device.CreateShaderResourceView(&buffer, Some(&desc), heap.cpu(index)) };
        assert!(copied_as_made(&raw), "copied as it was made");
        heap.null_raw_at(index);
        assert!(copied_as_made(&|| null_buffer_view(heap.cpu(index))), "another view since, copied as it was made");
        // SAFETY: a plain query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// A raw view moved on by some dwords says so in its descriptor, on a
    /// card whose descriptors are what we take them for; on another nothing
    /// is changed.
    #[test]
    fn a_raw_view_is_moved_only_where_its_descriptor_says_where_it_is() {
        let (Some(host), Some(at)) = (host(), staged(false, 1)) else {
            return; // no host GPU here
        };
        let (buffer, _) = host.shared_buffer(1 << 16).unwrap();
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: 8,
                    NumElements: 100,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        // SAFETY: this thread's slot, a live buffer.
        let start = unsafe {
            host.device.CreateShaderResourceView(&buffer, Some(&desc), at);
            buffer.GetGPUVirtualAddress() + 32
        };
        let words = || {
            let mut bytes = [0u8; 32];
            assert!(crate::host::read_into(at.ptr as *const u8, &mut bytes));
            bytes
        };
        let before = words();
        assert!(!move_raw(at, start + 16, 400, 8), "another address");
        assert!(!move_raw(at, start, 404, 8), "another size");
        assert!(!move_raw(at, start, 400, 400), "nothing left");
        assert_eq!(words(), before);
        if move_raw(at, start, 400, 8) {
            let moved = words();
            assert_eq!(moved[..8], (start + 8).to_le_bytes());
            assert_eq!(moved[8..12], 392u32.to_le_bytes());
            assert_eq!(moved[12..], before[12..]);
        } else {
            assert_eq!(words(), before);
        }
    }

    #[test]
    fn a_read_only_uav_does_not_conflict_with_a_copied_srv() {
        let mut op = BufferAliases::default();
        op.slots.opaque = 2 | 4;
        assert!(!op.needs_check());
        op.slots.exact.insert(7, 4 | victory_shader::emit::BUFFER_WRITE_ACCESS);
        assert!(op.needs_check());
        op.slots.exact.clear();
        op.root_writes.push((0x100, 0x200));
        assert!(op.needs_check());
    }

    /// Views that begin where no host view can: raw ones off 16 bytes,
    /// structured ones off their elements (and off 16 bytes, where they are
    /// made raw), typed ones off theirs.
    #[test]
    fn views_off_their_elements_are_told() {
        // the colour table bakes' parameter block: raw, 8 bytes into its window
        assert!(off_elements(8, 0, true, 4, true));
        assert!(!off_elements(0, 0, true, 4, true));
        assert!(!off_elements(48, 0, true, 4, true));
        // 456-byte elements: at one of them, on 16 bytes, at neither
        assert!(!off_elements(456, 456, false, 0, true));
        assert!(!off_elements(32, 456, false, 0, true));
        assert!(off_elements(100, 456, false, 0, true));
        // A typed view of 12-byte elements on 16 bytes and not on an element.
        assert!(off_elements(23_702_896, 0, false, 12, true));
        assert!(!off_elements(48, 0, false, 12, true));
        assert!(!off_elements(12, 0, false, 4, true));
        assert!(off_elements(12, 0, false, 8, true));
        // With no raw twin, a structured view is read as raw at its own slot.
        assert!(off_elements(56, 56, false, 0, false));
        assert!(!off_elements(56, 56, false, 0, true));
        assert!(!off_elements(112, 56, false, 0, false));
    }

    #[test]
    fn rgb32_window_matches_the_entire_guest_view_inside_the_writer() {
        let kind = (0, false, DXGI_FORMAT_R32G32B32_FLOAT.0 as u32);
        let offset = 0x2dd5_bf30 - 0x2dc3_0000;
        let bytes = 32 * 12;
        let (first, words) = rgb32_window(offset, bytes, kind).unwrap();
        assert_eq!((first * 4, u64::from(words) * 4), (offset, bytes));
        assert!(buffer_view_ok(DXGI_FORMAT_R32_TYPELESS, first, words, 0, true, 0x0ae0_0000));
        assert!(rgb32_window(48, bytes, kind).is_none()); // Native typed address.
        assert!(rgb32_window(offset + 4, bytes, kind).is_none()); // No exact raw start.
        assert!(rgb32_window(offset, bytes - 4, kind).is_none());
        assert!(rgb32_window(offset, bytes, (0, false, DXGI_FORMAT_R32_FLOAT.0 as u32)).is_none());
    }

    /// The descriptor and refresh agree on the guest bytes, including the
    /// dword prefix when mapped raw views share their writer's allocation.
    #[test]
    fn views_off_their_elements_refresh_the_same_bytes_the_shader_reads() {
        let Some(host) = host() else {
            return; // no host GPU here
        };
        let memory = vec![0u64; 1 << 13];
        let start = (memory.as_ptr() as u64).next_multiple_of(4096);
        // SAFETY: a plain CPU-side heap of one view.
        let cpu: ID3D12DescriptorHeap = unsafe {
            host.device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                ..Default::default()
            })
        }
        .unwrap();
        // SAFETY: a live heap.
        let at = unsafe { cpu.GetCPUDescriptorHandleForHeapStart() };
        // Memory placed at `address`, seen as the title passes it: raw, 32
        // dwords, or four 456-byte elements.
        let view = |address: u64, raw: bool| {
            let resource: [u32; 14] = [1, 0, 0, 0, 4 * 456, 0, 1, 1 | 1 << 16, 0, 1, 0, 1, 0, 0];
            let mut desc = [0u32; 16];
            let words = if raw {
                [39, 1, 5768, 0, 0, 0, 32, 0, 1, 0]
            } else {
                [0, 1, 5768, 0, 0, 0, 4, 456, 0, 0]
            };
            desc[..10].copy_from_slice(&words);
            (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
            View::Placed(false, resource, desc)
        };
        let width = |v: &View| {
            let made = make(v, at).expect("a view").expect("a buffer");
            // SAFETY: a live buffer.
            unsafe { made.GetDesc() }.Width
        };
        for (off, raw, len) in [(8, true, 128u64), (8, false, 4 * 456)] {
            let v = view(start + off, raw);
            let made = make(&v, at).expect("a view").expect("a buffer");
            let (copy, address, bytes) = buffer_copy(&v).unwrap();
            assert_eq!(made, copy.buffer);
            assert_eq!((address, bytes), (start + off, len));
            if victory_shader::emit::mapped_views() && victory_shader::emit::raw_views() == 0 {
                let offset = copy.offset(address);
                let (first, words, prefix) = canonical_raw(offset, len, view_elements(&v).unwrap()).unwrap();
                assert_eq!(raw_prefix(&v, &mut ViewBacking::default()), Some(8));
                assert_eq!(first * 4 + u64::from(prefix), offset);
                assert_eq!(u64::from(words) * 4 - u64::from(prefix), bytes);
            } else {
                assert_eq!(copy.span(), (address, len));
            }
        }
        // Register the backing range so the host copy starts at `start`,
        // independent of the loose memory window that contains the allocation.
        let mut v = view(start, false);
        let View::Placed(_, _, desc) = &mut v else { unreachable!() };
        desc[4] = 1; // FirstElement, at byte offset 456 in the copy
        desc[6] = 3; // NumElements
        let key = start as usize;
        crate::shadow::register(key, start, 4 * 456);
        let made = make(&v, at);
        let refreshed = buffer_copy(&v);
        crate::shadow::unregister(key, start);
        let made = made.expect("a view").expect("a buffer");
        let (copy, address, bytes) = refreshed.unwrap();
        assert_eq!((address, bytes), (start + 456, 3 * 456));
        assert_eq!(made, copy.buffer, "the descriptor and refresh share a copy");
        if victory_shader::emit::raw_views() == 0 && !victory_shader::emit::mapped_views() {
            assert_eq!(copy.span(), (address, bytes));
        } else {
            assert_eq!(copy.span(), (start, 4 * 456));
        }
        for (off, raw) in [(16, true), (32, false)] {
            let v = view(start + off, raw);
            let (copy, ..) = buffer_copy(&v).unwrap();
            assert_ne!(copy.span().0, start + off, "its buffer's copy");
            // SAFETY: a live buffer.
            assert_eq!(width(&v), unsafe { copy.buffer.GetDesc() }.Width);
        }
    }

    /// The queue of textures picked by index serves the slots looked at
    /// before first, then the newest write and the oldest in turns; a
    /// slot whose memory is not there is looked at again after
    /// 1, 2, 4 frames up to the limit; and what the pass reports about a
    /// slot written again meanwhile is dropped.
    #[test]
    fn the_texture_queue_serves_the_newest_writes_first() {
        let mut q = TextureQueue::default();
        // Slots 10..18 written in frames 1..9, slot 5 last.
        for i in 0..8u32 {
            q.written(10 + i, u64::from(i) + 1, 0, false);
        }
        q.written(5, 9, 0, false);
        let order: Vec<u32> = q.due(9, 100).into_iter().map(|d| d.0).collect();
        // newest, oldest, newest, oldest, and so on
        assert_eq!(order, [5, 10, 17, 11, 16, 12, 15, 13, 14]);
        assert_eq!(q.due(9, 2).len(), 2);
        assert_eq!(q.census(9), [9, 9, 0, 0, 8]);
        // Slot 17's memory is not mapped: again next frame, then in 2, 4, 4.
        let write = |q: &TextureQueue, index: u32| q.due(u64::MAX, 100).iter().find(|d| d.0 == index).unwrap().1;
        let w17 = write(&q, 17);
        let mut frame = 9;
        for wait in [1, 2, 4, 4] {
            q.later(17, w17, frame, 4);
            assert!(!q.due(frame + wait - 1, 100).iter().any(|d| d.0 == 17));
            frame += wait;
            // Due again, and ahead of every slot never looked at.
            assert_eq!(q.due(frame, 100)[0].0, 17);
        }
        assert_eq!(q.census(9)[2], 1);
        q.done(17, w17);
        assert!(!q.due(u64::MAX, 100).iter().any(|d| d.0 == 17));
        // Uploaded with part of its memory there: kept, with what was there.
        let w16 = write(&q, 16);
        q.partly(16, w16, 12, 4096);
        assert_eq!(q.mapped(16), 4096);
        assert_eq!(q.due(12, 100)[0].0, 16);
        // The title writes slot 16 again while the pass works on it: what
        // the pass then says about the old write changes nothing.
        q.written(16, 13, 0, false);
        q.done(16, w16);
        q.later(16, w16, 13, 4);
        assert_eq!(q.mapped(16), 0);
        assert_eq!(q.due(13, 100)[0].0, 16);
        // A draw that makes the host slot (`seen`) keeps what the queue knows,
        let w15 = write(&q, 15);
        q.partly(15, w15, 20, 8192);
        q.written(15, 14, 0, true);
        assert_eq!(q.mapped(15), 8192);
        // and queues a slot the queue did not hold.
        q.written(99, 14, 0, true);
        assert!(q.due(14, 100).iter().any(|d| d.0 == 99));
        // A write due later (a table's slot) is not handed out before.
        q.written(98, 14, 16, false);
        assert!(!q.due(15, 100).iter().any(|d| d.0 == 98));
        assert!(q.due(16, 100).iter().any(|d| d.0 == 98));
    }

    /// Slots held back for lack of room on the card are not handed out
    /// until their frame, keep their place, are not counted as waiting
    /// for their memory, and all come back at once when the card has room;
    /// a slot written again meanwhile is the title's new write.
    #[test]
    fn slots_held_for_lack_of_room_come_back_when_there_is_room() {
        let mut q = TextureQueue::default();
        for i in 0..4u32 {
            q.written(20 + i, 1, 0, false);
        }
        let due = q.due(1, 100);
        assert_eq!(due.len(), 4);
        // The card is full: slots 20 and 21 are held back until frame 101.
        for &(index, write) in due.iter().filter(|d| d.0 < 22) {
            q.hold(index, write, 101);
        }
        let now: Vec<u32> = q.due(2, 100).into_iter().map(|d| d.0).collect();
        assert!(!now.contains(&20) && !now.contains(&21));
        assert!(now.contains(&22) && now.contains(&23));
        assert_eq!(q.census(2)[2], 0, "held slots do not wait for their memory");
        // Due again at their frame, in the order they had.
        assert_eq!(q.due(101, 100), due);
        // Slot 21 is written again while held: due as written.
        q.written(21, 3, 0, false);
        assert!(q.due(3, 100).iter().any(|d| d.0 == 21));
        // Room again at frame 5: slot 20 is due at once, and only it was
        // still held as it was.
        assert_eq!(q.room(5), 1);
        assert!(q.due(5, 100).iter().any(|d| d.0 == 20));
        assert_eq!(q.room(6), 0);
    }

    /// Textures of which only the smallest levels are held, packed into
    /// one another's address range (the three of run 188: 0x2c000 bytes
    /// each at full size, their starts 0x2000 and 0x4000 apart): writing
    /// any of them, in any order and again, leaves the others.
    #[test]
    fn packed_neighbours_stay() {
        let mut spans = Spans::default();
        assert_eq!(spans.rule, Rule::Start);
        let at = 0xb_86bf_8000u64;
        let (a, b, c) = (at, at + 0x2000, at + 0x6000);
        assert!(spans.note(105_998, a, a + 0x2c000, true).is_empty());
        assert!(spans.note(106_004, b, b + 0x2c000, true).is_empty());
        assert!(spans.note(108_521, c, c + 0x2c000, true).is_empty());
        // The first written again (as a title does when levels arrive),
        // from another slot too: its span reaches over the other two.
        assert!(spans.note(7, a, a + 0x2c000, true).is_empty());
        assert!(spans.note(106_004, b, b + 0x2c000, true).is_empty());
        let mut all = spans.slots_in(0, u64::MAX);
        all.sort_unstable();
        assert_eq!(all, [7, 105_998, 106_004, 108_521]);
        // A draw makes a slot of the second texture: it is one of them.
        assert!(spans.note(9, b, b + 0x2c000, false).is_empty());
        assert_eq!(spans.by_slot.get(&9), Some(&b));
    }

    /// A texture written where another starts replaces it: at the same
    /// address with another length, or inside the block of its smallest
    /// levels. The packed neighbours of the one replaced stay.
    #[test]
    fn a_texture_written_at_the_start_of_another_replaces_it() {
        let mut spans = Spans::default();
        let at = 0x7_0000_0000u64;
        assert!(spans.note(1, at, at + 0x2c000, true).is_empty());
        assert!(spans.note(2, at + 0x2000, at + 0x2e000, true).is_empty());
        // Another length from the same start: slot 1 is stale, slot 2 not.
        assert_eq!(spans.note(3, at, at + 0xb000, true), [1]);
        // A start inside the first 4 KiB of slot 3's texture: stale too.
        assert_eq!(spans.note(4, at + 0x800, at + 0x1800, true), [3]);
        // And the other way: a texture that starts just before one.
        assert_eq!(spans.note(5, at + 0x1800, at + 0x4800, true), [2]);
        let mut left = spans.slots_in(0, u64::MAX);
        left.sort_unstable();
        assert_eq!(left, [4, 5]);
        // Exactly 4 KiB apart: both there.
        assert!(spans.note(6, at + 0x2800, at + 0x3800, true).is_empty());
        // A draw's view that clashes is left out, and nothing is stale.
        assert!(spans.note(8, at + 0x900, at + 0x2900, false).is_empty());
        assert_eq!(spans.by_slot.get(&8), None);
        assert_eq!(spans.by_slot.len(), 3);
    }

    /// A slot written with another texture leaves its old span; the same
    /// view written again changes nothing.
    #[test]
    fn a_slot_rewritten_moves_to_its_new_span() {
        let mut spans = Spans::default();
        assert!(spans.note(1, 0x10000, 0x3c000, true).is_empty());
        assert!(spans.note(2, 0x12000, 0x3e000, true).is_empty());
        assert!(spans.note(1, 0x10000, 0x3c000, true).is_empty());
        assert_eq!(spans.slots_in(0x10000, 0x10001), [1]);
        // Slot 1 now views a texture far away.
        assert!(spans.note(1, 0x90000, 0x91000, true).is_empty());
        assert_eq!(spans.slots_in(0x10000, 0x10001), [] as [u32; 0]);
        assert_eq!(spans.slots_in(0x12000, 0x12001), [2]);
        assert_eq!(spans.slots_in(0x90000, 0x90001), [1]);
        spans.forget(1);
        spans.forget(2);
        assert!(spans.by_start.is_empty() && spans.by_slot.is_empty());
    }

    /// Memory the runtime maps, unmaps or writes names the textures that
    /// are there: among packed neighbours the one whose held levels the
    /// range lies in (it ends where the next one starts), and the ones
    /// that start inside the range.
    #[test]
    fn memory_changed_under_packed_neighbours_names_the_ones_there() {
        let mut spans = Spans::default();
        let at = 0x7_0000_0000u64;
        assert!(spans.note(1, at, at + 0x2c000, true).is_empty());
        assert!(spans.note(2, at + 0x2000, at + 0x2e000, true).is_empty());
        assert!(spans.note(3, at + 0x6000, at + 0x32000, true).is_empty());
        // Inside the first one's held levels.
        assert_eq!(spans.slots_in(at + 0x1000, at + 0x2000), [1]);
        // Inside the second's: not the first, which ends where it starts.
        assert_eq!(spans.slots_in(at + 0x3000, at + 0x4000), [2]);
        // Past the last start: the last one.
        assert_eq!(spans.slots_in(at + 0x20000, at + 0x21000), [3]);
        // A range over all of them.
        assert_eq!(spans.slots_in(at, at + 0x8000), [1, 2, 3]);
        // Past the end of all: none.
        assert_eq!(spans.slots_in(at + 0x32000, at + 0x33000), [] as [u32; 0]);
    }

    /// A write names the textures that hold its bytes: the one that
    /// starts where the write starts, not the neighbour before it, whose
    /// held levels end there.
    #[test]
    fn a_write_at_the_start_of_a_packed_texture_names_it_alone() {
        let mut spans = Spans::default();
        let at = 0x7_0000_0000u64;
        assert!(spans.note(1, at, at + 0x2c000, true).is_empty());
        assert!(spans.note(2, at + 0x2000, at + 0x2e000, true).is_empty());
        assert!(spans.note(3, at + 0x6000, at + 0x32000, true).is_empty());
        // A request that fills the second texture's held levels.
        assert_eq!(spans.slots_in(at + 0x2000, at + 0x6000), [1, 2]);
        assert_eq!(spans.slots_holding(at + 0x2000, at + 0x6000), [2]);
        // From inside the first one's held levels on: both.
        assert_eq!(spans.slots_holding(at + 0x1000, at + 0x3000), [1, 2]);
        // A whole 64 KiB page: every texture that starts in it.
        assert_eq!(spans.slots_holding(at, at + 0x10000), [1, 2, 3]);
        // Past the last start, the last one; past all, or no bytes, none.
        assert_eq!(spans.slots_holding(at + 0x20000, at + 0x21000), [3]);
        assert_eq!(spans.slots_holding(at + 0x32000, at + 0x33000), [] as [u32; 0]);
        assert_eq!(spans.slots_holding(at + 0x2000, at + 0x2000), [] as [u32; 0]);
    }

    /// A write behind the guards' back under a placed texture: a slot
    /// waiting for its first upload is left alone, one uploaded before is
    /// counted, and made due two frames on when that is asked for. (The
    /// ranges are handed over here, not through the list all heaps share.)
    #[test]
    fn a_page_write_makes_an_uploaded_texture_slot_due_when_asked() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        let slots = |at: u64| -> Vec<u32> { heap.texture_slots_due(at, 8).0.into_iter().map(|d| d.0).collect() };
        // 64x64 BC7, one mip, at an address no test and no title uses.
        let at = 0x7fed_0000_0000u64;
        let mut desc = [0u32; 16];
        (desc[10], desc[11]) = (at as u32, (at >> 32) as u32);
        let view = View::Placed(false, [3, 0, 0, 0, 64, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0], desc);
        assert!(heap.note_buffer(1, Some(&view), false, 1).is_empty());
        let written = [(at, at + 0x1000)];
        // Waiting for its first upload: held, not uploaded, nothing done.
        assert_eq!(heap.slots_written_under(&written, 50, true), [1, 0, 0]);
        let (queued, write) = heap.texture_slots_due(u64::MAX, 8).0[0];
        assert_eq!(queued, 1);
        heap.texture_slot_done(1, write, None);
        assert_eq!(slots(u64::MAX), []);
        // Uploaded and out of the queue. Memory elsewhere: nothing.
        assert_eq!(heap.slots_written_under(&[(at + 0x10_0000, at + 0x10_1000)], 60, true), [0, 0, 0]);
        // Under it, counted only: it stays out of the queue.
        assert_eq!(heap.slots_written_under(&written, 60, false), [1, 1, 0]);
        assert_eq!(slots(u64::MAX), []);
        // Under it, asked for: available on the publishing frame.
        assert_eq!(heap.slots_written_under(&written, 60, true), [1, 1, 1]);
        assert_eq!(slots(59), []);
        assert_eq!(slots(60), [1]);
    }

    /// `VICTORY_STALE_TEXTURES=0`: nothing is ever stale, and a view that
    /// overlaps others is left out of the spans, as before.
    #[test]
    fn never_stale_leaves_overlapping_views_out() {
        let mut spans = Spans { rule: Rule::Never, ..Default::default() };
        assert!(spans.note(1, 0x1000, 0x3000, true).is_empty());
        assert!(spans.note(2, 0x2000, 0x4000, true).is_empty());
        assert!(spans.note(3, 0x1000, 0x2000, true).is_empty());
        assert_eq!(spans.slots_in(0, u64::MAX), [1]);
    }

    /// The rule until run 188 (`VICTORY_STALE_TEXTURES=overlap`): a view
    /// written over memory that older views describe otherwise makes
    /// those stale; views of the same span are one texture's.
    #[test]
    fn a_newer_view_of_other_memory_makes_the_older_stale() {
        let mut spans = Spans { rule: Rule::Overlap, ..Default::default() };
        // Three textures side by side, the first in two slots.
        assert!(spans.note(1, 0x1000, 0x3000, true).is_empty());
        assert!(spans.note(2, 0x1000, 0x3000, true).is_empty());
        assert!(spans.note(3, 0x3000, 0x4000, true).is_empty());
        assert!(spans.note(4, 0x8000, 0x9000, true).is_empty());
        // The same slot written with the same span again: nothing changes.
        assert!(spans.note(2, 0x1000, 0x3000, true).is_empty());
        assert_eq!(spans.slots_in(0x2000, 0x3800), [1, 2, 3]);
        assert_eq!(spans.slots_in(0x4000, 0x8000), [] as [u32; 0]);
        // A draw makes a slot whose view overlaps them: left out, nothing stale.
        assert!(spans.note(9, 0x2000, 0x3800, false).is_empty());
        assert_eq!(spans.slots_in(0x2000, 0x3800), [1, 2, 3]);
        // The title writes a texture over the end of the first and all of
        // the second: their three slots are stale.
        let mut stale = spans.note(5, 0x2000, 0x4000, true);
        stale.sort_unstable();
        assert_eq!(stale, [1, 2, 3]);
        assert_eq!(spans.slots_in(0, u64::MAX), [5, 4]);
        // Another start in the same memory: stale in turn. Another length
        // from the same start too.
        assert_eq!(spans.note(6, 0x2800, 0x3000, true), [5]);
        assert_eq!(spans.note(7, 0x2800, 0x2c00, true), [6]);
        // A slot that moves to another texture leaves its span behind.
        assert!(spans.note(7, 0x8000, 0x9000, true).is_empty());
        assert_eq!(spans.slots_in(0x2800, 0x2c00), [] as [u32; 0]);
        assert_eq!(spans.slots_in(0x8000, 0x8001), [4, 7]);
        spans.forget(4);
        spans.forget(7);
        assert!(spans.by_start.is_empty() && spans.by_slot.is_empty());
    }

    /// Memory the runtime maps under a slot's texture makes the slot due
    /// again in the same frame; a stale slot leaves the queue.
    #[test]
    fn remapped_memory_makes_its_texture_slots_due() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        let slots = |at: u64| -> Vec<u32> { heap.texture_slots_due(at, 8).0.into_iter().map(|d| d.0).collect() };
        // 64x64 BC7, one mip, at an address no test and no title uses.
        let at = 0x7fee_0000_0000u64;
        let view = |address: u64, width: u32| {
            let mut desc = [0u32; 16];
            (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
            View::Placed(false, [3, 0, 0, 0, width, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0], desc)
        };
        let len = crate::resource::placed_bytes(&[3, 0, 0, 0, 64, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0]).unwrap();
        assert!(heap.note_buffer(1, Some(&view(at, 64)), false, 1).is_empty());
        let (queued, write) = heap.texture_slots_due(u64::MAX, 8).0[0];
        assert_eq!(queued, 1);
        heap.texture_slot_done(1, write, None);
        assert_eq!(slots(u64::MAX), []);
        // Memory elsewhere: nothing. Under the texture: due in this frame.
        remapped((at + 2 * len) as usize, 0x1000);
        remapped(at as usize, 0x1000);
        assert_eq!(heap.texture_slots_remapped(50), 1);
        assert_eq!(slots(49), []);
        assert_eq!(slots(50), [1]);
        // A wider texture written at the same address: slot 1 is stale.
        assert_eq!(heap.note_buffer(2, Some(&view(at, 128)), false, 2), [(1, 0)]);
        assert!(heap.superseded.may(1));
        assert!(heap.superseded_record(1, || 1));
        assert!(!heap.superseded_record(1, || 3));
        // A speculative preparation of the same guest record cannot put it
        // back in the upload queue. A new guest write can reuse the slot.
        assert!(heap.note_buffer(1, Some(&view(at, 64)), true, 1).is_empty());
        assert_eq!(slots(u64::MAX), [2]);
        heap.note_buffer(1, Some(&view(at + 2 * len, 64)), false, 3);
        assert!(!heap.superseded.may(1));
        assert!(!heap.superseded_record(1, || 1));
    }

    /// A write of no placed texture view forgets the slot in the spans
    /// whether it goes by their lock or not (`spans_quick`): slots that are
    /// in them stay marked, also one another's view made stale.
    #[test]
    fn writes_of_other_views_leave_the_spans_as_under_their_lock() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        let marked = |slot: u32| heap.spanned.all(u64::MAX).contains(&slot);
        let at = 0x7fed_0000_0000u64;
        let view = |address: u64, width: u32| {
            let mut desc = [0u32; 16];
            (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
            View::Placed(false, [3, 0, 0, 0, width, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0], desc)
        };
        assert!(heap.note_buffer(1, Some(&view(at, 64)), false, 1).is_empty());
        assert!(marked(1) && !marked(2));
        // Slot 1 made stale by slot 2: out of `by_slot`, still in `first`.
        assert_eq!(heap.note_buffer(2, Some(&view(at, 128)), false, 2), [(1, 0)]);
        assert!(marked(1) && marked(2));
        assert!(heap.placed.lock().unwrap().first.contains_key(&1));
        // Written with no view: forgotten, under the lock, and unmarked.
        assert!(heap.note_buffer(1, None, false, 0).is_empty());
        assert!(!marked(1) && !heap.placed.lock().unwrap().first.contains_key(&1));
        // A slot never in the spans: nothing there to change.
        assert!(heap.note_buffer(3, None, false, 0).is_empty());
        assert!(heap.note_buffer(2, None, false, 0).is_empty());
        let spans = heap.placed.lock().unwrap();
        assert!(spans.by_slot.is_empty() && spans.by_start.is_empty() && spans.first.is_empty());
        assert!(heap.spanned.all(u64::MAX).is_empty());
    }

    #[test]
    fn zero_address_placed_views_preserve_their_typed_null() {
        let mut resource = [0; 14];
        resource[0] = 3;
        let mut desc = [0; 16];
        desc[1] = 4;
        desc[2] = 5768;
        desc[5] = 15;
        for unordered in [false, true] {
            resource[8] = if unordered { 28 } else { 72 };
            let view = placed_texture(&View::Placed(unordered, resource, desc)).unwrap();
            assert!(explicit_null_texture(&view));
            let words = match view {
                View::Shader(0, Some(words)) if !unordered => words,
                View::Unordered(0, 0, Some(words)) if unordered => words,
                _ => panic!("unexpected null descriptor"),
            };
            assert_eq!(&words[..2], &[resource[8], 4]);
        }
    }

    #[test]
    fn zero_element_placed_buffers_publish_both_null_presentations() {
        let Some(heap) = HostHeap::new(false, 4) else { return };
        let mut resource = [0; 14];
        resource[0] = 1;
        for unordered in [false, true] {
            let mut d = [0; 16];
            d[1] = 1;
            d[if unordered { 5 } else { 7 }] = 16;
            d[10] = 0x280c_ec80;
            let view = View::Placed(unordered, resource, d);
            assert!(matches!(make_twinned(&view, heap.cpu(0), Some(heap.cpu(1))), Some(None)));
            d[if unordered { 4 } else { 6 }] = 1;
            let outside = View::Placed(unordered, resource, d);
            assert!(make_twinned(&outside, heap.cpu(0), Some(heap.cpu(1))).is_none());
        }
    }

    #[test]
    fn a_set_of_slots_holds_what_was_put_in() {
        let set = Slots::new(200);
        for slot in [199, 0, 64, 63, 130, 64] {
            set.put(slot);
        }
        assert_eq!(set.all(u64::MAX), [0, 63, 64, 130, 199]);
        assert_eq!(set.part::<2>(0), [0, 64, 130]);
        assert!(set.remove(64) && !set.remove(64) && !set.remove(5));
        assert_eq!(set.take(), [0, 63, 130, 199]);
        assert!(set.take().is_empty());
    }

    #[test]
    fn spread_scan_visits_each_live_slot_in_its_original_part() {
        let set = Slots::new(201);
        let live: Vec<u32> = (0..201).filter(|slot| slot % 5 != 2).collect();
        for &slot in &live {
            set.put(slot);
        }
        for part in 0..16 {
            let expected: Vec<_> = live.iter().copied().filter(|slot| slot % 16 == part).collect();
            assert_eq!(set.part::<16>(part), expected);
        }
        assert!(set.part::<16>(16).is_empty());
        assert_eq!(set.take(), live);
    }

    /// A slot a draw read through a table leaves the queue; the title's
    /// next write of it waits for its draw, and is due after the grace
    /// frames if none reads it.
    #[test]
    fn slots_read_through_tables_leave_the_texture_queue() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        let frame = 100;
        let slots = |at: u64| -> Vec<u32> { heap.texture_slots_due(at, 8).0.into_iter().map(|d| d.0).collect() };
        let texture = View::Placed(false, [3, 0, 0, 0, 4, 0, 4, 0x1_0001, 28, 1, 0, 0x105, 0, 0], [0; 16]);
        assert!(is_texture_view(&texture));
        heap.queue_texture(2, true, false, frame);
        heap.queue_texture(3, true, false, frame);
        assert_eq!(slots(frame), [3, 2]);
        heap.tabled_in(3, true, frame);
        assert_eq!(slots(frame), [2]);
        // Made in place by the draw that read it: still out.
        heap.queue_texture(3, true, true, frame);
        assert_eq!(slots(frame), [2]);
        // Written again in the next frame: due once the grace frames have
        // passed without a draw that reads it.
        heap.queue_texture(3, true, false, frame + 1);
        assert_eq!(slots(frame + TABLED_GRACE), [2]);
        assert_eq!(slots(frame + 1 + TABLED_GRACE), [3, 2]);
        // The draw comes after all: out again.
        heap.tabled_in(3, true, frame + 1);
        assert_eq!(slots(u64::MAX), [2]);
        // A slot of a table no draw read stays queued; its next write waits.
        heap.tabled_in(2, false, frame + 1);
        assert_eq!(slots(frame + 1), [2]);
        heap.queue_texture(2, true, false, frame + 2);
        assert_eq!(slots(frame + 2), []);
        // Long after the last table, a write is due at once.
        heap.queue_texture(2, true, false, frame + 3 + TABLED_FRAMES);
        assert_eq!(slots(frame + 3 + TABLED_FRAMES), [2]);
    }

    #[test]
    fn dynamic_slots_are_held_until_released() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        let a = heap.dynamic(heap.dynamic - 100).unwrap();
        let b = heap.dynamic(60).unwrap();
        assert_eq!((a, b), (4, 4 + heap.dynamic - 100));
        let (buffer, _) = host().unwrap().upload_buffer(256).unwrap();
        heap.keep(a, Some(buffer.clone()));
        heap.keep(b, Some(buffer.clone()));
        // 40 left at the end: 50 do not fit until something is given back.
        assert_eq!(heap.dynamic(50), None);
        heap.release(a, 50);
        assert!(heap.kept_in(a).is_none());
        assert!(heap.keeps_resource(b, &buffer));
        assert_eq!(heap.dynamic(50), Some(4));
        assert_eq!(heap.dynamic(40), Some(4 + heap.dynamic - 40));
    }

    #[test]
    fn dynamic_search_can_find_a_run_across_the_cursor() {
        let Some(heap) = HostHeap::new(false, 4) else {
            eprintln!("no host descriptor heap; skipped");
            return;
        };
        let first = heap.dynamic(1).unwrap();
        let left = heap.dynamic(2).unwrap();
        let right = heap.dynamic(2).unwrap();
        let rest = heap.dynamic(heap.dynamic - 5).unwrap();
        assert_eq!((first, left, right, rest), (4, 5, 7, 9));

        // Reallocating the left half puts the cursor back in the middle.
        heap.release(left, 2);
        assert_eq!(heap.dynamic(2), Some(left));
        heap.release(left, 2);
        heap.release(right, 2);
        assert_eq!(heap.dynamic(4), Some(left));
        assert_eq!(heap.dynamic(1), None);

        // A free slot at each physical end does not form a two-slot range.
        heap.release(first, 1);
        let last = first + heap.dynamic - 1;
        heap.release(last, 1);
        assert_eq!(heap.dynamic(2), None);
    }
}

/// What is known of a record a list's tables hold, in that recording of the
/// list ([`HostHeap::make_listed`]).
#[derive(Clone, Copy)]
pub(crate) struct Listed {
    pub(crate) kind: Option<victory_shader::native::ResourceType>,
    pub(crate) texture: bool,
    /// Read at this epoch of the list ([`crate::host::HostList::epoch`]) with
    /// this depth target tested: up to date; and whether it is that target's.
    pub(crate) ready: (u64, usize, bool),
    /// The title's slot last noted for it, and whether the draw read it.
    pub(crate) tabled: Option<(u32, bool)>,
}

struct ListView {
    view: View,
    /// Its descriptor's slot in [`ListViews`]; its raw twin's is the next.
    at: u32,
    twin: bool,
    kept: Kept,
    listed: Listed,
}

/// The descriptors a thread made for one recording of a list, by the record
/// they were made from, in a heap the card does not see.
///
/// The title's culling and lighting dispatches each bind a table of their own
/// that differs in one constant buffer, and the same thirty views beside it:
/// each was found, checked, made and brought up to date anew for every
/// dispatch, some 2,500 indirect dispatches in the one recording job the
/// frame waits for last (timeline waits-2: 24 of 65 ms). Here a list makes a
/// view once, as it makes a table once (`crate::draw::State::tables`), and
/// copies its descriptor.
struct ListViews {
    recording: u64,
    heap: ID3D12DescriptorHeap,
    step: usize,
    start: usize,
    made: std::collections::HashMap<[u32; 3], ListView, std::hash::BuildHasherDefault<crate::host::AddressHash>>,
}

impl ListViews {
    const SLOTS: u32 = 8192;

    fn new() -> Option<ListViews> {
        let host = host()?;
        // SAFETY: plain object creation.
        unsafe {
            let heap: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: Self::SLOTS,
                    ..Default::default()
                })
                .ok()?;
            let step = host.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            let start = heap.GetCPUDescriptorHandleForHeapStart().ptr;
            Some(ListViews { recording: 0, heap, step, start, made: Default::default() })
        }
    }

    fn slot(&self, i: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        debug_assert!(i < Self::SLOTS && !windows::core::Interface::as_raw(&self.heap).is_null());
        D3D12_CPU_DESCRIPTOR_HANDLE { ptr: self.start + i as usize * self.step }
    }
}

thread_local! {
    static LIST_VIEWS: std::cell::RefCell<Option<ListViews>> = const { std::cell::RefCell::new(None) };
}

/// The view the record `words` names, as this thread made it for `recording`.
pub(crate) fn listed_view(recording: u64, words: [u32; 3]) -> Option<View> {
    LIST_VIEWS.with(|views| {
        let views = views.borrow();
        let views = views.as_ref().filter(|v| v.recording == recording)?;
        views.made.get(&words).map(|m| m.view.clone())
    })
}

/// Notes what is known of the record `words` in `recording`.
pub(crate) fn note_listed(recording: u64, words: [u32; 3], listed: Listed) {
    LIST_VIEWS.with(|views| {
        if let Some(views) = views.borrow_mut().as_mut().filter(|v| v.recording == recording)
            && let Some(made) = views.made.get_mut(&words)
        {
            made.listed = listed;
        }
    });
}

/// Whether a list makes a view its tables hold once (`VICTORY_LIST_VIEWS=0`:
/// for every table).
pub(crate) fn list_views() -> bool {
    crate::toggled!("VICTORY_LIST_VIEWS")
}
