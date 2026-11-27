use super::*;
use crate::shadow::Shadow;

// The original corona batches can contain 2048 48-byte vertices. Only an
// exact indexed selection may read this larger compact interval.
const INDEXED_VERTEX_READ_BUDGET: usize = 2 * SELECTOR_READ_BUDGET;

/// Index binding and arguments that will be given to a native indexed draw.
/// Callers using a strip cut must leave this proof unavailable.
pub(crate) struct VertexIndexView<'a> {
    pub(crate) shadow: &'a Arc<Shadow>,
    pub(crate) address: u64,
    pub(crate) size: u32,
    pub(crate) format: u32,
    pub(crate) first: u32,
    pub(crate) count: u32,
    pub(crate) base_vertex: i32,
}

/// Capture the exact native IA buffers, which have no descriptor records.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct VertexInputWords {
    vertex: VertexSource,
    selection: VertexSelection,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum VertexSelection {
    Columns(VertexColumns),
    Indexed(IndexWordsForIa),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct IndexWordsForIa {
    source: VertexSource,
    offset: u32,
    len: usize,
    width: usize,
    include_zero: bool,
    base_vertex: u32,
    vertex_stride: u32,
    fields: Vec<(u32, u32, u32)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct VertexSource {
    shadow: FrozenBuffer,
    address: usize,
    size: u32,
}

impl VertexInputWords {
    /// Fields are (byte offset in each vertex, descriptor byte bias, word mask).
    /// An absent vertex range covers all possible wrapped IA addresses.
    pub(crate) fn new(
        shadow: &Arc<Shadow>, base: u64, size: u32, stride: u32,
        vertices: Option<(u32, u32)>, fields: Vec<(u32, u32, u32)>,
    ) -> Option<Self> {
        let vertex = VertexSource::new(shadow, base, size, 4)?;
        let columns = VertexColumns::new(size, stride, vertices, fields)?;
        Some(Self { vertex, selection: VertexSelection::Columns(columns) })
    }

    pub(crate) fn new_indexed(
        shadow: &Arc<Shadow>, base: u64, size: u32, stride: u32,
        indices: VertexIndexView<'_>, mut fields: Vec<(u32, u32, u32)>,
    ) -> Option<Self> {
        let vertex = VertexSource::new(shadow, base, size, 4)?;
        if stride == 0 || !stride.is_multiple_of(4) || fields.is_empty()
            || fields.iter().any(|&(field, _, _)| !field.is_multiple_of(4)
                || field > stride - 4)
        {
            return None;
        }
        fields.sort_unstable();
        fields.dedup();
        let width = match indices.format { 57 => 2, 42 => 4, _ => return None };
        let source = VertexSource::new(indices.shadow, indices.address, indices.size, width)?;
        let (offset, available, len) = index_span(indices.size, width as u32, indices.first, indices.count)?;
        Some(Self { vertex, selection: VertexSelection::Indexed(IndexWordsForIa {
            source, offset, len, width: width as usize, include_zero: available < indices.count,
            base_vertex: indices.base_vertex as u32, vertex_stride: stride, fields,
        }) })
    }

    pub(super) fn device_sources(&self) -> [Option<u64>; 2] {
        match &self.selection {
            VertexSelection::Columns(columns) => [
                (columns.len != 0).then(|| self.vertex.device_source()).flatten(), None,
            ],
            VertexSelection::Indexed(indices) => [
                self.vertex.device_source(),
                (indices.len != 0).then(|| indices.source.device_source()).flatten(),
            ],
        }
    }

    pub(super) fn evaluate(
        &self, writes: &[(usize, usize)], limit: u32, out: &mut BTreeSet<u32>,
        device: Option<&DeviceBytes>, pending: &mut DeviceRanges,
    ) -> bool {
        match &self.selection {
            VertexSelection::Columns(columns) => {
                self.vertex.read(columns.start, columns.len, writes, device, pending, |bytes| {
                    self.accept(columns.start, columns.len, out, |slots| columns.collect(bytes, limit, slots));
                })
            }
            VertexSelection::Indexed(indices) => {
                let mut vertices = BTreeSet::new();
                if !indices.source.read(indices.offset, indices.len, writes, device, pending, |bytes| {
                    for row in bytes.chunks_exact(indices.width) {
                        let index = if indices.width == 2 { u32::from(u16::from_le_bytes(row.try_into().unwrap())) }
                            else { u32::from_le_bytes(row.try_into().unwrap()) };
                        vertices.insert(index.wrapping_add(indices.base_vertex));
                    }
                }) {
                    return false;
                }
                // Reading outside a bound IA index view returns zero before BaseVertex.
                if indices.include_zero { vertices.insert(indices.base_vertex); }
                let words = indexed_vertex_words(self.vertex.size, indices.vertex_stride, vertices, &indices.fields);
                self.collect_indexed(words, &indices.fields, writes, limit, out, device, pending)
            }
        }
    }

    fn collect_indexed(
        &self, words: BTreeSet<(u32, u32, u32)>, fields: &[(u32, u32, u32)],
        writes: &[(usize, usize)], limit: u32, out: &mut BTreeSet<u32>,
        device: Option<&DeviceBytes>, pending: &mut DeviceRanges,
    ) -> bool {
        let span = words.first().zip(words.last()).map(|(&(start, _, _), &(last, _, _))| (start, (last - start) as usize + 4));
        let (start, len) = span.unwrap_or((0, 0));
        // Ordinary coronas use contiguous transient vertices. There is no
        // whole-arena scan even if the view begins well before BaseVertex.
        if len <= INDEXED_VERTEX_READ_BUDGET {
            return self.vertex.read(start, len, writes, device, pending, |bytes| {
                self.accept(start, len, out, |slots| {
                    vertex_defaults(fields, limit, slots);
                    for &(at, bias, mask) in &words {
                        let offset = (at - start) as usize;
                        let word = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
                        insert_slot(word & mask, bias, limit, slots);
                    }
                });
            });
        }
        // Match the existing cap on tiny sparse gathers. Larger disjoint
        // selections stay conservative instead of widening the readback.
        if words.len() > 64 {
            note(Outcome::TooLarge, 0, u32::MAX, self.vertex.address as u64 + u64::from(start), len as u64,
                Some(self.vertex.shadow.0.selector_ownership()));
            return false;
        }
        let mut complete = true;
        let mut selected = BTreeSet::new();
        for &(at, bias, mask) in &words {
            complete &= self.vertex.read(at, 4, writes, device, pending, |bytes| {
                let word = u32::from_le_bytes(bytes.try_into().unwrap());
                insert_slot(word & mask, bias, limit, &mut selected);
            });
        }
        if complete {
            self.accept(start, 4 * words.len(), out, |slots| {
                vertex_defaults(fields, limit, slots);
                slots.extend(selected);
            });
        }
        complete
    }

    fn accept(&self, offset: u32, len: usize, out: &mut BTreeSet<u32>, collect: impl FnOnce(&mut BTreeSet<u32>)) {
        #[cfg(not(feature = "diagnostics"))]
        let _ = (offset, len);
        #[cfg(feature = "diagnostics")]
        if trace_enabled() {
            use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
            static PASSES: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
            let (source, kind) = if len == 0 { ("IA default", 0) }
                else if self.vertex.shadow.0.in_place() { ("mapped guest", 1) }
                else { ("ordered native readback", 2) };
            let passes = PASSES[kind].fetch_add(1, Relaxed) + 1;
            if passes.is_power_of_two() {
                let mut slots = BTreeSet::new();
                collect(&mut slots);
                let first_slots: Vec<_> = slots.iter().copied().take(16).collect();
                tracing::info!(passes, source, shadow=self.vertex.shadow.0.id(),
                    address=format_args!("{:#x}", self.vertex.address + offset as usize),
                    bytes=len, indexed=matches!(&self.selection, VertexSelection::Indexed(_)),
                    selected=slots.len(), ?first_slots, "validated vertex input selector");
                out.extend(slots);
                return;
            }
        }
        collect(out);
    }
}

impl VertexSource {
    fn new(shadow: &Arc<Shadow>, base: u64, size: u32, alignment: u64) -> Option<Self> {
        shadow.offset_in(base)?;
        if !base.is_multiple_of(alignment) || !shadow.gpu(base).is_multiple_of(alignment)
            || !u64::from(size).is_multiple_of(alignment) || u64::from(size) > shadow.left(base)
            || shadow.selector_ownership().2
            || shadow.offset(base).checked_add(u64::from(size))? > shadow.width()
        {
            return None;
        }
        let address = usize::try_from(base).ok()?;
        address.checked_add(size as usize)?;
        Some(Self { shadow: FrozenBuffer(shadow.clone()), address, size })
    }

    fn device_source(&self) -> Option<u64> {
        // Ordinary shared buffers also need a fenced read of their native
        // contents after the occurrence's refresh. The later list reuses it.
        (!self.shadow.0.in_place()).then(|| self.shadow.0.id())
    }

    fn read(
        &self, offset: u32, len: usize, writes: &[(usize, usize)], device: Option<&DeviceBytes>,
        pending: &mut DeviceRanges, use_bytes: impl FnOnce(&[u8]),
    ) -> bool {
        let shadow = &self.shadow.0;
        let at = self.address + offset as usize;
        if len == 0 {
            use_bytes(&[]);
            return true;
        }
        let report = |outcome| {
            note(outcome, 0, u32::MAX, at as u64, len as u64, Some(shadow.selector_ownership()));
            #[cfg(feature = "diagnostics")]
            if trace_enabled() {
                use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
                static SEEN: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
                let with_readback = device.is_some();
                let seen = &SEEN[usize::from(with_readback)];
                let bit = 1 << outcome as u32;
                if seen.load(Relaxed) & bit == 0 && seen.fetch_or(bit, Relaxed) & bit == 0 {
                    tracing::info!(shadow=shadow.id(), address=format_args!("{at:#x}"), bytes=len,
                        ?outcome, with_readback, "vertex input selector could not be validated");
                }
            }
            false
        };
        if u64::from(offset) + len as u64 > u64::from(self.size) {
            return report(Outcome::ChangedBounds);
        }
        let _mapping = if shadow.in_place() {
            let Some(guard) = crate::watch::title_memory_reading_now() else { return report(Outcome::UnverifiedMapping); };
            Some(guard)
        } else { None };
        for _ in 0..3 {
            let generation = crate::resource::alias_generation();
            let ownership = shadow.selector_ownership();
            if ownership.2 { return report(Outcome::OrderedSource); }
            if shadow.writes_pending_in(at, at + len) { return report(Outcome::PendingWriter); }
            let aliases = crate::inplace::aliases_of_bytes(at, len);
            if aliases.iter().any(|&(lo, hi)| crate::host::touches(writes, lo, hi)) {
                return report(Outcome::QueuedWrite);
            }
            if crate::resource::texture_writer_overlaps(&aliases) { return report(Outcome::OverlappingProducer); }

            let mut title;
            let bytes = if ownership.0 {
                if !shadow.shares_guest_pages(at, len) { return report(Outcome::UnverifiedMapping); }
                title = vec![0; len];
                if !crate::host::read_into(at as *const u8, &mut title) { return report(Outcome::Unreadable); }
                title.as_slice()
            } else {
                let key = (shadow.id(), at, len);
                let Some(native) = device.and_then(|device| device.get(&key)).filter(|bytes| bytes.len() == len) else {
                    pending.entry(key).or_insert_with(|| shadow.clone());
                    return report(Outcome::MissingNativeBytes);
                };
                native.as_slice()
            };
            if ownership != shadow.selector_ownership() { return report(Outcome::OrderedSource); }
            if shadow.writes_pending_in(at, at + len) { return report(Outcome::PendingWriter); }
            if generation == crate::resource::alias_generation() {
                use_bytes(bytes);
                return true;
            }
        }
        report(Outcome::ChangedDuringRead)
    }
}

fn index_span(size: u32, width: u32, first: u32, count: u32) -> Option<(u32, u32, usize)> {
    if count == 0 { return Some((0, 0, 0)); }
    first.checked_add(count - 1)?.checked_mul(width)?.checked_add(width - 1)?;
    let offset = (first * width).min(size);
    let available = count.min((size - offset) / width);
    let len = (available * width) as usize;
    (len <= SELECTOR_READ_BUDGET).then_some((offset, available, len))
}

fn indexed_vertex_words(
    size: u32, stride: u32, vertices: BTreeSet<u32>, fields: &[(u32, u32, u32)],
) -> BTreeSet<(u32, u32, u32)> {
    let mut words = BTreeSet::new();
    for vertex in vertices {
        let row = vertex.wrapping_mul(stride);
        for &(field, bias, mask) in fields {
            let at = row.wrapping_add(field);
            if at.checked_add(4).is_some_and(|end| end <= size) {
                words.insert((at, bias, mask));
            }
        }
    }
    words
}

fn vertex_defaults(fields: &[(u32, u32, u32)], limit: u32, out: &mut BTreeSet<u32>) {
    for &(_, bias, _) in fields {
        insert_slot(0, bias, limit, out);
    }
}

#[cfg(feature = "diagnostics")]
fn trace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::host::gpu_progress() || crate::switch::on("VICTORY_TRACE_TEXTURE_OWNER"))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct VertexColumns {
    start: u32,
    len: usize,
    step: u32,
    columns: Vec<(u32, u32, u32, u32)>, // first offset, count, bias, mask
}

impl VertexColumns {
    fn new(size: u32, stride: u32, vertices: Option<(u32, u32)>, mut fields: Vec<(u32, u32, u32)>) -> Option<Self> {
        if stride == 0 || !stride.is_multiple_of(4) || !size.is_multiple_of(4) || fields.is_empty() {
            return None;
        }
        fields.sort_unstable();
        fields.dedup();
        let step = if vertices.is_some() { stride } else { 1 << stride.trailing_zeros() };
        let mut columns = Vec::with_capacity(fields.len());
        let mut start = size;
        let mut end = 0;
        for (field, bias, mask) in fields {
            if !field.is_multiple_of(4) || field.checked_add(4)? > stride { return None; }
            // IA offset math is u32. With arbitrary indices only the residue
            // modulo gcd(stride, 2^32) is invariant through a multiplication
            // wrap. A known direct range can keep the original stride if it
            // proves that its first and last addresses do not wrap.
            let (first, count) = match vertices {
                Some((_, 0)) => (0, 0),
                Some((first, count)) => {
                    let last = first.checked_add(count - 1)?;
                    last.checked_mul(stride)?.checked_add(field)?.checked_add(3)?;
                    (first * stride + field, count)
                }
                None => (field % step, u32::MAX),
            };
            let in_bounds = first.checked_add(4).filter(|&last| last <= size)
                .map_or(0, |last| 1 + (size - last) / step);
            let n = count.min(in_bounds);
            if n != 0 {
                start = start.min(first);
                end = end.max(first + (n - 1) * step + 4);
            }
            columns.push((first, n, bias, mask));
        }
        let len = end.saturating_sub(start) as usize;
        if len > SELECTOR_READ_BUDGET { return None; }
        columns.sort_unstable();
        columns.dedup();
        Some(Self { start: if len == 0 { 0 } else { start }, len, step, columns })
    }

    fn collect(&self, bytes: &[u8], limit: u32, slots: &mut BTreeSet<u32>) {
        for &(first, count, bias, mask) in &self.columns {
            // An out-of-bounds IA fetch returns zero for the present components
            // of the input format. Keep its biased slot even for an empty view.
            insert_slot(0, bias, limit, slots);
            for row in 0..count {
                let at = (first + row * self.step - self.start) as usize;
                let word = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
                insert_slot(word & mask, bias, limit, slots);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_columns_keep_raw_words_from_the_bounded_draw_interval() {
        let fields = vec![(28, 0, 0x07ff_ffff), (44, 0, 0x07ff_ffff)];
        let layout = VertexColumns::new(144, 48, Some((1, 1)), fields.clone()).unwrap();
        assert_eq!((layout.start, layout.len, layout.step), (76, 20, 48));
        let mut bytes = [0u8; 20];
        bytes[..4].copy_from_slice(&0xf800_0005u32.to_le_bytes());
        bytes[16..].copy_from_slice(&0x0000_0059u32.to_le_bytes());
        let mut slots = BTreeSet::new();
        layout.collect(&bytes, 100, &mut slots);
        assert_eq!(slots, BTreeSet::from([0, 5, 89]));
        assert!(VertexColumns::new(144, 48, Some((u32::MAX, 1)), fields.clone()).is_none());
        let outside = VertexColumns::new(144, 48, Some((3, 1)), fields).unwrap();
        slots.clear();
        outside.collect(&[], 100, &mut slots);
        assert_eq!(slots, BTreeSet::from([0]));
    }

    #[test]
    fn vertex_columns_include_aligned_addresses_reachable_after_ia_wrap() {
        let fields = vec![(28, 0, u32::MAX), (44, 32, u32::MAX)];
        let layout = VertexColumns::new(48, 48, None, fields.clone()).unwrap();
        assert_eq!((layout.start, layout.len, layout.step), (12, 36, 16));
        assert_eq!(0x0555_5555u32.wrapping_mul(48).wrapping_add(28), layout.start);
        let mut bytes = [0u8; 36];
        for (offset, word) in [(0, 7u32), (16, 11), (32, 19)] {
            bytes[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
        }
        let mut slots = BTreeSet::new();
        layout.collect(&bytes, 100, &mut slots);
        assert_eq!(slots, BTreeSet::from([0, 1, 7, 8, 11, 12, 19, 20]));
        assert!(VertexColumns::new(262144, 48, None, fields).is_none());
    }

    #[test]
    fn vertex_input_requires_the_native_readback_and_rejects_a_prefix_writer() {
        let title = [0u32; 24];
        let base = title.as_ptr() as usize;
        let Some(shadow) = crate::shadow::by_address(base as u64, 96) else { return; };
        let input = VertexInputWords::new(&shadow, base as u64, 96, 48, Some((1, 1)),
            vec![(28, 0, u32::MAX), (44, 0, u32::MAX)]).unwrap();
        let key = (shadow.id(), base + 76, 20);
        let mut pending = DeviceRanges::new();
        let mut slots = BTreeSet::new();
        assert!(!input.evaluate(&[], 100, &mut slots, None, &mut pending));
        assert!(slots.is_empty() && pending.contains_key(&key));

        let mut native = vec![0u8; 20];
        native[0..4].copy_from_slice(&12u32.to_le_bytes());
        native[16..20].copy_from_slice(&81u32.to_le_bytes());
        let readback = DeviceBytes::from([(key, native)]);
        assert!(input.evaluate(&[], 100, &mut slots, Some(&readback), &mut pending));
        assert_eq!(slots, BTreeSet::from([0, 12, 81]));

        slots.clear();
        assert!(!input.evaluate(&[(base + 92, base + 96)], 100, &mut slots, Some(&readback), &mut pending));
        assert!(slots.is_empty());
    }

    #[test]
    fn indexed_vertex_columns_bound_index_reads_and_keep_wrapped_addresses() {
        assert_eq!(index_span(12, 2, 2, 6), Some((4, 4, 8)));
        assert_eq!(index_span(12, 2, 6, 3), Some((12, 0, 0)));
        assert_eq!(index_span(12, 4, u32::MAX / 4, 2), None);
        let base_vertex = -1i32 as u32;
        let indices = [0u32, 1].into_iter().map(|index| index.wrapping_add(base_vertex)).collect();
        let words = indexed_vertex_words(48, 48, indices, &[(28, 0, u32::MAX), (44, 0, u32::MAX)]);
        assert_eq!(words, BTreeSet::from([(28, 0, u32::MAX), (44, 0, u32::MAX)]));
        let indices = BTreeSet::from([0x0555_5555]);
        assert_eq!(indexed_vertex_words(48, 48, indices, &[(28, 0, u32::MAX)]),
            BTreeSet::from([(12, 0, u32::MAX)]));
    }

    #[test]
    fn indexed_vertex_compact_budget_covers_the_full_original_corona_batch() {
        let vertices = (2000..2000 + 512 * 4).collect();
        let words = indexed_vertex_words(256 * 1024, 48, vertices, &[(28, 0, u32::MAX), (44, 0, u32::MAX)]);
        assert_eq!(words.len(), 512 * 4 * 2);
        let span = (words.last().unwrap().0 - words.first().unwrap().0) as usize + 4;
        assert_eq!(span, 98_276);
        assert!(span > SELECTOR_READ_BUDGET && span <= INDEXED_VERTEX_READ_BUDGET);
        assert_eq!(index_span(6144, 2, 0, 512 * 6), Some((0, 3072, 6144)));
    }

    #[test]
    fn vertex_input_reads_native_quad_indices_before_selected_vertex_words() {
        use windows::Win32::Graphics::Direct3D12::D3D12_RANGE;

        let Some(host) = crate::host::host() else { return; };
        if !crate::shadow::cards() { return; }
        const ARENA: usize = 128 * 1024;
        const STRIDE: u32 = 48;
        const BASE_VERTEX: i32 = 2000;
        const START: usize = BASE_VERTEX as usize * STRIDE as usize;
        const QUAD: [u16; 6] = [0, 1, 2, 0, 2, 3];
        let vertex_title = vec![0u32; ARENA / 4];
        let index_title = [42u16; QUAD.len()];
        let vertex_base = vertex_title.as_ptr() as usize;
        let index_base = index_title.as_ptr() as usize;
        let vertex = crate::shadow::by_address(vertex_base as u64, ARENA as u64).expect("native vertex buffer");
        let index = crate::shadow::by_address(index_base as u64, 12).expect("native index buffer");
        let (vertex_upload, vertex_data) = host.upload_buffer(4 * u64::from(STRIDE)).expect("vertex upload");
        let (index_upload, index_data) = host.upload_buffer(12).expect("index upload");
        let mut quad_words = [0u32; 48];
        for (row, pair) in [[12, 81], [12, 82], [13, 81], [13, 82]].into_iter().enumerate() {
            quad_words[row * 12 + 7] = pair[0];
            quad_words[row * 12 + 11] = pair[1];
        }
        // SAFETY: both upload resources are mapped over these exact lengths.
        unsafe {
            std::ptr::copy_nonoverlapping(quad_words.as_ptr().cast::<u8>(), vertex_data, 192);
            std::ptr::copy_nonoverlapping(QUAD.as_ptr().cast::<u8>(), index_data, 12);
        }
        host.copy_preserved_buffers(&[
            (vertex.buffer.clone(), START as u64, vertex_upload, 0, 192),
            (index.buffer.clone(), 0, index_upload, 0, 12),
        ]).expect("native upload precedes both selector rounds");

        let source = VertexInputWords::new_indexed(
            &vertex, vertex_base as u64, ARENA as u32, STRIDE,
            VertexIndexView { shadow: &index, address: index_base as u64, size: 12,
                format: 57, first: 0, count: 6, base_vertex: BASE_VERTEX },
            vec![(28, 0, u32::MAX), (44, 0, u32::MAX)],
        ).unwrap();
        let probe = |pending: &mut DeviceRanges, observed: &mut DeviceBytes| {
            for (key @ (_, address, len), shadow) in std::mem::take(pending) {
                let readback = host.read_back_buffer(len as u64).expect("selector readback buffer");
                host.copy_preserved_buffers(&[(readback.clone(), 0, shadow.buffer.clone(),
                    shadow.offset(address as u64), len as u64)]).expect("selector readback fence");
                let mut data = std::ptr::null_mut();
                // SAFETY: this readback resource holds len bytes, and the
                // copy above completed on the same native queue.
                let bytes = unsafe {
                    readback.Map(0, Some(&D3D12_RANGE { Begin: 0, End: len }), Some(&mut data)).expect("selector readback map");
                    assert!(!data.is_null());
                    let bytes = std::slice::from_raw_parts(data.cast::<u8>(), len).to_vec();
                    readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
                    bytes
                };
                observed.insert(key, bytes);
            }
        };
        let mut pending = DeviceRanges::new();
        let mut observed = DeviceBytes::new();
        let mut slots = BTreeSet::new();
        assert!(!source.evaluate(&[], 100, &mut slots, Some(&observed), &mut pending));
        assert_eq!(pending.keys().copied().collect::<Vec<_>>(), vec![(index.id(), index_base, 12)]);
        probe(&mut pending, &mut observed);
        assert!(!source.evaluate(&[], 100, &mut slots, Some(&observed), &mut pending));
        assert_eq!(pending.keys().copied().collect::<Vec<_>>(), vec![(vertex.id(), vertex_base + START + 28, 164)]);
        probe(&mut pending, &mut observed);
        assert!(source.evaluate(&[], 100, &mut slots, Some(&observed), &mut pending));
        assert_eq!(slots, BTreeSet::from([0, 12, 13, 81, 82]));
        slots.clear();
        let first_field = vertex_base + START + 28;
        assert!(!source.evaluate(&[(first_field, first_field + 4)], 100, &mut slots, Some(&observed), &mut pending));
        assert!(slots.is_empty() && pending.is_empty());
    }
}
