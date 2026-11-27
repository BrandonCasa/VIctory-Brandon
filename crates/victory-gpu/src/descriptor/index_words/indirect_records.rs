use super::*;
use victory_shader::resource::IndirectRecordColumn;

const ROWS_PER_READ: u32 = 8192;
pub(crate) const INDIRECT_PARENT_BUDGET: usize = 2 * 1024 * 1024 + 4 * 1024;
pub(crate) const INDIRECT_PARENT_RANGES: usize = 1024;
pub(crate) const INDIRECT_CHILD_BUDGET: usize = 128 * 1024;
pub(crate) const INDIRECT_CHILD_RANGES: usize = 8192;

/// The parent columns and a direct field of the same table request identical
/// spans when they belong to this occurrence's same validated native binding.
pub(super) fn dense_record_chunks(size: u64, count: u32, stride: u32) -> impl Iterator<Item = (u32, usize, u32)> {
    let len = size.min(u64::from(count) * u64::from(stride)) & !3;
    let rows = len.div_ceil(u64::from(stride)) as u32;
    (0..rows).step_by(ROWS_PER_READ as usize).map(move |row| {
        let chunk_rows = (rows - row).min(ROWS_PER_READ);
        let offset = row * stride;
        let len = (len - u64::from(offset)).min(u64::from(chunk_rows * stride)) as usize;
        (offset, len, chunk_rows)
    })
}

/// Parent rows and sparse child words belong to one native occurrence. A
/// wide child descriptor supplies its bounds but never triggers a full read.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct IndirectRecordWords {
    parent: IndexWords,
    records: IndexWords,
    guard: Option<IndexWords>,
    columns: Vec<IndirectRecordColumn>,
}

impl IndirectRecordWords {
    pub(crate) fn new(
        parent: IndexWords, records: IndexWords, guard: Option<IndexWords>, mut columns: Vec<IndirectRecordColumn>,
    ) -> Option<Self> {
        let first = *columns.first()?;
        if !(1..=2 * ROWS_PER_READ).contains(&first.count) || !(4..=128).contains(&first.stride)
            || !first.stride.is_multiple_of(4) || columns.len() > 64
            || guard.is_some() != first.guard.is_some()
        { return None; }
        // The reflector can name a component of a wider SMEM read. Keep a
        // full word group inside each present row, and decline a truncated
        // last row whose loader might zero more than one component.
        if parent.size < u64::from(first.count * first.stride) && !parent.size.is_multiple_of(u64::from(first.stride)) {
            return None;
        }
        let fits = |field: u32| field.is_multiple_of(4) && field.checked_add(16).is_some_and(|end| end <= first.stride);
        if let Some(tag) = first.guard && (!fits(tag.parent_field) || !tag.field.is_multiple_of(4)) {
            return None;
        }
        if let Some((tag, guard)) = first.guard.zip(guard.as_ref()) {
            let at = u64::from(tag.field);
            if at < guard.size && at + 16 > guard.size { return None; }
        }
        for column in &columns {
            if column.parent != first.parent || column.records != first.records || column.guard != first.guard
                || (column.count, column.stride) != (first.count, first.stride)
                || !fits(column.pointer_field) || !column.handle_offset.is_multiple_of(4)
            { return None; }
        }
        columns.sort_unstable_by_key(|c| (c.pointer_field, c.handle_offset, c.heap_bytes));
        columns.dedup();
        Some(Self { parent, records, guard, columns })
    }

    pub(crate) fn device_sources(&self) -> [Option<u64>; 3] {
        let device = |source: &IndexWords| source.device_input().map(|owner| owner.id());
        [device(&self.parent), device(&self.records), self.guard.as_ref().and_then(device)]
    }

