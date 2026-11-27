use super::*;
use crate::gfx10::Valu;
use crate::io::{Io, Target};
use crate::native::{NativeProgram, PsInput, Usage};
use std::collections::{BTreeMap, BTreeSet};

type Limit = (u8, u8, u32);
type Table = (u8, u32);
// A scalar pointer plus an immediate, or just an immediate. Keep every
// possible value. If a product/union is too large, retain opaque access.
type AddressWord = (Option<(Table, u32)>, u32);
pub(super) enum PixelFields {
    Pointers(Vec<PointerField>, Vec<Limit>),
    Words(Vec<BufferIndex>),
    Ranges(Vec<ScalarRangeColumn>),
}
const MAX_FIELDS: usize = 128;
// Enumerate a small flat-input domain only when the *bound producer*
// proves the same upper limit at pipeline bind time.
const FLAT_LIMIT: u32 = 7;

fn valu(inst: &Inst) -> Option<&Valu> {
    let Fields::Valu(op) = &inst.fields else {
        return None;
    };
    (op.sdwa.is_none()
        && op.dpp.is_none()
        && op.abs == 0
        && op.neg == 0
        && op.opsel == 0
        && op.omod == 0
        && !op.clamp)
        .then_some(op)
}

fn unsigned_lane_compare(inst: &Inst) -> Option<([Value; 2], u8)> {
    let op = valu(inst)?;
    let [Operand::Scalar { code, dwords: 1 }, Operand::Vector { reg, dwords: 1 }] = op.src.as_slice() else {
        return None;
    };
    let Operand::Scalar { code: dest, .. } = op.dst? else { return None; };
    Some(([inst.read(Reg::S(*code))?, inst.read(Reg::V(*reg))?], dest))
}

