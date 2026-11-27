use super::*;
use super::indirect_records::dense_record_chunks;
use victory_shader::resource::{FixedRecordColumn, PackedMaterialColumn, ScalarRangeColumn, ScalarRecordChain};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DependentWords {
    Compute(MaterialWords),
    Scalar { spec: ScalarRecordChain, sources: [IndexWords; 3] },
    Fields { pointer: IndexWords, records: IndexWords, offset: u32, fields: Vec<u32> },
    Programs(ScalarPrograms),
    PackedMaterial(PackedMaterialWords),
    FixedRecord(FixedRecordWords),
    ScalarRange(ScalarRangeWords),
    VertexInput(VertexInputWords),
    IndirectRecords(IndirectRecordWords),
}

/// The device-validated scalar base is extended by the original bounded
/// lane delta. Every field uses the same wrapped set of child record rows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ScalarRangeWords {
    base: ScalarPrograms,
    records: IndexWords,
    count: u32,
    stride: u32,
    fields: Vec<(u32, u32, bool)>,
}

impl ScalarRangeWords {
    pub(crate) fn new(base: ScalarPrograms, records: IndexWords, columns: &[&ScalarRangeColumn]) -> Option<Self> {
        let first = columns.first()?;
        if !(1..=256).contains(&first.count) || !first.record_stride.is_power_of_two()
            || !(4..=64).contains(&first.record_stride)
            || !matches!(first.base.last(), Some(victory_shader::resource::ScalarWordOp::Read { .. }))
        {
            return None;
        }
        let mut fields = Vec::with_capacity(columns.len());
        for column in columns {
            if column.base != first.base || column.records != first.records
                || column.count != first.count || column.record_stride != first.record_stride
                || !column.handle_field.is_multiple_of(4) || column.handle_field.checked_add(4)? > first.record_stride
            {
                return None;
            }
            fields.push((column.handle_field, column.bytes, column.zero_record));
        }
        fields.sort_unstable();
        fields.dedup();
        Some(Self { base, records, count: first.count, stride: first.record_stride, fields })
    }

    fn evaluate(
        &self, writes: &[(usize, usize)], limit: u32, out: &mut BTreeSet<u32>,
        device: Option<&DeviceBytes>, pending: &mut DeviceRanges,
    ) -> bool {
        let mut base = None;
        if !self.base.evaluate_values(writes, device, pending, |value| base = Some(value)) { return false; }
        let Some(base) = base else { return false; };
        let rows = scalar_range_rows(base, self.count, self.stride);
        let mut complete = true;
        let mut has_interval = false;
        let first = self.fields[0].0;
        let last = self.fields.last().expect("scalar range fields").0;
        for &(_, bias, _) in &self.fields { insert_slot(0, bias, limit, out); }
        for (start, count) in rows {
            if count == 0 { continue; }
            let begin = start + first;
            let end = u64::from(start) + u64::from((count - 1) * self.stride + last + 4);
            let len = end.min(self.records.size & !3).saturating_sub(u64::from(begin)) as usize;
            if len == 0 { continue; }
            has_interval = true;
            complete &= self.records.dependent_bytes(begin, len, writes, device, pending, |bytes| {
                for row in 0..count {
                    for &(field, bias, _) in &self.fields {
                        let at = (row * self.stride + field - first) as usize;
                        if let Some(word) = bytes.get(at..at + 4) {
                            insert_slot(u32::from_le_bytes(word.try_into().expect("four-byte field")), bias, limit, out);
                        }
                    }
                }
            });
        }
        if rows[0].0 != 0 && rows[1].1 == 0 {
            for &(field, bias, zero_record) in &self.fields {
                if !zero_record { continue; }
                if let Some(slot) = self.records.dependent_word(field, writes, device, pending) {
                    insert_slot(slot, bias, limit, out);
                } else { complete = false; }
            }
        }
        // A wholly out-of-bounds range supplies zero only for the same
        // view, so still verify the recorded native descriptor identity.
        complete && (has_interval || self.records.validated_bytes((self.records.base as usize, 0), writes, device, |_| {}))
    }
}

