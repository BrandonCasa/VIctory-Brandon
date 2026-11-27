use super::*;
use std::collections::BTreeMap;
use victory_shader::resource::ScalarWordOp;

type Table = (u8, u32);
const MAX_OPS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ScalarPrograms {
    sources: BTreeMap<Table, IndexWords>,
    programs: Vec<Vec<ScalarWordOp>>,
}

impl ScalarPrograms {
    pub(crate) fn capture(programs: Vec<Vec<ScalarWordOp>>, mut capture: impl FnMut(Table) -> Option<IndexWords>) -> Option<Self> {
        use ScalarWordOp::*;
        let mut sources = BTreeMap::new();
        for code in &programs {
            if code.is_empty() || code.len() > MAX_OPS { return None; }
            for (at, &op) in code.iter().enumerate() {
                match op {
                    Constant(_) => {}
                    Read { table, address } if usize::from(address) < at => {
                        if let std::collections::btree_map::Entry::Vacant(e) = sources.entry(table) {
                            e.insert(capture(table)?);
                        }
                    }
                    Add(a,b) | Multiply(a,b) | And(a,b) | Or(a,b) | ShiftLeft(a,b) | ShiftRight(a,b) if usize::from(a.max(b)) < at => {}
                    _ => return None,
                }
            }
        }
        Some(Self { sources, programs })
    }

    pub(super) fn device_sources(&self) -> impl Iterator<Item = u64> + '_ {
        self.sources.values().filter_map(|s| s.device_input().map(|b| b.id()))
    }

    pub(super) fn evaluate(&self, writes: &[(usize, usize)], limit: u32, out: &mut BTreeSet<u32>, device: Option<&DeviceBytes>, pending: &mut DeviceRanges) -> bool {
        let mut complete = true;
        let mut words = BTreeMap::new();
        for code in &self.programs {
            let value = evaluate(code, |table, at| {
                *words.entry((table, at)).or_insert_with(|| self.sources.get(&table)?
                    .dependent_word(at, writes, device, pending))
            });
            if let Some(slot) = value {
                insert_slot(slot, 0, limit, out);
            } else {
                complete = false;
            }
        }
        complete
    }
}

/// Keep evaluating independent nodes after a missing device word. This
/// gathers every ready read from the same queue position in one round.
fn evaluate(code: &[ScalarWordOp], mut read: impl FnMut(Table,u32) -> Option<u32>) -> Option<u32> {
    use ScalarWordOp::*;
    let mut values: [Option<u32>; MAX_OPS] = [None; MAX_OPS];
    for (i, &op) in code.iter().enumerate() {
        let value = |a: u8| values[usize::from(a)];
        let pair = |a,b| value(a).zip(value(b));
        values[i] = match op {
            Constant(v) => Some(v),
            Read { table, address } => value(address).and_then(|at| read(table, at)),
            Add(a,b) => pair(a,b).map(|(x,y)| x.wrapping_add(y)),
            Multiply(a,b) => pair(a,b).map(|(x,y)| x.wrapping_mul(y)),
            And(a,b) => pair(a,b).map(|(x,y)| x & y),
            Or(a,b) => pair(a,b).map(|(x,y)| x | y),
            ShiftLeft(a,b) => pair(a,b).map(|(x,y)| x.wrapping_shl(y)),
            ShiftRight(a,b) => pair(a,b).map(|(x,y)| x.wrapping_shr(y)),
        };
    }
    values[code.len() - 1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ScalarWordOp::*;

    #[test]
    fn independent_table_words_are_gathered_in_four_ordered_rounds() {
        let (a,b,c,d,e) = ((1,8), (0,712), (1,0), (0,656), (0,192));
        let code = [
            Constant(20), Read { table: a, address: 0 }, Constant(48), Multiply(1,2),
            Constant(4), Or(3,4), Read { table: b, address: 5 }, Constant(0xffff), And(6,7),
            Constant(8), ShiftLeft(8,9), Constant(72), Read { table: c, address: 11 },
            Constant(2), ShiftLeft(12,13), Add(10,14), Read { table: d, address: 15 },
            Constant(100), Multiply(16,17), Add(18,4), Read { table: e, address: 19 },
        ];
        let native = BTreeMap::from([((a,20),2), ((c,72),3), ((b,100),0xa503_0001),
            ((d,268),5), ((e,504),117)]);
        let mut cache = BTreeMap::new();
        for expected in [vec![(a,20),(c,72)], vec![(b,100)], vec![(d,268)], vec![(e,504)]] {
            let mut missing = Vec::new();
            let ready = evaluate(&code, |table, offset| {
                let at = (table, offset);
                if !cache.contains_key(&at) { missing.push(at); }
                cache.get(&at).copied()
            });
            assert_eq!(ready, None);
            assert_eq!(missing, expected);
            for at in missing { cache.insert(at, native[&at]); }
        }
        assert_eq!(evaluate(&code, |t,o| cache.get(&(t,o)).copied()), Some(117));
        assert!(ScalarPrograms::capture(vec![vec![Add(1,0)]], |_| None).is_none());
    }

    #[test]
    fn packed_record_index_uses_unsigned_shift_and_wrapping_addresses() {
        use ScalarWordOp::*;
        let (a,b,c)=((1,8),(0,712),(0,352));
        let code=[Constant(20),Read{table:a,address:0},Constant(48),Multiply(1,2),Constant(16),Add(3,4),
            Read{table:b,address:5},ShiftRight(6,4),Constant(24),Multiply(7,8),Add(9,0),Read{table:c,address:10}];
        let data=BTreeMap::from([((a,20),u32::MAX),((b,0xffff_ffe0),0xfedc_1234),((c,0xfedc*24+20),123)]);
        assert_eq!(evaluate(&code,|table,at|data.get(&(table,at)).copied()),Some(123));
    }
}