    pub(crate) fn evaluate(
        &self, writes: &[(usize, usize)], limit: u32, out: &mut BTreeSet<u32>, device: Option<&DeviceBytes>,
        parent_pending: &mut DeviceRanges, child_pending: &mut DeviceRanges,
    ) -> bool {
        // The recorder only needs to know that an ordered probe is needed.
        // It cannot use this occurrence's device data yet, so avoid decoding
        // the same full parent table on every bound dispatch.
        if device.is_none() && self.device_sources().into_iter().any(|source| source.is_some()) {
            return false;
        }
        let topology = crate::resource::alias_generation();
        let mapping = crate::watch::changes();
        let first = self.columns[0];
        let guard = self.guard.as_ref().zip(first.guard).map(|(words, tag)| {
            words.dependent_word(tag.field, writes, device, parent_pending).map(|value| (tag.parent_field, value))
        });
        let guard_ready = guard.is_none_or(|word| word.is_some());
        let guard = guard.flatten();
        let parent_len = self.parent.size.min(u64::from(first.count * first.stride)) & !3;
        let rows = parent_len.div_ceil(u64::from(first.stride)) as u32;
        let mut children = ChildColumns::default();
        let mut complete = guard_ready;
        if rows == 0 {
            complete &= self.parent.validated_bytes((self.parent.base as usize, 0), writes, device, |_| {});
        }
        for (offset, len, chunk_rows) in dense_record_chunks(self.parent.size, first.count, first.stride) {
            if !guard_ready && self.parent.device_input().is_none() { break; }
            complete &= self.parent.dependent_bytes(offset, len, writes, device, parent_pending, |bytes| {
                if guard_ready { children.collect(&self.columns, bytes, chunk_rows, guard); }
            });
        }
        if !complete { return false; }
        // Any wholly out-of-view row has the same zero tag and pointer.
        // A zero pointer still addresses the child field, even when the
        // translated handle expression can also produce a literal zero.
        if rows < first.count && guard.is_none_or(|(_, value)| value == 0) {
            children.collect(&self.columns, &[], 1, guard);
            if !children.unsuitable { children.matching += first.count - rows - 1; }
        }
        let candidates = children.offsets.len();
        let ranges = children.intervals(self.records.size);
        let suitable = !children.unsuitable && ranges <= INDIRECT_CHILD_RANGES;
        #[cfg(feature = "diagnostics")]
        if crate::host::gpu_progress() && device.is_some() {
            use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
            static VISITS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
            let uses = VISITS[usize::from(suitable)].fetch_add(1, Relaxed) + 1;
            if uses.is_power_of_two() {
                tracing::info!(uses, suitable, parent_bytes=parent_len, rows, matching_rows=children.matching,
                    columns=self.columns.len(), candidates, ranges, guarded=guard.is_some(), sources=?self.device_sources(),
                    "bounded indirect-record selector shape");
            }
        }
        #[cfg(not(feature = "diagnostics"))]
        let _ = candidates;
        if !suitable { return false; }
        for column in &self.columns { insert_slot(0, column.heap_bytes, limit, out); }
        let offsets: Vec<_> = children.offsets.into_iter().collect();
        let read_end = offsets.partition_point(|&(at, _)| u64::from(at) + 4 <= self.records.size);
        for &(_, bias) in &offsets[read_end..] { insert_slot(0, bias, limit, out); }
        if read_end != offsets.len() || offsets.is_empty() {
            complete &= self.records.validated_bytes((self.records.base as usize, 0), writes, device, |_| {});
        }
        let mut i = 0;
        while i < read_end {
            let first = i;
            let start = offsets[i].0;
            let mut last = start;
            i += 1;
            while i < read_end && (offsets[i].0 == last || last.checked_add(4) == Some(offsets[i].0)) {
                last = offsets[i].0;
                i += 1;
            }
            let len = (last - start) as usize + 4;
            complete &= self.records.dependent_bytes(start, len, writes, device, child_pending, |bytes| {
                for &(offset, bias) in &offsets[first..i] {
                    insert_slot(word(bytes, offset - start), bias, limit, out);
                }
            });
        }
        complete && topology == crate::resource::alias_generation() && mapping == crate::watch::changes()
    }
}

#[derive(Default)]
struct ChildColumns {
    offsets: BTreeSet<(u32, u32)>,
    matching: u32,
    unsuitable: bool,
}

impl ChildColumns {
    fn intervals(&self, size: u64) -> usize {
        self.offsets.iter().take_while(|&&(at, _)| u64::from(at) + 4 <= size)
            .scan(None, |last, &(at, _)| {
                let starts = last.is_none_or(|old: u32| old != at && old.checked_add(4) != Some(at));
                *last = Some(at);
                Some(usize::from(starts))
            }).sum()
    }

    fn collect(&mut self, columns: &[IndirectRecordColumn], bytes: &[u8], rows: u32, guard: Option<(u32, u32)>) {
        if self.unsuitable { return; }
        for index in 0..rows {
            let row = index * columns[0].stride;
            if guard.is_some_and(|(field, value)| word(bytes, row + field) != value) { continue; }
            self.matching += 1;
            for column in columns {
                let pointer = word(bytes, row + column.pointer_field);
                let offset = pointer.wrapping_add(column.handle_offset);
                if !offset.is_multiple_of(4) || {
                    self.offsets.insert((offset, column.heap_bytes));
                    self.offsets.len() > INDIRECT_CHILD_BUDGET / 4
                } {
                    self.unsuitable = true;
                    return;
                }
            }
        }
    }
}