fn scalar_range_rows(base: u32, count: u32, stride: u32) -> [(u32, u32); 2] {
    let start = base.wrapping_mul(stride);
    let before_wrap = (1u64 << 32) - u64::from(start);
    let first = count.min((before_wrap / u64::from(stride)) as u32);
    [(start, first), (0, count - first)]
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FixedRecordWords {
    source: IndexWords,
    columns: Vec<FixedRecordColumn>,
    dense: bool,
}

impl FixedRecordWords {
    pub(crate) fn new(source: IndexWords, mut columns: Vec<FixedRecordColumn>) -> Option<Self> {
        let first = *columns.first()?;
        let dense = is_resolve_handle_column(first);
        if !dense && (!(1..=512).contains(&first.count) || !first.stride.is_multiple_of(4)
            || first.count.checked_mul(first.stride)? > 2 * SELECTOR_READ_BUDGET as u32)
        {
            return None;
        }
        for column in &columns {
            if column.table != first.table || column.count != first.count || column.stride != first.stride
                || !column.field.is_multiple_of(4) || column.field.checked_add(4)? > first.stride
                || (dense && !is_resolve_handle_column(*column))
            {
                return None;
            }
        }
        columns.sort_unstable_by_key(|column| (column.field, column.bytes, column.value_mask));
        columns.dedup();
        if !dense && columns.len().checked_mul(first.count as usize)? > 512 { return None; }
        Some(Self { source, columns, dense })
    }

    fn evaluate(
        &self, writes: &[(usize, usize)], limit: u32, out: &mut BTreeSet<u32>,
        device: Option<&DeviceBytes>, pending: &mut DeviceRanges,
    ) -> bool {
        let first = self.columns[0];
        if self.dense {
            if device.is_none() && self.source.device_input().is_some() { return false; }
            let topology = crate::resource::alias_generation();
            let mapping = crate::watch::changes();
            let mut chunks = dense_record_chunks(self.source.size, first.count, first.stride).peekable();
            if chunks.peek().is_none() {
                return self.source.validated_bytes((self.source.base as usize, 0), writes, device,
                    |bytes| decode_fixed_record_rows(&self.columns, bytes, 0, 0, limit, out));
            }
            let mut complete = true;
            for (offset, len, rows) in chunks {
                complete &= self.source.dependent_bytes(offset, len, writes, device, pending,
                    |bytes| decode_fixed_record_rows(&self.columns, bytes, 0, rows, limit, out));
            }
            return complete && topology == crate::resource::alias_generation() && mapping == crate::watch::changes();
        }
        let last = self.columns.last().expect("fixed record columns");
        let end = (first.count - 1) * first.stride + last.field + 4;
        let end = u64::from(end).min(self.source.size & !3);
        let len = end.saturating_sub(u64::from(first.field)) as usize;
        if len == 0 {
            return self.source.validated_bytes((self.source.base as usize, 0), writes, device,
                |b| decode_fixed_records(&self.columns, b, first.field, limit, out));
        }
        if self.source.device_input().is_none() || len <= SELECTOR_READ_BUDGET {
            return self.source.dependent_bytes(first.field, len, writes, device, pending,
                |b| decode_fixed_records(&self.columns, b, first.field, limit, out));
        }
        // A mostly empty span can still be bounded by exact device words.
        // Missing words are requested together through the ordinary plan.
        let mut complete = true;
        for column in &self.columns {
            insert_slot(0, column.bytes, limit, out);
            for row in 0..column.count {
                if let Some(slot) = self.source.dependent_word(row * column.stride + column.field, writes, device, pending) {
                    insert_slot(slot & column.value_mask, column.bytes, limit, out);
                } else {
                    complete = false;
                }
            }
        }
        complete
    }
}

pub(crate) fn is_resolve_handle_column(column: FixedRecordColumn) -> bool {
    column.table == (0, 288) && matches!(column.field, 52 | 60 | 108) && column.stride == 116 && column.count == 16384
        && column.bytes == 0 && column.value_mask == u32::MAX
}

fn decode_fixed_records(columns: &[FixedRecordColumn], bytes: &[u8], start: u32, limit: u32, out: &mut BTreeSet<u32>) {
    decode_fixed_record_rows(columns, bytes, start, columns[0].count, limit, out);
}

fn decode_fixed_record_rows(columns: &[FixedRecordColumn], bytes: &[u8], start: u32, rows: u32, limit: u32, out: &mut BTreeSet<u32>) {
    for column in columns {
        insert_slot(0, column.bytes, limit, out);
        for row in 0..rows {
            let offset = (row * column.stride + column.field - start) as usize;
            if let Some(word) = bytes.get(offset..offset + 4) {
                let slot = u32::from_le_bytes(word.try_into().expect("four-byte field"));
                insert_slot(slot & column.value_mask, column.bytes, limit, out);
            }
        }
    }
}

/// Gather the slots from a bounded first table of packed record indices.
/// All columns in a group share this exact parent prefix and record view.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PackedMaterialWords {
    parent: IndexWords,
    records: IndexWords,
    columns: Vec<PackedMaterialColumn>,
}

