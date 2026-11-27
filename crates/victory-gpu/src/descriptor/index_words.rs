//! Guest table-buffer words that supply sampled descriptor indices.
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Weak};

use super::{Heap, View};
use super::host::{HostHeap, buffer_copy};
mod material_chain;
mod scalar_records;
mod scalar_program;
pub(crate) use scalar_program::ScalarPrograms;
pub(crate) use material_chain::{IndirectGroups, MaterialWords};
pub(crate) use scalar_records::DependentWords;

struct NativeOwner<T>(Weak<T>);
impl<T> Clone for NativeOwner<T> {
    fn clone(&self) -> Self { Self(self.0.clone()) }
}
impl<T> std::fmt::Debug for NativeOwner<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.0.as_ptr().fmt(f) }
}

#[derive(Debug, Clone, Copy)]
enum Outcome {
    Captured, MissingRecord, UnsupportedView, UnknownShadow, UnsafeShadow, UnalignedWord,
    TooLarge, ReleasedHeap, ChangedRecord, ChangedShadow, OverlappingProducer, Unreadable,
    ChangedDuringRead, Read, DifferentNativeBuffer, UnverifiedMapping, GpuOnlySource, NativeBytesDiffer, QueuedWrite, MissingNativeBytes, ChangedNativeBinding, PendingWriter, ChangedBounds, OrderedSource,
}

#[cfg(feature = "diagnostics")]
#[derive(Clone, Copy, Debug)]
pub(crate) struct MissingWord {
    pub(crate) reason: &'static str,
    pub(crate) logical: u32,
    pub(crate) address: u64,
    pub(crate) bytes: u64,
    pub(crate) step: u32,
}
#[cfg(feature = "diagnostics")]
thread_local! {
    static MISSING_WORD: std::cell::Cell<Option<MissingWord>> = const { std::cell::Cell::new(None) };
}
#[cfg(feature = "diagnostics")]
pub(crate) fn take_missing_word() -> Option<MissingWord> { MISSING_WORD.take() }

/// Print the first observation of each outcome and scan stride across recording workers.
#[inline]
fn note(outcome: Outcome, step: u32, logical: u32, address: u64, bytes: u64, ownership: Option<(bool, bool, bool)>) {
    #[cfg(feature = "diagnostics")]
    {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        let reason = match outcome {
            Outcome::MissingNativeBytes => "GPU bytes absent from this occurrence's readback",
            Outcome::QueuedWrite => "GPU write overlaps the source before its consumer",
            Outcome::OverlappingProducer => "texture producer overlaps the word",
            Outcome::ChangedShadow => "captured shadow identity changed",
            Outcome::ChangedNativeBinding => "native descriptor now names another buffer",
            Outcome::ChangedBounds => "buffer view address or size changed",
            Outcome::OrderedSource => "buffer mapping or ordered read presentation changed",
            Outcome::PendingWriter => "an earlier native writer is still in flight",
            Outcome::ChangedRecord => "descriptor publication changed",
            Outcome::ReleasedHeap => "descriptor heap no longer available",
            Outcome::NativeBytesDiffer => "CPU data differs from the native buffer",
            Outcome::UnverifiedMapping => "physical mapping cannot be verified",
            Outcome::ChangedDuringRead => "mapping changed while capturing the word",
            Outcome::Unreadable => "guest word unreadable",
            Outcome::TooLarge => "read exceeds the selector capture budget",
            Outcome::UnalignedWord => "unaligned selector word",
            _ => "",
        };
        if !reason.is_empty() { MISSING_WORD.set(Some(MissingWord { reason, logical, address, bytes, step })); }
        static SEEN: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
        let seen = &SEEN[match step { 0 => 0, 1 => 1, _ => 2 }];
        let bit = 1 << outcome as u32;
        if seen.load(Relaxed) & bit == 0 && seen.fetch_or(bit, Relaxed) & bit == 0 {
            let (in_place, written, ordered) = ownership.unwrap_or_default();
            tracing::info!(?outcome, step, logical, address = format_args!("{address:#x}"), bytes,
                in_place, written, ordered, "sampled texture index source");
        }
    }
    #[cfg(not(feature = "diagnostics"))]
    let _ = (outcome, step, logical, address, bytes, ownership);
}