impl Tracer<'_> {
    // Enumerate register writes feasible for a lane. An implicitly masked
    // write may preserve the old register even without a branch in the CFG.
    fn possible_writes(
        &self,
        v: Value,
        known: &Known,
        fuel: &mut usize,
        out: &mut Vec<(Value, Known)>,
    ) -> Option<()> {
        *fuel = fuel.checked_sub(1)?;
        let lanes = self.lanes?;
        if let Some((b, i)) = self.phis[v.0 as usize] {
            let block = &self.p.blocks[b];
            for (j, &p) in block.preds.iter().enumerate() {
                let mut edge = known.clone();
                edge.push((lanes.reach[p], true));
                if let LaneExit::Branch {
                    when,
                    taken,
                    not_taken,
                } = lanes.exits[p]
                    && taken != not_taken
                    && lanes.edge_fixes_lane_bit(p, taken == b)
                {
                    edge.push((when, taken == b));
                }
                if self.meet(lanes, &edge) {
                    if self.rpo[p] >= self.rpo[b] {
                        return None;
                    }
                    self.possible_writes(block.phis[i].args[j], &edge, fuel, out)?;
                }
            }
            return Some(());
        }
        let mut here = known.clone();
        if let Some(site) = self.defs[v.0 as usize] {
            let inst = self.inst(site);
            let (reg, _) = inst.writes.iter().find(|(_, w)| *w == v)?;
            if matches!(reg, Reg::V(_))
                && let Some(guard) = lanes.guards[site.0][site.1]
            {
                if self.runs(site, known).is_err() {
                    let mut skip = known.clone();
                    skip.push((guard, false));
                    self.possible_writes(inst.read(*reg)?, &skip, fuel, out)?;
                }
                here.push((guard, true));
            }
        }
        if self.meet(lanes, &here) {
            out.push((v, here));
        }
        Some(())
    }

    fn output_max(&self, v: Value, known: &Known, io: &Io, fuel: &mut usize) -> Option<u32> {
        let mut writes = Vec::new();
        self.possible_writes(v, known, fuel, &mut writes)?;
        let mut bound = 0;
        for (v, k) in writes {
            let Some(inst) = self.def(v) else {
                // Host lowering supplies zero for uninitialized VGPR inputs.
                if !matches!(self.inputs[v.0 as usize], Some(Reg::V(_))) || io.inputs.iter().any(|(s, _)| *s == v)
                {
                    return None;
                }
                continue;
            };
            let op = valu(inst)?;
            let next = match (inst.name(), op.src.as_slice()) {
                ("v_mov_b32", [x]) => match self.raw(inst, x)? {
                    Term::Const(k) => k,
                    Term::Value(u) => self.output_max(u, &k, io, fuel)?,
                },
                ("v_min_u32" | "v_and_b32", [a, b]) => [a, b]
                    .into_iter()
                    .filter_map(|x| match self.raw(inst, x)? {
                        Term::Const(c) => Some(c),
                        _ => None,
                    })
                    .min()?,
                _ => return None,
            };
            bound = bound.max(next);
        }
        Some(bound)
    }

    /// Vertex preparation may compare the same count and lane number that
    /// the final export tests again after restoring EXEC. Both native
    /// compares evaluate the same predicate. A lane exporting under the
    /// later compare must have been active after the earlier compare when
    /// the earlier compare began with all lanes active.
    fn mesh_export_facts(&self, site: Site, known: &mut Known) {
        let Some(lanes) = self.lanes else { return; };
        if self.p.blocks.iter().enumerate().any(|(block, code)| {
            self.rpo[block] != usize::MAX
                && code.exit.successors().iter().any(|&next| self.rpo[next] <= self.rpo[block])
        }) { return; }

        let dom = self.dominators.get_or_init(|| {
            crate::ir::idoms(&self.p.blocks, &crate::ir::reverse_postorder(&self.p.blocks), &self.rpo)
        });
        let mask = |inst: &Inst, reg, read| {
            let get = |r| if read { inst.read(Reg::S(r)) } else { inst.write(Reg::S(r)) };
            let high = if self.p.wave64 { Some(get(reg + 1)?) } else { None };
            lanes.mask_bits(get(reg)?, high)
        };
        let mut compared = Vec::new();
        let (mut block, mut end) = site;
        loop {
            let instructions = &self.p.blocks[block].insts;
            for inst in instructions[..end].iter().rev() {
                match inst.name() {
                    "v_cmp_gt_u32" => {
                        if let Some((inputs, dest)) = unsigned_lane_compare(inst)
                            && let Some(after) = mask(inst, dest, false)
                        {
                            let mut excluded = known.clone();
                            excluded.push((after, false));
                            if !self.meet(lanes, &excluded) { compared.push(inputs); }
                        }
                    }
                    "v_cmpx_gt_u32" => {
                        if let Some((inputs, EXEC)) = unsigned_lane_compare(inst)
                            && compared.contains(&inputs)
                            && mask(inst, EXEC, true) == Some(Bool::TRUE)
                            && let Some(after) = mask(inst, EXEC, false)
                        {
                            known.push((after, true));
                        }
                    }
                    _ => {}
                }
            }
            let parent = dom[block];
            if parent == block || parent == usize::MAX { break; }
            block = parent;
            end = self.p.blocks[block].insts.len();
        }
    }

    pub(super) fn pixel_fields(
        &self,
        site: Site,
        r: &Resources,
        inputs: &[PsInput],
        used: &[crate::UsedResource],
    ) -> Option<PixelFields> {
        let offset = self.first_lane_offset(site)?;
        let v = self.first_lane_source(offset)?;
        let reader = self.defs[offset.0 as usize]?;
        let lanes = self.lanes?;
        // Only lanes remaining after the equality compare may sample. Check
        // each image use is dominated by the exact cmpx and still in its
        // EXEC. This includes nested sampler/texture waterfalls.
        let block = &self.p.blocks[site.0];
        if reader.0 != site.0 || reader.1 + 2 > site.1 {
            return None;
        }
        let cmp = &block.insts[reader.1 + 1];
        if cmp.name() != "v_cmpx_eq_u32" {
            return None;
        }
        let op = valu(cmp)?;
        let [a, b] = op.src.as_slice() else {
            return None;
        };
        let pair = [self.raw(cmp, a)?, self.raw(cmp, b)?];
        if !pair.contains(&Term::Value(v)) || !pair.contains(&Term::Value(offset)) {
            return None;
        }
        let exec = cmp.write(Reg::S(EXEC))?;
        let high = cmp.write(Reg::S(EXEC + 1));
        let mut found = false;
        for use_ in &r.descriptors {
            if use_.role != Role::Resource
                || !use_
                    .source
                    .leaves()
                    .into_iter()
                    .any(|s| matches!(s,Source::Loaded{load,..} if *load==site))
            {
                continue;
            }
            found = true;
            let inst = self.inst(use_.site);
            if !inst.name().starts_with("image_")
                || !self.mask_subset(inst.read(Reg::S(EXEC))?, exec)
                || (self.p.wave64 && !self.mask_subset(inst.read(Reg::S(EXEC + 1))?, high?))
            {
                return None;
            }
        }
        if !found {
            return None;
        }
        let mut known = vec![(lanes.reach[site.0], true)];
        if let Some(active) = lanes.mask_bits(exec, high) {
            known.push((active, true));
        }
        let mut fields = BTreeSet::new();
        let mut words = Vec::new();
        let mut ranges = Some(Vec::new());
        let mut limits = BTreeSet::new();
        let mut fuel = 900;
        let mut shifts = Vec::new();
        self.possible_writes(v, &known, &mut fuel, &mut shifts)?;
        for (v, k) in shifts {
            let shift = self.def(v)?;
            let [n, x] = valu(shift)?.src.as_slice() else {
                return None;
            };
            if shift.name() != "v_lshlrev_b32" || self.raw(shift, n)? != Term::Const(5) {
                return None;
            }
            let Term::Value(value) = self.raw(shift, x)? else {
                return None;
            };
            let mut pending = vec![(value, k)];
            while let Some((value, k)) = pending.pop() {
                let mut reads = Vec::new();
                self.possible_writes(value, &k, &mut fuel, &mut reads)?;
                for (v, k) in reads {
                    let load_site = self.defs[v.0 as usize]?;
                    let load = self.inst(load_site);
                    if let Some((records, base, field)) = self.chain_word(v, r) {
                        let (pointer, pointer_offset) = self.constant_buffer_word(base, r)?;
                        fields.insert((records, pointer, pointer_offset, field));
                        continue;
                    }
                    if matches!(load.name(), "v_mov_b32" | "v_cndmask_b32") {
                        let args = &valu(load)?.src;
                        let count = if load.name() == "v_mov_b32" { 1 } else { 2 };
                        for arg in args.get(..count)? {
                            let Term::Value(arm) = self.raw(load, arg)? else {
                                return None;
                            };
                            pending.push((arm, k.clone()));
                        }
                        continue;
                    }
                    let Fields::Buffer {
                        data: Some(Operand::Vector { reg: dst, dwords }),
                        addr: Operand::Vector { reg, .. },
                        soffset,
                        offset,
                        offen,
                        idxen,
                        format: None,
                        tfe: false,
                        lds: false,
                        ..
                    } = &load.fields
                    else {
                        return None;
                    };
                    if !matches!(
                        load.name(),
                        "buffer_load_dword"
                            | "buffer_load_dwordx2"
                            | "buffer_load_dwordx3"
                            | "buffer_load_dwordx4"
                    ) {
                        return None;
                    }
                    let Term::Const(base) = self.term(load, soffset)? else {
                        return None;
                    };
                    let component = (0..*dwords).find(|n| load.write(Reg::V(*dst + *n)) == Some(v))?;
                    let records = self.buffer_index_table(load_site, r)?;
                    let field = base
                        .wrapping_add(u32::from(*offset))
                        .wrapping_add(u32::from(component) * 4);
                    let address = load.read(Reg::V(*reg))?;
                    let addresses = if *offen && !*idxen {
                        self.address_words(address, &k, inputs, r, &mut limits, &mut fuel)
                    } else {
                        None
                    };
                    if addresses.is_none() {
                        let stride = if *idxen && !*offen && base == 0 {
                            Some(self.record_stride(records.0, records.1, field, used)?)
                        } else {
                            None
                        };
                        let step = if let Some(stride) = stride {
                            1 << stride.trailing_zeros()
                        } else if *offen && !*idxen {
                            let mut writers = Vec::new();
                            self.possible_writes(address, &k, &mut fuel, &mut writers)?;
                            let mut shift = 31;
                            for (val, _) in writers {
                                let i = self.def(val)?;
                                let [bits, _] = valu(i)?.src.as_slice() else {
                                    return None;
                                };
                                let Term::Const(s) = self.raw(i, bits)? else {
                                    return None;
                                };
                                if i.name() != "v_lshlrev_b32" {
                                    return None;
                                }
                                shift = shift.min(s & 31);
                            }
                            if shift < 2 {
                                return None;
                            }
                            1 << shift
                        } else {
                            return None;
                        };
                        let index = BufferIndex {
                            table_slot: records.0,
                            table_dword: records.1,
                            word: HeapOffset {
                                bytes: field % step,
                                constant: None,
                            },
                            word_step: step,
                            bytes: 0,
                            guard: None,
                            records: None,
                            value_mask: u32::MAX,
                        };
                        if let Some(proven) = &mut ranges {
                            if let Some(stride) = stride
                                && let Some(column) = self.scalar_range_column(load, &k, r, index, field, stride)
                            {
                                if !proven.contains(&column) { proven.push(column); }
                            } else {
                                ranges = None;
                            }
                        }
                        if !words.contains(&index) {
                            words.push(index);
                        }
                        continue;
                    }
                    for (ptr, delta) in addresses? {
                        let (pointer, pointer_offset) = ptr?;
                        fields.insert((
                            records,
                            pointer,
                            pointer_offset,
                            delta
                                .wrapping_add(base)
                                .wrapping_add(u32::from(*offset))
                                .wrapping_add(u32::from(component) * 4),
                        ));
                    }
                }
            }
        }
        if fields.is_empty() && !words.is_empty() {
            if let Some(ranges) = ranges && !ranges.is_empty()
                && r.waterfall.loads.contains(&site) && self.image_only_in_waterfall(site, r)
            {
                return Some(PixelFields::Ranges(ranges));
            }
            return Some(PixelFields::Words(words));
        }
        if fields.is_empty() || !words.is_empty() {
            return None;
        }
        Some(PixelFields::Pointers(
            fields
                .into_iter()
                .map(|(records, pointer, pointer_offset, field)| PointerField {
                    records,
                    pointer,
                    pointer_offset,
                    field,
                })
                .collect(),
            limits.into_iter().collect(),
        ))
    }

    // Each transfer below can only clear bits. Induction over a loop phi
    // requires all of its incoming masks, including the first entry, to
    // arise from the original compare; an unrelated value fails the proof.
    fn mask_subset(&self, v: Value, limit: Value) -> bool {
        let mut pending = vec![v];
        let mut seen = BTreeSet::new();
        while let Some(v) = pending.pop() {
            if v == limit || !seen.insert(v) {
                continue;
            }
            if seen.len() > 64 {
                return false;
            }
            if let Some((b, i)) = self.phis[v.0 as usize] {
                pending.extend(&self.p.blocks[b].phis[i].args);
                continue;
            }
            let Some(inst) = self.def(v) else {
                return false;
            };
            let Some((Reg::S(reg), _)) = inst.writes.iter().find(|(_, w)| *w == v) else {
                return false;
            };
            let half = reg & 1;
            let source = if inst.name().starts_with("v_cmpx_") && *reg >= EXEC && *reg <= EXEC + 1 {
                EXEC + half
            } else if matches!(inst.name(), "s_mov_b64" | "s_andn2_wrexec_b64") {
                let Fields::Sop {
                    src,
                    dst: Some(Operand::Scalar { code: dst, .. }),
                    ..
                } = &inst.fields
                else {
                    return false;
                };
                let [Operand::Scalar { code, .. }] = src.as_slice() else {
                    return false;
                };
                let k = if *reg == EXEC || *reg == EXEC + 1 {
                    half
                } else if *reg >= *dst && *reg <= *dst + 1 {
                    reg - dst
                } else {
                    return false;
                };
                *code + k
            } else {
                return false;
            };
            let Some(next) = inst.read(Reg::S(source)) else {
                return false;
            };
            pending.push(next);
        }
        true
    }

    fn address_words(
        &self,
        v: Value,
        k: &Known,
        inputs: &[PsInput],
        r: &Resources,
        limits: &mut BTreeSet<Limit>,
        fuel: &mut usize,
    ) -> Option<BTreeSet<AddressWord>> {
        let mut out = BTreeSet::new();
        let mut writes = Vec::new();
        self.possible_writes(v, k, fuel, &mut writes)?;
        for (w, k) in writes {
            if let Some(ptr) = self.constant_buffer_word(w, r) {
                out.insert((Some(ptr), 0));
                continue;
            }
            if let Some((_, [Term::Value(base), Term::Const(off)] | [Term::Const(off), Term::Value(base)])) =
                self.sop2(w, &["s_add_i32", "s_add_u32"])
            {
                let ptr = self.constant_buffer_word(base, r)?;
                out.insert((Some(ptr), off));
                continue;
            }
            if let Some((_, operands)) = self.sop2(w, &["s_min_u32"]) {
                let bound = operands.into_iter().filter_map(|operand| match operand {
                    Term::Const(bound) => Some(bound),
                    Term::Value(_) => None,
                }).min()?;
                if bound >= MAX_FIELDS as u32 { return None; }
                out.extend((0..=bound).map(|value| (None, value)));
                continue;
            }
            let i = self.def(w)?;
            if let Fields::Vintrp {
                src: Operand::Const(2),
                attr:
                    Operand::Attr {
                        index,
                        chan,
                        high: false,
                    },
                ..
            } = &i.fields
            {
                if i.name() != "v_interp_mov_f32" {
                    return None;
                }
                let a = inputs.iter().find(|a| {
                    a.attr == *index
                        && a.usage == Usage::Generic
                        && a.flat
                        && !a.fp16
                        && !a.pack2
                        && a.mask & (1 << chan) != 0
                })?;
                limits.insert((a.row, *chan, FLAT_LIMIT));
                out.extend((0..=FLAT_LIMIT).map(|x| (None, x)));
                continue;
            }
            let op = valu(i)?;
            let mut operand = |x: &Operand| -> Option<BTreeSet<AddressWord>> {
                match self.raw(i, x)? {
                    Term::Const(k) => Some(BTreeSet::from([(None, k)])),
                    Term::Value(v) => self.address_words(v, &k, inputs, r, limits, fuel),
                }
            };
            match (i.name(), op.src.as_slice()) {
                ("v_mov_b32", [x]) => out.extend(operand(x)?),
                ("v_min_u32", [a, b]) => {
                    let bound = [a, b]
                        .into_iter()
                        .filter_map(|x| match self.raw(i, x)? {
                            Term::Const(n) => Some(n),
                            _ => None,
                        })
                        .min()?;
                    if bound >= MAX_FIELDS as u32 {
                        return None;
                    }
                    out.extend((0..=bound).map(|x| (None, x)));
                }
                (name @ ("v_add_nc_u32" | "v_or_b32" | "v_lshlrev_b32" | "v_lshl_add_u32"), ops) => {
                    let [a, b, ..] = ops else {
                        return None;
                    };
                    let av = operand(a)?;
                    let bv = operand(b)?;
                    let cv = if name == "v_lshl_add_u32" {
                        operand(ops.get(2)?)?
                    } else {
                        BTreeSet::from([(None, 0)])
                    };
                    for &(p, x) in &av {
                        for &(q, y) in &bv {
                            for &(s, z) in &cv {
                                let term = match name {
                                    "v_add_nc_u32" if p.is_none() || q.is_none() => (p.or(q), x.wrapping_add(y)),
                                    "v_or_b32" if p.is_none() && q.is_none() => (None, x | y),
                                    "v_lshlrev_b32" if p.is_none() && q.is_none() => (None, y.wrapping_shl(x)),
                                    "v_lshl_add_u32" if p.is_none() && q.is_none() => {
                                        (s, x.wrapping_shl(y).wrapping_add(z))
                                    }
                                    _ => return None,
                                };
                                out.insert(term);
                                if out.len() > MAX_FIELDS {
                                    return None;
                                }
                            }
                        }
                    }
                }
                _ => return None,
            }
        }
        Some(out)
    }
}