impl PackedMaterialWords {
    pub(crate) fn new(parent: IndexWords, records: IndexWords, mut columns: Vec<PackedMaterialColumn>) -> Option<Self> {
        let first = columns.first()?;
        let parent_bytes = first.parent_count.checked_mul(first.parent_stride)?;
        if first.parent_count == 0 || first.parent_count > 256 || parent_bytes > SELECTOR_READ_BUDGET as u32
            || !first.parent_stride.is_multiple_of(4)
        {
            return None;
        }
        for column in &columns {
            if column.parent != first.parent || column.records != first.records
                || column.parent_count != first.parent_count || column.parent_stride != first.parent_stride
                || !column.packed_field.is_multiple_of(4) || column.packed_field.checked_add(4)? > first.parent_stride
                || !matches!(column.shift, 0 | 16) || column.index_mask != 0xffff
                || !column.record_stride.is_multiple_of(4) || !column.handle_field.is_multiple_of(4)
                || column.handle_field.checked_add(4)? > column.record_stride
            {
                return None;
            }
            column.index_mask.checked_mul(column.record_stride)?.checked_add(column.handle_field)?.checked_add(3)?;
        }
        columns.sort_unstable_by_key(|c| (c.packed_field, c.shift, c.record_stride, c.handle_field, c.bytes));
        columns.dedup();
        Some(Self { parent, records, columns })
    }

    fn evaluate(
        &self, writes: &[(usize, usize)], limit: u32, out: &mut BTreeSet<u32>,
        device: Option<&DeviceBytes>, pending: &mut DeviceRanges,
    ) -> bool {
        let first = self.columns[0];
        let bytes = self.parent.size.min(u64::from(first.parent_count * first.parent_stride)) as usize & !3;
        let mut indices = BTreeSet::new();
        let complete = if bytes == 0 {
            self.parent.validated_bytes((self.parent.base as usize, 0), writes, device,
                |b| packed_material_indices(&self.columns, b, &mut indices))
        } else {
            self.parent.dependent_bytes(0, bytes, writes, device, pending,
                |b| packed_material_indices(&self.columns, b, &mut indices))
        };
        if !complete { return false; }
        // A zero handle leaf and an out-of-view record can both name the
        // same zero-relative slot. Preserve it even if the gathered fields
        // happen to contain only nonzero handles.
        for column in &self.columns {
            insert_slot(0, column.bytes, limit, out);
        }
        let mut complete = true;
        for (offset, bias) in indices {
            // Missing words are added to the ordered readback together.
            if let Some(slot) = self.records.dependent_word(offset, writes, device, pending) {
                insert_slot(slot, bias, limit, out);
            } else {
                complete = false;
            }
        }
        complete
    }
}

fn packed_material_indices(columns: &[PackedMaterialColumn], parent: &[u8], out: &mut BTreeSet<(u32, u32)>) {
    for column in columns {
        for row in 0..column.parent_count {
            let offset = (row * column.parent_stride + column.packed_field) as usize;
            let packed = parent.get(offset..offset + 4)
                .map_or(0, |word| u32::from_le_bytes(word.try_into().unwrap()));
            let index = (packed >> column.shift) & column.index_mask;
            out.insert((index * column.record_stride + column.handle_field, column.bytes));
        }
    }
}

