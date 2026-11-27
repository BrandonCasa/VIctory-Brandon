use super::*;
use victory_shader::resource::{PackedMaterialColumn, ScalarRecordChain};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DependentWords {
    Compute(MaterialWords),
    Scalar { spec: ScalarRecordChain, sources: [IndexWords; 3] },
    Fields { pointer: IndexWords, records: IndexWords, offset: u32, fields: Vec<u32> },
    Programs(ScalarPrograms),
    PackedMaterial(PackedMaterialWords),
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
        }
    }

    pub(crate) fn device_sources(&self) -> impl Iterator<Item = u64> {
        let programs = if let Self::Programs(p) = self { Some(p) } else { None };
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