impl<T> PartialEq for NativeOwner<T> {
    fn eq(&self, other: &Self) -> bool { self.0.ptr_eq(&other.0) }
}
impl<T> Eq for NativeOwner<T> {}
impl<T> Hash for NativeOwner<T> {
    fn hash<H: Hasher>(&self, state: &mut H) { self.0.as_ptr().hash(state); }
}

#[derive(Clone)]
struct FrozenBuffer(Arc<crate::shadow::Shadow>);
impl std::fmt::Debug for FrozenBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.0.id().fmt(f) }
}
impl PartialEq for FrozenBuffer {
    fn eq(&self, other: &Self) -> bool { Arc::ptr_eq(&self.0, &other.0) }
}
impl Eq for FrozenBuffer {}
impl Hash for FrozenBuffer {
    fn hash<H: Hasher>(&self, state: &mut H) { Arc::as_ptr(&self.0).hash(state); }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct IndexWords {
    title: usize,
    owner: NativeOwner<HostHeap>,
    logical: u32,
    shown: u32,
    frozen: Option<FrozenBuffer>,
    card_buffer: Option<NativeOwner<crate::shadow::Shadow>>,
    volatile_word: bool,
    record: [u32; 3],
    shadow: u64,
    in_place: bool,
    written: bool,
    base: u64,
    size: u64,
    offset: u32,
    step: u32,
    bias: u32,
    records: Option<RecordWindow>,
    value_mask: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct RecordWindow {
    start: u64, stride: u32, words: u32, out_of_bounds: bool,
    float: bool,
}

impl RecordWindow {
    fn new(field: u32, first: u32, count: u32, stride: u32, size: u64) -> Option<Self> {
        if stride < 4 || !stride.is_multiple_of(4) || !field.is_multiple_of(4) || field.checked_add(4)? > stride { return None; }
        if count == 0 { return Some(Self { start: 0, stride, words: 0, out_of_bounds: false, float: true }); }
        // The emitted buffer address uses 32-bit multiply/add. If it wraps,
        // the accesses no longer describe one interval in the declared view.
        first.checked_add(count - 1)?.checked_mul(stride)?.checked_add(field)?.checked_add(3)?;
        let start = u64::from(first) * u64::from(stride) + u64::from(field);
        let available = size.checked_sub(start + 4).map_or(0, |rest| 1 + rest / u64::from(stride));
        let words = u64::from(count).min(available) as u32;
        Some(Self { start: start.min(size), stride, words, out_of_bounds: words != count, float: true })
    }

    fn integer_column(start: u32, stride: u32, size: u64) -> Option<Self> {
        let words = size.checked_sub(u64::from(start) + 4).map_or(0, |rest| 1 + rest / u64::from(stride));
        Some(Self { start: u64::from(start).min(size), stride, words: words.try_into().ok()?, out_of_bounds: true, float: false })
    }

    fn len(self) -> usize {
        if self.words == 0 { 0 } else { (self.words as usize - 1) * self.stride as usize + 4 }
    }

    fn decode(self, data: &[u8], mask: u32, bias: u32, limit: u32, out: &mut BTreeSet<u32>) {
        for four in data.windows(4).step_by(self.stride as usize) {
            let word = u32::from_le_bytes(four.try_into().unwrap());
            let value = if self.float { f32::from_bits(word) as u32 } else { word };
            insert_slot(value & mask, bias, limit, out);
        }
        if self.out_of_bounds { insert_slot(0, bias, limit, out); }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SampleWords {
    pub index: Option<IndexWords>,
    pub guard: Option<IndexWords>,
    pub chain: Option<Box<DependentWords>>,
}

pub(crate) type DeviceRanges = std::collections::BTreeMap<(u64, usize, usize), Arc<crate::shadow::Shadow>>;
pub(crate) type DeviceBytes = std::collections::BTreeMap<(u64, usize, usize), Vec<u8>>;
pub(crate) const SELECTOR_READ_BUDGET: usize = 64 * 1024;
pub(crate) const WIDE_SELECTOR_READ_BUDGET: usize = 512 * 1024;

impl RecordWindow {
    // Tiny sparse gathers cost at most 64 copy commands. For larger compact
    // tables copy one interval and extract its columns from that snapshot.
    fn copy_interval(self) -> bool {
        self.words > 64 && self.stride <= 16 && self.len() <= SELECTOR_READ_BUDGET
    }
}

impl SampleWords {
    pub(crate) fn direct(mut index: Option<IndexWords>, mut guard: Option<IndexWords>) -> Self {
        // A volatile word can acquire a GPU writer between list construction
        // and execution. Reserve its ordered four-byte read at the first draw,
        // before the recorder decides whether it can group that occurrence.
        for input in [&mut index, &mut guard].into_iter().flatten() {
            input.volatile_word = input.step == 0 && input.records.is_none();
        }
        Self { index, guard, chain: None }
    }

    pub(crate) fn on_device(&self) -> bool {
        self.chain.as_deref().is_some_and(DependentWords::on_device) ||
            [&self.index, &self.guard].into_iter().flatten().any(|input| input.device_input().is_some())
    }

    pub(crate) fn device_ranges(&self, ranges: &mut DeviceRanges) {
        let mut add = |source: &IndexWords, (at, len): (usize, usize)| {
            if let Some(buffer) = source.device_input() && len != 0 {
                ranges.entry((buffer.id(), at, len)).or_insert(buffer);
            }
        };
        if let Some(index) = &self.index {
            if let Some(records) = index.records {
                if index.device_input().is_some() {
                    let (at, _) = index.span();
                    if records.copy_interval() { add(index,index.span()); }
                    else { for i in 0..records.words { add(index, (at + i as usize * records.stride as usize, 4)); } }
                }
            } else { add(index, index.span()); }
        }
        if let Some(guard) = &self.guard && u64::from(guard.offset) + 8 <= guard.size {
            add(guard, ((guard.base + u64::from(guard.offset)) as usize, 8));
        }
    }

    pub(crate) fn values_after(&self, writes: &[(usize, usize)], heap_count: u32, out: &mut BTreeSet<u32>, device: Option<&DeviceBytes>) -> bool {
        if let Some(chain) = &self.chain {
            let mut missing = DeviceRanges::new();
            let complete = chain.evaluate(writes, heap_count, out, device, &mut missing);
            #[cfg(feature = "diagnostics")]
            if let Some(bytes) = device {
                use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
                static USES: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
                let scalar = matches!(**chain, DependentWords::Scalar { .. });
                let fields = matches!(**chain, DependentWords::Fields { .. });
                let program = matches!(**chain, DependentWords::Programs(_));
                let kind = usize::from(scalar) + 2 * usize::from(fields) + 3 * usize::from(program);
                let uses = USES[kind * 2 + usize::from(complete)].fetch_add(1, Relaxed) + 1;
                if uses.is_power_of_two() {
                    tracing::info!(uses, complete, scalar_records=scalar, pointer_fields=fields, scalar_program=program, native_spans=bytes.len(), still_missing=missing.len(),
                        sampled=out.len(), "validated a dependent material selector");
                }
            }
            return complete;
        }
        // The selector need not be accessible if the shader cannot reach it.
        // An unavailable guard leaves the ordinary selector scan in force.
        if let Some(guard) = &self.guard {
            let mut empty = false;
            if u64::from(guard.offset) + 8 <= guard.size {
                let at = (guard.base + u64::from(guard.offset)) as usize;
                guard.validated_bytes((at, 8), writes, device, |pair| {
                    let first = u32::from_le_bytes(pair[..4].try_into().unwrap());
                    let end = u32::from_le_bytes(pair[4..].try_into().unwrap());
                    empty = first >= end;
                    #[cfg(feature = "diagnostics")]
                    {
                        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
                        static READS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
                        let observations = READS[usize::from(empty)].fetch_add(1, Relaxed) + 1;
                        if observations.is_power_of_two() {
                            tracing::info!(first, end, empty, observations, "sampled material range");
                        }
                    }
                });
            }
            if empty { return true; }
        }
        self.index.as_ref().is_some_and(|index| index.values_after(writes, heap_count, out, device))
    }
}

impl IndexWords {
    /// Capture identities and bounds at the same root-table binding as the shader.
    pub(crate) fn capture(heap: &Heap, handle: u64, snapshot: Option<(u32, [u32; 3])>, offset: u32, step: u32, bias: u32, records: Option<(u32,u32,u32)>, value_mask: u32) -> Option<Self> {
        let logical = heap.index(handle)?;
        if heap.samplers() || heap.handle(logical)? != handle || (step != 0 && !step.is_power_of_two()) {
            return None;
        }
        let host = heap.host_owner()?;
        let owner = NativeOwner(Arc::downgrade(host));
        let shown = snapshot.map_or(logical, |(shown, _)| shown);
        let Some((record, view)) = snapshot.map(|(_, r)| r).or_else(|| super::record_at(handle))
            .and_then(|r| super::recorded_view(r).map(|v| (r, v))) else {
            note(Outcome::MissingRecord, step, logical, handle, 0, None);
            return None;
        };
        let constant = matches!(view, View::Constant(..));
        if !constant && !matches!(&view, View::Shader(_, Some(d)) if d[1] == 1)
            && !matches!(&view, View::Placed(false, r, d) if r[0] == 1 && d[1] == 1)
        { note(Outcome::UnsupportedView, step, logical, handle, 0, None); return None; }
        // An unaligned raw load cannot be narrowed to one unaligned CPU word.
        // A byte-stride scan already includes every possible dword alignment.
        if (!constant || !victory_shader::emit::mapped_views()) && step != 1 && offset % 4 != 0 {
            note(Outcome::UnalignedWord, step, logical, handle, u64::from(offset), None);
            return None;
        }
        let Some((shadow, base, size)) = buffer_copy(&view) else {
            note(Outcome::UnknownShadow, step, logical, handle, 0, None);
            return None;
        };
        let records = match records {
            Some((first,count,stride)) if !constant => {
                let range = RecordWindow::new(offset,first,count,stride,size)?;
                #[cfg(feature = "diagnostics")]
                {
                    static LOOKUPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    if (LOOKUPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1).is_power_of_two() {
                        tracing::info!(first, count, stride, field = offset, words = range.words, "sampled structured texture records");
                    }
                }
                Some(range)
            }
            Some(_) => return None,
            None => None,
        };
        shadow.offset_in(base)?;
        let ownership = shadow.selector_ownership();
        if ownership.2 || size > shadow.left(base) {
            note(Outcome::UnsafeShadow, step, logical, base, size, Some(ownership));
            return None;
        }
        // A cached table may still hold an older allocation with the same
        // guest descriptor, or this CBV may be an independent aligned copy.
        if !host.keeps_resource(shown, &shadow.buffer) {
            note(Outcome::DifferentNativeBuffer, step, logical, base, size, Some(ownership));
            return None;
        }
        base.checked_add(size)?;
        let device_input = ownership.1 && shadow.carded();
        let records = if shadow.carded() && records.is_none() && step >= 4 {
            Some(RecordWindow::integer_column(offset, step, size)?)
        } else { records };
        let mut card_candidate = shadow.carded();
        if card_candidate {
            let first = u64::from(offset).min(size);
            let remaining = size - first;
            let bytes = records.map_or_else(|| if step == 0 { if remaining >= 4 { 4 } else { 0 } } else { remaining },
                |r| u64::from(r.words) * 4);
            // A reflected offset without a known stride may use any byte in
            // the view. Keep its full GPU span available to the ordered
            // selector readback even when it is wider than a compact table.
            let budget = if step == 1 { WIDE_SELECTOR_READ_BUDGET } else { SELECTOR_READ_BUDGET };
            card_candidate = bytes <= 256 || records.map_or(bytes <= budget as u64, RecordWindow::copy_interval);
            if device_input {
                note(Outcome::GpuOnlySource, step, logical, base + first, bytes, Some(ownership));
                if !card_candidate { return None; }
            }
        }
        note(Outcome::Captured, step, logical, base, size, Some(ownership));
        Some(Self { title: heap as *const Heap as usize, owner, logical, shown, record,
            shadow: shadow.id(), volatile_word: false, card_buffer: card_candidate.then(|| NativeOwner(Arc::downgrade(&shadow))),
            frozen: snapshot.filter(|(slot, _)| *slot != logical).map(|_| FrozenBuffer(shadow)),
            in_place: ownership.0, written: ownership.1, base, size, offset, step, bias, records, value_mask })
    }

    // Raw-address writes can first mark a shared native allocation after this
    // command was recorded. Select its reading path for this occurrence.
    fn device_input(&self) -> Option<Arc<crate::shadow::Shadow>> {
        self.card_buffer.as_ref()?.0.upgrade().filter(|s| self.volatile_word || s.selector_ownership().1)
    }

    pub(crate) fn recording_static(&self) -> bool { !self.in_place && !self.written }

    pub(crate) fn span(&self) -> (usize, usize) {
        if let Some(records) = self.records { return ((self.base + records.start) as usize, records.len()); }
        let first = u64::from(self.offset).min(self.size);
        let remaining = self.size - first;
        let bytes = if self.step == 0 { if remaining >= 4 { 4 } else { 0 } } else { remaining };
        ((self.base + first) as usize, bytes as usize)
    }

    pub(crate) fn values(&self, heap_count: u32, out: &mut BTreeSet<u32>) -> bool {
        self.values_after(&[], heap_count, out, None)
    }

    /// A later level of a descriptor gather can name new words on the card.
    /// Request those ranges only after validating their producer ordering.
    fn dependent_bytes(
        &self, offset: u32, len: usize, writes: &[(usize, usize)], device: Option<&DeviceBytes>,
        pending: &mut DeviceRanges, use_bytes: impl FnOnce(&[u8]),
    ) -> bool {
        if !offset.is_multiple_of(4) || u64::from(offset) + len as u64 > self.size {
            return false;
        }
        let Some(at) = self.base.checked_add(u64::from(offset)).map(|at| at as usize) else { return false };
        if let Some(shadow) = self.device_input() {
            let key = (shadow.id(), at, len);
            if device.is_none_or(|d| !d.contains_key(&key)) {
                let overlap = crate::inplace::aliases_of_bytes(at, len)
                    .iter().any(|&(a, b)| crate::host::touches(writes, a, b));
                if !overlap {
                    pending.entry(key).or_insert(shadow);
                }
                note(if overlap { Outcome::QueuedWrite } else { Outcome::MissingNativeBytes },
                    self.step, self.logical, at as u64, len as u64, None);
                return false;
            }
        }
        self.validated_bytes((at, len), writes, device, use_bytes)
    }

    fn dependent_word(&self, offset: u32, writes: &[(usize, usize)], device: Option<&DeviceBytes>, pending: &mut DeviceRanges) -> Option<u32> {
        if !offset.is_multiple_of(4) { return None; }
        if u64::from(offset) + 4 > self.size {
            // A load outside the bound raw view returns zero. Verify that
            // the same SRV is still bound before accepting that result.
            return self.validated_bytes((self.base as usize, 0), writes, device, |_| {}).then_some(0);
        }
        let mut word = None;
        self.dependent_bytes(offset, 4, writes, device, pending,
            |b| word = Some(u32::from_le_bytes(b.try_into().unwrap())));
        word
    }

    fn values_after(&self, writes: &[(usize, usize)], heap_count: u32, out: &mut BTreeSet<u32>, device: Option<&DeviceBytes>) -> bool {
        if let Some(records) = self.records {
            let (at, len) = self.span();
            if self.device_input().is_some() && len != 0 && !records.copy_interval() {
                for i in 0..records.words {
                    let word = at + i as usize * records.stride as usize;
                    if !self.validated_bytes((word,4), writes, device, |b| records.decode(b, self.value_mask, self.bias, heap_count, out)) { return false; }
                }
                return true;
            }
            return self.validated_bytes((at,len), writes, device, |b| records.decode(b, self.value_mask, self.bias, heap_count, out));
        }
        self.validated_bytes(self.span(), writes, device, |bytes| collect_slots(bytes, self.step, self.bias, self.value_mask, heap_count, out))
    }

    fn validated_bytes(&self, (address, len): (usize, usize), writes: &[(usize, usize)], device: Option<&DeviceBytes>, use_bytes: impl FnOnce(&[u8])) -> bool {
        const MAX_SCAN: usize = 1024 * 1024;
        let report = |result| { note(result, self.step, self.logical, address as u64, len as u64, None); false };
        if (address as u64) < self.base || (address as u64).checked_add(len as u64).is_none_or(|end| end > self.base + self.size) {
            return report(Outcome::ChangedShadow);
        }
        if len > MAX_SCAN { return report(Outcome::TooLarge); }
        let native = device.and_then(|d| d.get(&(self.shadow, address, len)));
        let Some(owner) = self.owner.0.upgrade() else { return report(Outcome::ReleasedHeap) };
        let _mapping = if self.in_place {
            let Some(guard) = crate::watch::title_memory_reading_now() else { return report(Outcome::UnverifiedMapping); };
            Some(guard)
        } else { None };
        super::with_heap(self.title, |heap| {
            if !heap.host_owner().is_some_and(|current| Arc::ptr_eq(current, &owner)) { return report(Outcome::ReleasedHeap); }
            let Some(handle) = heap.handle(self.logical) else { return report(Outcome::ChangedRecord) };
            let bound = || self.frozen.is_some() || super::record_at(handle) == Some(self.record);
            if !bound() { return report(Outcome::ChangedRecord); }
            let (shadow, base, size) = if let Some(frozen) = &self.frozen {
                (frozen.0.clone(), self.base, self.size)
            } else {
                let Some(binding) = super::recorded_view(self.record).as_ref().and_then(buffer_copy) else { return report(Outcome::UnknownShadow) };
                binding
            };
            if (base, size) != (self.base, self.size) { return report(Outcome::ChangedBounds); }
            let ownership = shadow.selector_ownership();
            let readable = || {
                if shadow.id() != self.shadow { return Err(Outcome::ChangedShadow); }
                if !owner.keeps_resource(self.shown, &shadow.buffer) { return Err(Outcome::ChangedNativeBinding); }
                if ownership.0 != self.in_place || ownership.2 || ownership != shadow.selector_ownership() {
                    return Err(Outcome::OrderedSource);
                }
                if shadow.writes_pending_in(address, address + len) { return Err(Outcome::PendingWriter); }
                Ok(())
            };
            if let Err(reason) = readable() { return report(reason); }
            let mut small = [0u8; 8];
            let mut large = Vec::new();
            // A texture created elsewhere also advances the global mapping/target
            // generation. Re-observe both this interval and its bytes if that
            // happens. No slots from an invalidated attempt enter the selection.
            for _ in 0..3 {
                let generation = crate::resource::alias_generation();
                let mut observed: &[u8] = &[];
                if len != 0 {
                    if self.in_place && !shadow.shares_guest_pages(address, len) { return report(Outcome::UnverifiedMapping); }
                    // Validate queued aliases and current producers in the same mapping
                    // generation as the bytes. Texture copy destinations can lack RT/UAV flags.
                    let ranges = crate::inplace::aliases_of_bytes(address, len);
                    if !writes.is_empty() && ranges.iter().any(|&(lo, hi)| crate::host::touches(writes, lo, hi)) {
                        return report(Outcome::QueuedWrite);
                    }
                    if crate::resource::texture_writer_overlaps(&ranges) {
                        return report(Outcome::OverlappingProducer);
                    }
                    if shadow.carded() && (self.volatile_word || ownership.1) && native.is_none() {
                        return report(Outcome::MissingNativeBytes);
                    }
                    if let Some(native) = native { observed = native; }
                    else {
                        let bytes = if len <= small.len() { &mut small[..len] } else {
                            large.resize(len, 0);
                            &mut large[..]
                        };
                        if !crate::host::read_into(address as *const u8, bytes) { return report(Outcome::Unreadable); }
                        if (ownership.1 || (self.frozen.is_some() && super::record_at(handle) != Some(self.record)))
                            && !self.in_place && !shadow.matches_native_bytes(address, bytes) { return report(Outcome::NativeBytesDiffer); }
                        observed = bytes;
                    }
                }
                if !bound() { return report(Outcome::ChangedRecord); }
                if let Err(reason) = readable() { return report(reason); }
                if generation == crate::resource::alias_generation() {
                    use_bytes(observed);
                    note(Outcome::Read, self.step, self.logical, address as u64, len as u64, None);
                    return true;
                }
            }
            report(Outcome::ChangedDuringRead)
        }).unwrap_or_else(|| report(Outcome::ReleasedHeap))
    }
}

fn insert_slot(word: u32, bias: u32, heap_count: u32, out: &mut BTreeSet<u32>) {
    let slot = (word << 5).wrapping_add(bias) >> 5;
    if slot < heap_count { out.insert(slot); }
}

fn collect_slots(bytes: &[u8], step: u32, bias: u32, value_mask: u32, heap_count: u32, out: &mut BTreeSet<u32>) {
    let mut append = |word| insert_slot(word & value_mask, bias, heap_count, out);
    // A dynamic offset can fall outside this interval and return zero.
    if step != 0 || bytes.len() < 4 { append(0); }
    for four in bytes.windows(4).step_by((step as usize).max(1)) {
        append(u32::from_le_bytes(four.try_into().unwrap()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_compact_device_table_keeps_only_the_reflected_column() {
        let mut words = vec![99u32; 192];
        for (i, pair) in words.chunks_exact_mut(2).enumerate() { pair[1] = 200 + i as u32; }
        let bytes: Vec<_> = words.into_iter().flat_map(u32::to_le_bytes).collect();
        let window = RecordWindow::integer_column(4, 8, bytes.len() as u64).unwrap();
        assert!(window.copy_interval());
        let mut slots = BTreeSet::new();
        window.decode(&bytes[4..], u32::MAX, 0, 1024, &mut slots);
        assert_eq!(slots, std::iter::once(0).chain(200..296).collect());
        assert!(!RecordWindow::integer_column(4,8,128 << 10).unwrap().copy_interval());
    }

    #[test]
    fn structured_selectors_read_only_the_converted_field_in_the_dispatch_range() {
        let window = RecordWindow::new(80, 2, 4, 176, 5 * 176).unwrap();
        assert_eq!((window.start, window.words, window.len()), (432, 3, 356));
        let mut bytes = vec![0x41; window.len()];
        for (index, f) in [47.75f32, f32::NAN, -9.0].into_iter().enumerate() {
            bytes[index * 176..index * 176 + 4].copy_from_slice(&f.to_le_bytes());
        }
        let mut slots = BTreeSet::new();
        window.decode(&bytes, u32::MAX, 32, 99, &mut slots);
        assert_eq!(slots, BTreeSet::from([1, 48]));
        assert!(RecordWindow::new(80, u32::MAX / 176, 2, 176, 4096).is_none());
        assert!(RecordWindow::new(80, u32::MAX, 0, 176, 4096).is_some_and(|r| r.words == 0 && !r.out_of_bounds));
        let raw = RecordWindow::integer_column(76, 512, 32768).unwrap();
        assert_eq!(raw.words, 64);
        let mut slots = BTreeSet::new();
        raw.decode(&0xfa00_0025u32.to_le_bytes(), 0xff_ffff, 0, 256, &mut slots);
        assert_eq!(slots, BTreeSet::from([0, 37]));
    }

    #[test]
    fn selector_words_cover_gpu_offsets_and_wrapping_indices() {
        let scan = |bytes: &[u8], step, bias, limit| {
            let mut out = BTreeSet::new();
            collect_slots(bytes, step, bias, u32::MAX, limit, &mut out);
            out.into_iter().collect::<Vec<_>>()
        };
        assert_eq!(scan(&[0xff; 4], 0, 32, 20), [0]);
        assert_eq!(scan(&[], 0, 64, 20), [2]);
        assert_eq!(scan(&[1, 0, 0, 0, 2, 0, 0, 0], 4, 32, 20), [1, 2, 3]);
        assert_eq!(scan(&[0, 2, 0, 0, 0], 1, 0, 20), [0, 2]);
        assert!(scan(&[21, 0, 0, 0], 0, 0, 20).is_empty());
    }
}