impl DependentWords {
    pub(crate) fn on_device(&self) -> bool {
        match self {
            Self::Compute(chain) => chain.on_device(),
            Self::Scalar { sources, .. } => sources.iter().any(|s| s.device_input().is_some()),
            Self::Fields { pointer, records, .. } => pointer.device_input().is_some() || records.device_input().is_some(),
            Self::Programs(programs) => programs.device_sources().next().is_some(),
            Self::PackedMaterial(words) => words.parent.device_input().is_some() || words.records.device_input().is_some(),
            Self::FixedRecord(words) => words.source.device_input().is_some(),
            Self::ScalarRange(words) => words.records.device_input().is_some() || words.base.device_sources().next().is_some(),
            Self::VertexInput(words) => words.device_sources().into_iter().any(|id| id.is_some()),
            Self::IndirectRecords(words) => words.device_sources().into_iter().any(|id| id.is_some()),
        }
    }

    pub(crate) fn device_sources(&self) -> impl Iterator<Item = u64> {
        let programs = match self {
            Self::Programs(programs) => Some(programs),
            Self::ScalarRange(words) => Some(&words.base),
            _ => None,
        };
        match self {
            Self::Compute(chain) => chain.device_sources(),
            Self::Scalar { sources, .. } => sources.each_ref().map(|s| s.device_input().map(|b| b.id())),
            Self::Fields { pointer, records, .. } => [
                pointer.device_input().map(|b| b.id()),
                records.device_input().map(|b| b.id()), None,
            ],
            Self::Programs(_) => [None; 3],
            Self::PackedMaterial(words) => [
                words.parent.device_input().map(|b| b.id()),
                words.records.device_input().map(|b| b.id()), None,
            ],
            Self::FixedRecord(words) => [words.source.device_input().map(|b| b.id()), None, None],
            Self::ScalarRange(words) => [words.records.device_input().map(|b| b.id()), None, None],
            Self::VertexInput(words) => {
                let [vertex, index] = words.device_sources();
                [vertex, index, None]
            }
            Self::IndirectRecords(words) => words.device_sources(),
        }
        .into_iter().flatten().chain(programs.into_iter().flat_map(ScalarPrograms::device_sources))
    }

    pub(crate) fn evaluate(
        &self,
        writes: &[(usize, usize)],
        limit: u32,
        out: &mut BTreeSet<u32>,
        device: Option<&DeviceBytes>,
        pending: &mut DeviceRanges,
    ) -> bool {
        match self {
            Self::Compute(chain) => chain.evaluate(writes, limit, out, device, pending),
            Self::Programs(programs) => programs.evaluate(writes, limit, out, device, pending),
            Self::PackedMaterial(words) => words.evaluate(writes, limit, out, device, pending),
            Self::FixedRecord(words) => words.evaluate(writes, limit, out, device, pending),
            Self::ScalarRange(words) => words.evaluate(writes, limit, out, device, pending),
            Self::VertexInput(words) => words.evaluate(writes, limit, out, device, pending),
            Self::IndirectRecords(words) => {
                let mut parents = DeviceRanges::new();
                let complete = words.evaluate(writes, limit, out, device, &mut parents, pending);
                pending.extend(parents);
                complete
            }
            Self::Scalar { spec, sources } => {
                scalar_handles(spec, |table, offset| sources[table].dependent_word(offset, writes, device, pending),
                    |slot| insert_slot(slot, 0, limit, out))
            }
            Self::Fields { pointer, records, offset, fields } => {
                let Some(base) = pointer.dependent_word(*offset, writes, device, pending) else { return false };
                read_fields(base, fields.iter().copied(), |word| records.dependent_word(word, writes, device, pending),
                    |slot| insert_slot(slot, 0, limit, out))
            }
        }
    }

    pub(crate) fn plan_ranges(
        &self, writes: &[(usize, usize)], device: &DeviceBytes, ordinary: &mut DeviceRanges,
        record_parents: &mut DeviceRanges, record_children: &mut DeviceRanges,
    ) {
        let mut unused = BTreeSet::new();
        match self {
            Self::IndirectRecords(words) => {
                words.evaluate(writes, 0, &mut unused, Some(device), record_parents, record_children);
            }
            Self::FixedRecord(words) if words.dense => {
                words.evaluate(writes, 0, &mut unused, Some(device), record_parents);
            }
            _ => { self.evaluate(writes, 0, &mut unused, Some(device), ordinary); }
        }
    }
}