fn word(bytes: &[u8], at: u32) -> u32 {
    let at = at as usize;
    bytes.get(at..at + 4).map_or(0, |word| u32::from_le_bytes(word.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_shader::resource::RecordTag;

    #[test]
    fn indirect_record_columns_filter_same_row_and_preserve_wrapping_and_oob() {
        let tag = RecordTag { parent_field: 8, table: (0, 416), field: 0x710 };
        let column = IndirectRecordColumn { parent: (0, 288), count: 3, stride: 116, pointer_field: 64,
            records: (0, 648), handle_offset: 24, heap_bytes: 0, guard: Some(tag) };
        let second = IndirectRecordColumn { pointer_field: 96, handle_offset: 4, heap_bytes: 32, ..column };
        let columns = [column, second];
        let mut parent = [0; 2 * 116];
        parent[8..12].copy_from_slice(&7u32.to_le_bytes());
        parent[64..68].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
        parent[96..100].copy_from_slice(&40u32.to_le_bytes());
        parent[116 + 8..116 + 12].copy_from_slice(&9u32.to_le_bytes());
        parent[116 + 64..116 + 68].copy_from_slice(&1u32.to_le_bytes());
        let mut selected = ChildColumns::default();
        selected.collect(&columns, &parent, 3, Some((8, 7)));
        assert_eq!(selected.matching, 1);
        assert_eq!(selected.offsets.iter().copied().collect::<Vec<_>>(), [(8, 0), (44, 32)]);
        let mut missing = ChildColumns::default();
        missing.collect(&columns, &[], 1, Some((8, 0)));
        assert_eq!(missing.offsets.into_iter().collect::<Vec<_>>(), [(4, 32), (24, 0)]);
        let mut unsupported = ChildColumns::default();
        unsupported.collect(&columns, &parent, 2, None);
        assert!(unsupported.unsuitable);
    }

    #[test]
    fn indirect_child_sparse_limit_counts_exact_multicolumn_intervals() {
        let rows = INDIRECT_CHILD_RANGES as u32 / 2 + 1;
        let first = IndirectRecordColumn { parent: (0, 288), count: rows, stride: 116, pointer_field: 64,
            records: (0, 648), handle_offset: 0, heap_bytes: 0, guard: None };
        let second = IndirectRecordColumn { pointer_field: 96, handle_offset: 8, heap_bytes: 32, ..first };
        let adjacent = IndirectRecordColumn { pointer_field: 96, handle_offset: 12, ..first };
        let mut parent = vec![0; (rows * first.stride) as usize];
        for row in 0..rows {
            for field in [64, 96] {
                let at = (row * first.stride + field) as usize;
                parent[at..at + 4].copy_from_slice(&(row * 20).to_le_bytes());
            }
        }
        let mut children = ChildColumns::default();
        children.collect(&[first, second, adjacent], &parent, rows, None);
        assert!(!children.unsuitable);
        assert_eq!(children.offsets.len(), rows as usize * 3);
        let last_row = u64::from((rows - 1) * 20);
        assert_eq!(children.intervals(last_row), INDIRECT_CHILD_RANGES);
        assert_eq!(children.intervals(last_row + 3), INDIRECT_CHILD_RANGES);
        assert_eq!(children.intervals(last_row + 4), INDIRECT_CHILD_RANGES + 1);
        assert_eq!(children.intervals(last_row + 16), INDIRECT_CHILD_RANGES + 2);
    }

    #[test]
    fn indirect_child_gather_leaves_in_prefix_writer_unresolved() {
        use crate::descriptor::{HEAP_IIDS, HeapDesc, Placement, SHADER_VISIBLE, VIEWS_KIND,
            create_cbv, create_heap, heap, release};
        #[repr(C)]
        struct Extended(HeapDesc, Placement);
        #[repr(align(256))]
        struct Data([u32; 64]);
        let Some(_) = crate::host::host() else { return; };
        let parent = Box::new(Data([0; 64]));
        let mut child = Box::new(Data([0; 64]));
        child.0[2] = 37;
        let mut title = std::ptr::null();
        let desc = Extended(HeapDesc { kind: VIEWS_KIND, count: 2, flags: SHADER_VISIBLE, nodes: 0 },
            Placement { address: 0, size: 0 });
        assert_eq!(create_heap(std::ptr::null(), &desc.0, &HEAP_IIDS[0], &mut title), crate::object::S_OK);
        let views = heap(title);
        let parent_address = parent.0.as_ptr() as u64;
        let child_address = child.0.as_ptr() as u64;
        let capture = |slot, address: u64| {
            let handle = views.handle(slot).unwrap();
            create_cbv(std::ptr::null(), &[address as u32, (address >> 32) as u32, 128, 0], handle);
            views.host().unwrap().mirror(slot, handle, true);
            IndexWords::capture(views, handle, None, 0, 0, 0, None, u32::MAX).unwrap()
        };
        let source = IndirectRecordColumn { parent: (0, 288), count: 1, stride: 116, pointer_field: 64,
            records: (0, 648), handle_offset: 8, heap_bytes: 0, guard: None };
        let words = IndirectRecordWords::new(capture(0, parent_address), capture(1, child_address), None,
            vec![source]).unwrap();
        let evaluate = |writes: &[(usize, usize)]| {
            let mut slots = BTreeSet::new();
            let complete = words.evaluate(writes, 64, &mut slots, None, &mut Default::default(), &mut Default::default());
            (complete, slots)
        };
        assert_eq!(evaluate(&[]), (true, [0, 37].into()));
        let at = child_address as usize + 8;
        assert!(!evaluate(&[(at, at + 4)]).0);
        assert_eq!(evaluate(&[(at + 4, at + 8)]), (true, [0, 37].into()));
        release(title);
    }
}
