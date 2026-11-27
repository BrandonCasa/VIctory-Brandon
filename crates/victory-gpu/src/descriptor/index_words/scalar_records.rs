use super::*;
use victory_shader::resource::ScalarRecordChain;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DependentWords {
    Compute(MaterialWords),
    Scalar { spec: ScalarRecordChain, sources: [IndexWords; 3] },
    Fields { pointer: IndexWords, records: IndexWords, offset: u32, fields: Vec<u32> },
    Programs(ScalarPrograms),
}

impl DependentWords {
    pub(crate) fn on_device(&self) -> bool {
        match self {
            Self::Compute(chain) => chain.on_device(),
            Self::Scalar { sources, .. } => sources.iter().any(|s| s.device_input().is_some()),
            Self::Fields { pointer, records, .. } => pointer.device_input().is_some() || records.device_input().is_some(),
            Self::Programs(programs) => programs.device_sources().next().is_some(),
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