fn scalar_handles(
    spec: &ScalarRecordChain,
    mut read: impl FnMut(usize, u32) -> Option<u32>,
    emit: impl FnMut(u32),
) -> bool {
    let Some(instance) = read(0, spec.index_offset) else { return false };
    let packed = instance.wrapping_mul(spec.instance_stride).wrapping_add(spec.packed_offset);
    let Some(packed) = read(1, packed) else { return false };
    let record = (packed >> (spec.record_shift & 31)).wrapping_mul(spec.record_stride);
    let Some(count) = read(2, record.wrapping_add(spec.count_offset)) else { return false };
    let count = (count as i32).max(0) as u32;
    if count > 2048 { return false; }
    let first = record.wrapping_add(spec.handle_offset);
    read_fields(first, (0..count).map(|i| i.wrapping_mul(spec.handle_stride)), |at| read(2, at), emit)
}

fn read_fields(base: u32, fields: impl Iterator<Item = u32>, mut read: impl FnMut(u32) -> Option<u32>, mut emit: impl FnMut(u32)) -> bool {
    let mut complete = true;
    for field in fields {
        // Queue all missing fields together, even when the first is pending.
        if let Some(slot) = read(base.wrapping_add(field)) {
            emit(slot);
        } else {
            complete = false;
        }
    }
    complete
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_based_record_ranges_preserve_wrapping_byte_addresses() {
        let range = |base, count| scalar_range_rows(base, count, 64);
        assert_eq!(range(31, 256), [(31 * 64, 256), (0, 0)]);
        assert_eq!(range(u32::MAX - 1, 4), [(0xffff_ff80, 2), (0, 2)]);
        for base in [0, 1, 0x3ff_fffe, 0x3ff_ffff, 0x400_0000, u32::MAX] {
            let clipped = range(base, 256).into_iter().flat_map(|(start, rows)| {
                (0..rows).map(move |row| start + row * 64 + 44)
            });
            let actual = (0..256).map(|row| base.wrapping_add(row).wrapping_mul(64).wrapping_add(44));
            assert!(clipped.eq(actual));
        }
    }

    #[test]
    fn fixed_record_proof_reads_field_four_at_non_power_of_two_stride() {
        let column = FixedRecordColumn {
            table: (0, 0), field: 4, stride: 136, count: 512, bytes: 0, value_mask: u32::MAX,
        };
        let mut bytes = [0; 140];
        bytes[..4].copy_from_slice(&91u32.to_le_bytes());
        bytes[13..17].copy_from_slice(&60u32.to_le_bytes());
        bytes[136..140].copy_from_slice(&37u32.to_le_bytes());
        let mut slots = BTreeSet::new();
        decode_fixed_records(&[column], &bytes, 4, 1024, &mut slots);
        assert_eq!(slots.into_iter().collect::<Vec<_>>(), [0, 37, 91]);
    }

    #[test]
    fn resolve_fixed_handle_column_shares_the_exact_dense_parent_chunks() {
        let column = FixedRecordColumn { table: (0, 288), field: 60, stride: 116, count: 16384,
            bytes: 0, value_mask: u32::MAX };
        assert!(is_resolve_handle_column(column));
        assert!(!is_resolve_handle_column(FixedRecordColumn { bytes: 32, ..column }));
        assert!(!is_resolve_handle_column(FixedRecordColumn { value_mask: 0xffff, ..column }));
        let chunks = |size| dense_record_chunks(size, column.count, column.stride).collect::<Vec<_>>();
        assert_eq!(chunks(1_900_544), [(0, 950_272, 8192), (950_272, 950_272, 8192)]);
        assert_eq!(chunks(950_272 + 72), [(0, 950_272, 8192), (950_272, 72, 1)]);
        assert_eq!(chunks(950_272 + 63), [(0, 950_272, 8192), (950_272, 60, 1)]);

        let mut second = [0; 72];
        second[60..64].copy_from_slice(&0x8000_0025u32.to_le_bytes());
        second[64..68].copy_from_slice(&9u32.to_le_bytes());
        let select = |bytes: &[u8]| {
            let mut slots = BTreeSet::new();
            decode_fixed_record_rows(&[column], bytes, 0, 1, 64, &mut slots);
            slots
        };
        assert_eq!(select(&second), BTreeSet::from([0, 37]));
        assert_eq!(select(&second[..63]), BTreeSet::from([0]));
    }

    #[test]
    fn packed_material_columns_gather_unique_records_and_include_out_of_bounds_zero() {
        let column = PackedMaterialColumn {
            parent: (1, 24), records: (0, 368), parent_count: 3, parent_stride: 16,
            packed_field: 0, shift: 0, index_mask: 0xffff, record_stride: 64,
            handle_field: 36, bytes: 0,
        };
        let columns = [column, PackedMaterialColumn { packed_field: 4, shift: 16, handle_field: 40, ..column }];
        let mut parent = [0; 20];
        parent[0..4].copy_from_slice(&0xffff_0002u32.to_le_bytes());
        parent[4..8].copy_from_slice(&0x0003_ffffu32.to_le_bytes());
        parent[16..20].copy_from_slice(&0x0009_0002u32.to_le_bytes());
        let mut offsets = BTreeSet::new();
        packed_material_indices(&columns, &parent, &mut offsets);
        assert_eq!(offsets.into_iter().collect::<Vec<_>>(), [(36, 0), (40, 0), (164, 0), (232, 0)]);
    }

    fn chain() -> ScalarRecordChain {
        ScalarRecordChain {
            index: (1, 8), instances: (0, 712), records: (0, 480),
            index_offset: 20, instance_stride: 48, packed_offset: 24,
            record_shift: 16, record_stride: 912, count_offset: 136,
            handle_offset: 148, handle_stride: 16,
        }
    }

    #[test]
    fn scalar_record_counts_bound_only_the_selected_handle_column() {
        let c = chain();
        // Multiplication wraps to zero at the first instance. The unused
        // low half of the instance word cannot change the selected record.
        let table = |count| std::collections::BTreeMap::from([
            ((0,20), 0x1000_0000), ((1,24), (1 << 16) | 0xa5a5), ((2,912 + 136),count),
            ((2,912 + 148),39), ((2,912 + 152),190), ((2,912 + 164),61),
        ]);
        for (count, expected, complete) in [
            (2, vec![39,61], true), (3, vec![39,61], false),
            (0, vec![], true), (u32::MAX, vec![], true), (2049, vec![], false),
        ] {
            let values = table(count);
            let mut slots = vec![];
            assert_eq!(scalar_handles(&c, |t, offset| values.get(&(t,offset)).copied(), |slot| slots.push(slot)), complete);
            assert_eq!(slots, expected);
        }
    }

    #[test]
    fn zero_from_an_out_of_bounds_instance_is_still_a_record_index() {
        // Neither zero intermediate is a terminator. Record zero can
        // have a positive count and contain an in-bounds sampled handle.
        let mut visits = Vec::new();
        let mut handles = Vec::new();
        assert!(scalar_handles(&chain(), |table, offset| {
            visits.push((table, offset));
            Some(match (table, offset) {
                (2,136) => 1, (2,148) => 23, _ => 0,
            })
        }, |handle| handles.push(handle)));
        assert_eq!(visits, [(0,20), (1,24), (2,136), (2,148)]);
        assert_eq!(handles, [23]);
    }

    #[test]
    fn pointer_fields_gather_only_proven_offsets_even_after_address_wraps() {
        let mut requested = Vec::new();
        let mut handles = Vec::new();
        let fields = [0, 4, 1936, 1940];
        let complete = read_fields(u32::MAX - 3, fields.into_iter(), |at| {
            requested.push(at);
            match at {
                0 => None, 1932 => Some(73), _ => Some(0),
            }
        }, |slot| handles.push(slot));
        assert_eq!(requested, [u32::MAX - 3, 0, 1932, 1936]);
        assert_eq!(handles, [0, 73, 0]);
        assert!(!complete);
    }
}
