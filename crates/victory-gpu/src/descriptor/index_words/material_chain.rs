use super::*;

/// One selector expression from a compute shader. Both SRVs retain the
/// exact binding that the native command will use.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct MaterialWords {
    groups: IndexWords,
    records: IndexWords,
    count: u32,
    outside: bool,
    indirect: Option<IndirectGroups>,
    parent: u32,
    handle: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct IndirectGroups(FrozenBuffer, usize);

impl IndirectGroups {
    pub(crate) fn new(source: &Arc<crate::shadow::Shadow>, at: u64) -> Option<Self> {
        // This allocation is kept by the argument clamp. Only a card copy
        // is protected against subsequent CPU writes by the upload journal.
        (at % 4 == 0
            && source.carded()
            && !source.selector_ownership().2
            && source.offset_in(at).is_some()
            && source.left(at) >= 4)
            .then(|| Self(FrozenBuffer(source.clone()), at as usize))
    }

    fn upper_bound(
        &self,
        writes: &[(usize, usize)],
        device: Option<&DeviceBytes>,
        pending: &mut DeviceRanges,
    ) -> Option<u32> {
        let (buffer, at) = (&self.0.0, self.1);
        let aliases = crate::inplace::aliases_of_bytes(at, 4);
        if buffer.writes_pending_in(at, at + 4)
            || buffer.selector_ownership().2
            || aliases.iter().any(|&(lo, hi)| crate::host::touches(writes, lo, hi))
            || crate::resource::texture_writer_overlaps(&aliases)
        {
            return None;
        }
        let key = (buffer.id(), at, 4);
        let Some(word) = device.and_then(|d| d.get(&key)) else {
            pending.entry(key).or_insert_with(|| buffer.clone());
            return None;
        };
        Some(u32::from_le_bytes(word.as_slice().try_into().ok()?))
    }
}

impl MaterialWords {
    pub(crate) fn new(
        groups: IndexWords,
        records: IndexWords,
        count: Option<u32>,
        indirect: Option<IndirectGroups>,
        parent: u32,
        handle: u32,
    ) -> Option<Self> {
        // One root-table read, one parent word and one handle per X group.
        // A larger dispatch uses the ordinary full-heap preparation.
        let available = u32::try_from(groups.size / 4).unwrap_or(u32::MAX);
        // An indirect dispatch can reach every workgroup word in the SRV
        // and its defined zero return beyond the SRV.
        let outside = count.is_none_or(|n| n > available);
        let count = count.unwrap_or(available).min(available);
        #[cfg(feature = "diagnostics")]
        {
            use std::sync::atomic::{AtomicU8, Ordering::Relaxed};
            static SEEN: AtomicU8 = AtomicU8::new(0);
            let state = 1 << (usize::from(outside) * 2 + usize::from(count > 2048));
            if SEEN.load(Relaxed) & state == 0 && SEEN.fetch_or(state, Relaxed) & state == 0 {
                tracing::info!(
                    count,
                    outside,
                    group_size = groups.size,
                    group_gpu = groups.device_input().is_some(),
                    record_size = records.size,
                    records_gpu = records.device_input().is_some(),
                    "sampled compute material chain"
                );
            }
        }
        if indirect.is_none() && count > 2048 {
            return None;
        }
        Some(Self {
            groups,
            records,
            count,
            outside,
            indirect,
            parent,
            handle,
        })
    }

    pub(crate) fn on_device(&self) -> bool {
        self.indirect.is_some() || self.groups.device_input().is_some() || self.records.device_input().is_some()
    }

    pub(super) fn device_sources(&self) -> [Option<u64>; 3] {
        [
            self.groups.device_input().map(|s| s.id()),
            self.records.device_input().map(|s| s.id()),
            self.indirect.as_ref().map(|s| s.0.0.id()),
        ]
    }

    /// Missing native spans are gathered one level at a time. A failed
    /// binding or range check leaves the expression unbounded. The caller
    /// only installs a selection once every path completed.
    pub(crate) fn evaluate(
        &self,
        writes: &[(usize, usize)],
        limit: u32,
        out: &mut BTreeSet<u32>,
        device: Option<&DeviceBytes>,
        pending: &mut DeviceRanges,
    ) -> bool {
        let mut groups = BTreeSet::new();
        // The clamp can zero or reduce the X count, and can never increase
        // it. Reading that source word bounds every actual indirect group.
        let (count, outside) = if let Some(arg) = &self.indirect {
            let Some(n) = arg.upper_bound(writes, device, pending) else {
                return false;
            };
            (n.min(self.count), n > self.count)
        } else {
            (self.count, self.outside)
        };
        if count > 2048 {
            return false;
        }
        if count != 0 {
            if !self.groups.dependent_bytes(0, count as usize * 4, writes, device, pending, |bytes| {
                groups.extend(bytes.chunks_exact(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())));
            }) {
                return false;
            }
        }
        if outside {
            groups.insert(0);
        }
        chase(
            groups,
            self.parent,
            self.handle,
            |offset| {
                let mut word = None;
                self.records.dependent_bytes(offset, 4, writes, device, pending,
                    |b| word = Some(u32::from_le_bytes(b.try_into().unwrap())));
                word
            },
            |slot| insert_slot(slot, 0, limit, out),
        )
    }
}

fn chase(
    groups: BTreeSet<u32>,
    parent: u32,
    handle: u32,
    mut read: impl FnMut(u32) -> Option<u32>,
    mut emit: impl FnMut(u32),
) -> bool {
    let mut complete = true;
    let mut fields = BTreeSet::new();
    for group in groups {
        match read(group.wrapping_add(parent)) {
            Some(word) => {
                fields.insert(word.wrapping_add(handle));
            }
            None => complete = false,
        }
    }
    for field in fields {
        match read(field) {
            Some(slot) => emit(slot),
            None => complete = false,
        }
    }
    complete
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_selector_follows_only_selected_records_and_rejects_incomplete_paths() {
        let groups = BTreeSet::from([12, 2048, 4096]);
        let memory =
            std::collections::BTreeMap::from([(16, 200), (2052, 100), (4100, 200), (0x3a0 + 100, 7), (0x3a0 + 200, 9)]);
        let mut reads = Vec::new();
        let mut slots = Vec::new();
        assert!(chase(
            groups.clone(),
            4,
            0x3a0,
            |at| {
                reads.push(at);
                memory.get(&at).copied()
            },
            |slot| slots.push(slot)
        ));
        assert_eq!(reads, [16, 2052, 4100, 0x3a0 + 100, 0x3a0 + 200]);
        assert_eq!(slots, [7, 9]);
        assert!(!chase(groups, 4, 0x39c, |at| memory.get(&at).copied(), |_| panic!()));
    }
}