pub(crate) fn mesh_output_limits(
    p: &Program,
    native: &NativeProgram,
    st: &Structured,
    io: &Io,
    channels: &BTreeSet<(u8, u8)>,
) -> Vec<Limit> {
    let Ok(t) = Tracer::new(p, &native.layout, native.stage, Some(st)) else {
        return vec![];
    };
    let mut maxima = BTreeMap::new();
    for exp in &io.exports {
        let Target::Param {
            usage: Usage::Generic,
            row,
        } = exp.target
        else {
            continue;
        };
        let inst = t.inst(exp.site);
        let mut known = vec![(st.reach[exp.site.0], true)];
        if let Some(guard) = st.guards[exp.site.0][exp.site.1] {
            known.push((guard, true));
        }
        if let Some(exec) = inst
            .read(Reg::S(EXEC))
            .and_then(|x| st.mask_bits(x, inst.read(Reg::S(EXEC + 1))))
        {
            known.push((exec, true));
        }
        for (col, v) in exp.values.iter().enumerate() {
            if !channels.contains(&(row, col as u8)) {
                continue;
            }
            if let Some(v) = v {
                let bound = if exp.compressed {
                    None
                } else {
                    t.output_max(*v, &known, io, &mut 128).or_else(|| {
                        let mut with_comparison = known.clone();
                        t.mesh_export_facts(exp.site, &mut with_comparison);
                        if with_comparison.len() == known.len() { return None; }
                        t.output_max(*v, &with_comparison, io, &mut 128)
                    })
                };
                let entry = maxima.entry((row, col as u8)).or_insert(Some(0u32));
                *entry = entry.zip(bound).map(|(a, b)| a.max(b));
            } else {
                maxima.insert((row, col as u8), None);
            }
        }
    }
    maxima
        .into_iter()
        .filter_map(|((row, col), m)| Some((row, col, m.filter(|m| *m <= 255)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_export_reuses_the_exact_vertex_comparison_after_exec_restore() {
        let prove = |late| {
            let words: [u32; 13] = [
                0xbefe_04c1,                 // All lanes initially run.
                0xd765_0001, 0x0001_00c1,   // An unrelated, unbounded v1.
                0x7da8_0028,                // EXEC = s40 > v0.
                0xbf88_0001,                // Skip the bounded write if inactive.
                0x7e02_0287,                // v1 = 7 on active lanes only.
                0xbefe_04c1,                // Restore all lanes.
                late,                       // VCC = the final vertex test.
                0xbefe_046a,                // EXEC = VCC.
                0xbf88_0002,                // No vertices to export.
                0xf800_0211, 0x0000_0001,   // Export the material field.
                0xbf81_0000,
            ];
            let code: Vec<_> = words.iter().flat_map(|x| x.to_le_bytes()).collect();
            let p = crate::ir::build(&code, true, 32).unwrap();
            let st = crate::structure::structure(&p).unwrap();
            let t = Tracer::new(&p, &[], HwStage::Ms, Some(&st)).unwrap();
            let (block, index, export) = p.blocks.iter().enumerate().find_map(|(b, code)| {
                code.insts.iter().enumerate().find(|(_, inst)| inst.name() == "exp").map(|(i, inst)| (b, i, inst))
            }).unwrap();
            let exec = st.mask_bits(export.read(Reg::S(EXEC))?, export.read(Reg::S(EXEC + 1)))?;
            let mut facts = vec![(st.reach[block], true), (exec, true)];
            let v = export.read(Reg::V(1))?;
            let io = Io::default();
            assert_eq!(t.output_max(v, &facts, &io, &mut 128), None);
            t.mesh_export_facts((block, index), &mut facts);
            t.output_max(v, &facts, &io, &mut 128)
        };
        assert_eq!(prove(0x7d88_0028), Some(7));
        assert_eq!(prove(0x7d88_0029), None); // Different count SSA operand.
    }

    #[test]
    fn linked_fields_bound_a_broadcast_scalar_unsigned_minimum() {
        let prove = |bound| {
            let words: [u32; 5] = [
                0x8380_ff00, bound, // s0 = s_min_u32(s0, bound).
                0x7e12_0200,       // v9 = s0.
                0x3412_1284,       // v9 <<= 4.
                0xbf81_0000,
            ];
            let code: Vec<_> = words.iter().flat_map(|x| x.to_le_bytes()).collect();
            let p = crate::ir::build(&code, false, 32).unwrap();
            let st = crate::structure::structure(&p).unwrap();
            let t = Tracer::new(&p, &[], HwStage::Ps, Some(&st)).unwrap();
            let shift = p.blocks[0].insts.iter().find(|i| i.name() == "v_lshlrev_b32").unwrap();
            let active = st.mask_bits(shift.read(Reg::S(EXEC))?, None)?;
            let known = vec![(active, true)];
            t.address_words(shift.write(Reg::V(9))?, &known, &[], &Resources::default(), &mut Default::default(), &mut 64)
        };
        assert_eq!(prove(7), Some((0..8).map(|i| (None, i * 16)).collect()));
        assert_eq!(prove(MAX_FIELDS as u32), None);
    }
}
