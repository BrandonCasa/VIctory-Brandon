use super::*;
use crate::gfx10::Valu;
use crate::{ResourceType, UsedResource};

fn plain(inst: &Inst) -> Option<&Valu> {
    let Fields::Valu(f) = &inst.fields else {
        return None;
    };
    (f.sdwa.is_none() && f.dpp.is_none() && f.abs == 0 && f.neg == 0 && !f.clamp && f.omod == 0 && f.opsel == 0)
        .then_some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masked_writes_union_all_reachable_structured_components() {
        let words = [
            0xbe84_0309,
            0xbe85_0380, // Table pointer in s[4:5].
            0xf408_0402,
            0xfa00_0000, // Load s[16:19] from table dword 0.
            0xe038_2028,
            0x8004_030f, // Load v[3:6] at record byte 40.
            0xbe9a_047e, // Save both halves of EXEC.
            0x7da4_1a80, // v_cmpx_eq_u32 exec, 0, v13
            0xe030_200c,
            0x8004_060f, // Overwrite selected v6 at byte 12.
            0xbefe_041a, // Restore both halves of EXEC.
            0x3424_0c85, // v_lshlrev_b32 v18, 5, v6
            0xbf81_0000,
        ];
        let prove = |words: &[u32], wave64| -> Option<Vec<u32>> {
            let code: Vec<_> = words.iter().flat_map(|x| x.to_le_bytes()).collect();
            let p = crate::ir::build(&code, wave64, 32).unwrap();
            let lanes = crate::structure::structure(&p).unwrap();
            let layout = [RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 9,
                slot: 1,
            }];
            let meta = [UsedResource {
                root: RootType::Table,
                kind: ResourceType::Srv,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 64,
                dwords: 4,
                user_data: 9,
                slot: 1,
            }];
            let resources = track(&p, &layout, HwStage::Ps, Some(&lanes)).unwrap();
            let t = Tracer::new(&p, &layout, HwStage::Ps, Some(&lanes)).unwrap();
            let shift = p
                .blocks
                .iter()
                .flat_map(|b| &b.insts)
                .find(|i| i.name() == "v_lshlrev_b32")
                .unwrap();
            let high = if wave64 { Some(shift.read(Reg::S(EXEC + 1))?) } else { None };
            let active = lanes.mask_bits(shift.read(Reg::S(EXEC))?, high)?;
            let mut out = Vec::new();
            t.selector_branches(
                shift.write(Reg::V(18))?,
                true,
                &vec![(active, true)],
                &resources,
                &meta,
                0,
                &mut out,
                &mut 128,
            )?;
            let mut fields: Vec<_> = out
                .iter()
                .map(|index| {
                    assert_eq!(index.word_step, 64);
                    index.word.bytes
                })
                .collect();
            fields.sort_unstable();
            Some(fields)
        };
        assert_eq!(prove(&words, true), Some(vec![12, 52]));
        let mut bad = words;
        bad[11] = 0x3424_0c84; // A different shift does not scale slots by 32.
        assert_eq!(prove(&bad, true), None);
        let mut bad = words;
        bad[8] = 0x7e0c_0303; // v_mov_b32 v6, v3 on a reachable masked arm.
        bad[9] = 0xbf80_0000;
        assert_eq!(prove(&bad, true), None);

        let mut wave32 = words;
        wave32[6] = 0xbe9a_037e; // Save EXEC_LO.
        wave32[10] = 0xbefe_031a; // Restore EXEC_LO.
        assert_eq!(prove(&wave32, false), Some(vec![12, 52]));
        wave32[8] = 0x7e0c_0280; // A reachable masked arm writes the default slot.
        wave32[9] = 0xbf80_0000;
        assert_eq!(prove(&wave32, false), Some(vec![52]));
        wave32[8] = 0x7e0c_0281; // Another literal is outside the column proof.
        assert_eq!(prove(&wave32, false), None);
    }

    #[test]
    fn image_waterfall_proof_does_not_cover_a_scalar_descriptor_read() {
        let words = super::super::tests::WATERFALL_INDEXED_WAVE32;
        let accepts = |words: &[u32]| {
            let code: Vec<_> = words.iter().flat_map(|x| x.to_le_bytes()).collect();
            let p = crate::ir::build(&code, false, 256).unwrap();
            let lanes = crate::structure::structure(&p).unwrap();
            let layout = [(RootType::ResourceHeap, 0), (RootType::Table, 1)].map(|(root, slot)| RootParam {
                root,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: slot.into(),
                slot,
            });
            let resources = track(&p, &layout, HwStage::Cs, Some(&lanes)).unwrap();
            let [load] = resources.waterfall.loads.as_slice() else { panic!("expected one waterfall"); };
            Tracer::new(&p, &layout, HwStage::Cs, Some(&lanes)).unwrap().image_only_in_waterfall(*load, &resources)
        };
        assert!(accepts(words));
        let mut mixed = words.to_vec();
        let retry = mixed.iter().position(|&w| w == 0xbe95_4715).unwrap();
        mixed[retry + 1] = 0xbf89_fff5; // The back edge still reaches the same instruction.
        mixed.insert(retry, 0xbe9c_0310); // s_mov_b32 s28, s16 reads a descriptor word.
        assert!(!accepts(&mixed));
    }

    #[test]
    fn bounded_record_loop_requires_the_unsigned_count_guard_and_sentinel() {
        let fixture = [
            0x7e16_02c1, // v_mov_b32 v11, -1
            0xbe89_0380, // s_mov_b32 s9, 0
            0xbf06_8007, // s_cmp_eq_u32 s7, 0
            0xbf85_0009, // skip loop when count is zero
            0xbe82_037e, // save exec
            0xbf82_0000, // jump to header
            0x8104_0609, // s4 = root first + i
            0x7e16_0204, // v11 = s4 in active lanes
            0xbf88_0003, // early exit
            0x8109_8109, // i += 1
            0xbf09_0709, // s_cmp_ge_u32 i, count
            0xbf84_fffa, // continue while i < count
            0xbefe_0302, // restore exec
            0x7daa_16c1, // v_cmpx_ne_u32 exec_lo, -1, v11
            0xe030_2050,
            0x8003_100b, // buffer_load_dword v16, v11 idxen offset:80
            0xbf81_0000,
        ];
        let prove = |words: &[u32]| {
            let code = words.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<_>>();
            let p = crate::ir::build(&code, false, 32).unwrap();
            let layout = [RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 2,
                base_register: 0,
                user_data: 6,
                slot: 2,
            }];
            let t = Tracer::new(&p, &layout, HwStage::Cs, None).unwrap();
            let load = p
                .blocks
                .iter()
                .flat_map(|b| &b.insts)
                .find(|i| i.name() == "buffer_load_dword")
                .unwrap();
            t.selected_records(load.read(Reg::V(11)).unwrap(), load.read(Reg::S(EXEC)).unwrap())
        };
        assert_eq!(prove(&fixture), Some(((2, 0), (2, 1))));
        for (word, bad) in [
            (2, 0xbf06_8107),
            (9, 0x8109_8209),
            (10, 0xbf08_0709),
            (13, 0x7daa_1680),
        ] {
            let mut changed = fixture;
            changed[word] = bad;
            assert_eq!(prove(&changed), None);
        }
    }
}

impl Tracer<'_> {
    /// In a waterfall the first-lane read visits the active lanes of its
    /// preheader. Follow its exact EXEC mask. Phi edges and masked writes
    /// can yield several structured-buffer fields or the default slot.
    pub(super) fn structured_indices(
        &self,
        load: Site,
        resources: &Resources,
        used: &[UsedResource],
    ) -> Option<Vec<BufferIndex>> {
        if used.is_empty() {
            return None;
        }
        let lanes = self.lanes?;
        let offset = self.first_lane_offset(load)?;
        let reader = self.def(offset)?;
        let block = &self.p.blocks[load.0];
        if block.preds.len() != 2 || !block.preds.contains(&load.0) {
            return None;
        }
        let pre = block.preds.iter().position(|&b| b != load.0)?;
        let entry = |reg| {
            let (b, i) = self.phis[reader.read(Reg::S(reg))?.0 as usize]?;
            (b == load.0).then(|| block.phis[i].args[pre])
        };
        let low = entry(EXEC)?;
        let high = if self.p.wave64 { Some(entry(EXEC + 1)?) } else { None };
        let active = lanes.mask_bits(low, high)?;
        if !resources.waterfall.loads.contains(&load) {
            // The shader also reads image descriptor words as scalar numbers
            // (width and height). The retry instruction observes the selected
            // EXEC at the load even though SMEM has no explicit EXEC operand.
            // With an initially empty EXEC, readfirstlane could name lane 0.
            let retry = block.insts.len().checked_sub(2)?;
            if retry <= load.1
                || block.insts[retry].name() != "s_andn2_wrexec_b64"
                || block.insts[load.1..retry]
                    .iter()
                    .any(|i| i.write(Reg::S(EXEC)).is_some() || i.write(Reg::S(EXEC + 1)).is_some())
                || !self.nonempty_entry(block.preds[pre], [low, high?])
            {
                return None;
            }
            self.waterfall(load.0, &[retry], &[offset]).ok()?;
        }
        let known = vec![(lanes.reach[block.preds[pre]], true), (active, true)];
        let heap = resources.scalar.iter().find(|(s, _)| *s == load)?.1;
        if self.layout.get(heap.param)?.root != RootType::ResourceHeap {
            return None;
        }
        let shifted = self.first_lane_source(offset)?;
        let mut out = Vec::new();
        let mut fuel = 128;
        self.selector_branches(
            shifted,
            true,
            &known,
            resources,
            used,
            heap.offset,
            &mut out,
            &mut fuel,
        )?;
        if out.is_empty() || !self.p.wave64 && !self.image_only_in_waterfall(load, resources) {
            return None;
        }
        Some(out)
    }

    /// The image proof only bounds lanes active at waterfall entry. A
    /// scalar descriptor use can run even when no lane is active.
    fn image_only_in_waterfall(&self, load: Site, resources: &Resources) -> bool {
        let outputs = &self.inst(load).writes;
        let descriptor_word = |v| outputs.iter().any(|&(_, word)| word == v);
        for (b, block) in self.p.blocks.iter().enumerate() {
            if block.phis.iter().any(|phi| phi.args.iter().copied().any(descriptor_word)) {
                return false;
            }
            for (i, inst) in block.insts.iter().enumerate() {
                if !inst.reads.iter().any(|&(_, v)| descriptor_word(v)) {
                    continue;
                }
                if b != load.0 || !matches!(inst.fields, Fields::Image { .. })
                    || !resources.descriptors.iter().any(|d| {
                        d.site == (b, i) && d.role == Role::Resource
                            && matches!(d.source, Source::Loaded { load: read, first: 0 } if read == load)
                    })
                {
                    return false;
                }
            }
        }
        true
    }

    fn nonempty_entry(&self, mut block: usize, pair: [Value; 2]) -> bool {
        let dom = self.dominators.get_or_init(|| {
            crate::ir::idoms(
                &self.p.blocks,
                &crate::ir::reverse_postorder(&self.p.blocks),
                &self.rpo,
            )
        });
        let mask = |v| self.saved_mask(v, &mut 64).unwrap_or(v);
        let pair = pair.map(mask);
        while block != 0 {
            let parent = dom[block];
            if parent == usize::MAX || parent == block {
                break;
            }
            let guard = &self.p.blocks[parent];
            if self.p.blocks[block].preds.as_slice() == [parent]
                && let Exit::Branch { taken, not_taken } = guard.exit
                && taken != not_taken
                && let Some(branch) = guard.insts.last()
                && ((branch.name() == "s_cbranch_execz" && not_taken == block)
                    || (branch.name() == "s_cbranch_execnz" && taken == block))
                && [EXEC, EXEC + 1].map(|r| branch.read(Reg::S(r)).map(mask)) == pair.map(Some)
            {
                return true;
            }
            block = parent;
        }
        false
    }

    fn selector_branches(
        &self,
        v: Value,
        needs_shift: bool,
        known: &Known,
        resources: &Resources,
        used: &[UsedResource],
        bias: u32,
        out: &mut Vec<BufferIndex>,
        fuel: &mut usize,
    ) -> Option<()> {
        *fuel = fuel.checked_sub(1)?;
        let lanes = self.lanes?;
        if let Some((b, i)) = self.phis[v.0 as usize] {
            let block = &self.p.blocks[b];
            if block.preds.iter().any(|&p| self.rpo[p] >= self.rpo[b]) {
                return None;
            }
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
                    self.selector_branches(
                        block.phis[i].args[j],
                        needs_shift,
                        &edge,
                        resources,
                        used,
                        bias,
                        out,
                        fuel,
                    )?;
                }
            }
            return Some(());
        }
        let site = self.defs[v.0 as usize]?;
        let inst = self.inst(site);
        let reg = inst.writes.iter().find(|(_, value)| *value == v)?.0;
        let Reg::V(component) = reg else {
            return None;
        };
        let mut here = known.clone();
        let low = inst.read(Reg::S(EXEC))?;
        let high = if self.p.wave64 { Some(inst.read(Reg::S(EXEC + 1))?) } else { None };
        let mask = lanes.mask_bits(low, high)?;
        let mut skipped = known.clone();
        skipped.push((mask, false));
        if self.meet(lanes, &skipped) {
            self.selector_branches(
                inst.read(reg)?,
                needs_shift,
                &skipped,
                resources,
                used,
                bias,
                out,
                fuel,
            )?;
        }
        here.push((mask, true));
        if !self.meet(lanes, &here) {
            return Some(());
        }
        if !self.p.wave64 && inst.name() == "v_mov_b32"
            && let [src] = plain(inst)?.src.as_slice()
            && self.term(inst, src)? == Term::Const(0)
        {
            // A reflected column scan includes its bias plus zero explicitly,
            // for both this default slot and an out-of-bounds buffer read.
            // If no column is reached, the caller keeps the access opaque.
            return Some(());
        }
        if needs_shift {
            if inst.name() != "v_lshlrev_b32" {
                return None;
            }
            let [bits, Operand::Vector { reg, .. }] = plain(inst)?.src.as_slice() else {
                return None;
            };
            if self.term(inst, bits)? != Term::Const(5) {
                return None;
            }
            return self.selector_branches(
                inst.read(Reg::V(*reg))?,
                false,
                &here,
                resources,
                used,
                bias,
                out,
                fuel,
            );
        }
        let Fields::Buffer {
            data: Some(Operand::Vector { reg: dest, .. }),
            soffset,
            offset,
            offen: false,
            idxen: true,
            format: None,
            lds: false,
            tfe: false,
            ..
        } = &inst.fields
        else {
            return None;
        };
        let count = match inst.name() {
            "buffer_load_dword" => 1,
            "buffer_load_dwordx2" => 2,
            "buffer_load_dwordx3" => 3,
            "buffer_load_dwordx4" => 4,
            _ => return None,
        };
        let component = component.checked_sub(*dest)?;
        if component >= count || self.term(inst, soffset)? != Term::Const(0) {
            return None;
        }
        let (table_slot, table_dword) = self.buffer_index_table(site, resources)?;
        let field = u32::from(*offset) + 4 * u32::from(component);
        let stride = self.record_stride(table_slot, table_dword, field, used)?;
        let step = 1 << stride.trailing_zeros();
        let index = BufferIndex {
            table_slot,
            table_dword,
            word: HeapOffset {
                bytes: field % step,
                constant: None,
            },
            word_step: step,
            bytes: bias,
            guard: None,
            records: None,
            value_mask: u32::MAX,
        };
        if !out.contains(&index) {
            out.push(index);
        }
        Some(())
    }

    pub(super) fn record_stride(&self, slot: u8, dword: u32, field: u32, used: &[UsedResource]) -> Option<u32> {
        let table = self
            .layout
            .iter()
            .find(|r| r.root == RootType::Table && r.slot == slot)?;
        let meta = used.iter().find(|u| {
            u.root == RootType::Table && u.user_data == table.user_data && u.table_offset_dwords == dword
        })?;
        (meta.kind == ResourceType::Srv
            && meta.stride.is_multiple_of(4)
            && field.is_multiple_of(4)
            && field.checked_add(4)? <= meta.stride)
            .then_some(meta.stride)
    }

    // A scalar move can restore a saved EXEC after divergent control flow.
    // saveexec's explicit SGPR destination receives the *previous* mask.
    fn saved_mask(&self, v: Value, remaining: &mut u8) -> Option<Value> {
        *remaining = remaining.checked_sub(1)?;
        if let Some((b, i)) = self.phis[v.0 as usize] {
            let args = &self.p.blocks[b].phis[i].args;
            let first = self.saved_mask(*args.first()?, remaining)?;
            return args[1..]
                .iter()
                .all(|&a| self.saved_mask(a, remaining) == Some(first))
                .then_some(first);
        }
        let Some(inst) = self.def(v) else { return Some(v) };
        if inst.name() == "s_mov_b64"
            && let Fields::Sop {
                src,
                dst: Some(Operand::Scalar { code: dest, .. }),
                ..
            } = &inst.fields
            && let [Operand::Scalar { code, .. }] = src.as_slice()
            && let Some(half) = (0..2).find(|k| inst.write(Reg::S(*dest + k)) == Some(v))
        {
            return self.saved_mask(inst.read(Reg::S(*code + half))?, remaining);
        }
        if inst.name() == "s_mov_b32"
            && let Fields::Sop { src, .. } = &inst.fields
            && let [Operand::Scalar { code, .. }] = src.as_slice()
        {
            return self.saved_mask(inst.read(Reg::S(*code))?, remaining);
        }
        if inst.name() == "s_and_saveexec_b32"
            && let Fields::Sop {
                dst: Some(Operand::Scalar { code, .. }),
                ..
            } = &inst.fields
            && *code != EXEC
            && inst.write(Reg::S(*code)) == Some(v)
        {
            return self.saved_mask(inst.read(Reg::S(EXEC))?, remaining);
        }
        Some(v)
    }

    fn root_word(&self, v: Value) -> Option<(u8, u32)> {
        let (p, dword) = self.param_of(self.inputs[v.0 as usize]?)?;
        (self.layout[p].root == RootType::Constants).then_some((self.layout[p].slot, dword))
    }

    // i = phi(0, i+1); the only back edge tests i+1 < count. The unique
    // entry edge is guarded by count != 0, so every visit has i < count.
    fn counter_bound(&self, v: Value) -> Option<(u8, u32)> {
        self.root_word(self.guarded_counter_limit(v, false)?)
    }

    pub(super) fn guarded_counter_limit(&self, v: Value, signed: bool) -> Option<Value> {
        let (head, i) = self.phis[v.0 as usize]?;
        let block = &self.p.blocks[head];
        let phi = &block.phis[i];
        if phi.args.len() != 2 {
            return None;
        }
        let init = phi
            .args
            .iter()
            .position(|&a| matches!(self.origin(a), Term::Const(0)))?;
        let latch = block.preds[1 - init];
        let next = phi.args[1 - init];
        let (_, [Term::Value(old), Term::Const(1)] | [Term::Const(1), Term::Value(old)]) =
            self.sop2(next, &["s_add_i32", "s_add_u32"])?
        else {
            return None;
        };
        if old != v {
            return None;
        }
        let edge = &self.p.blocks[latch];
        if !matches!(edge.exit, Exit::Branch {taken, not_taken} if taken == head && not_taken != head) {
            return None;
        }
        let branch = edge.insts.last()?;
        if branch.name() != "s_cbranch_scc0" {
            return None;
        }
        let cmp = self.def(branch.read(Reg::Scc)?)?;
        let Fields::Sop { src, .. } = &cmp.fields else {
            return None;
        };
        if cmp.name() != if signed { "s_cmp_ge_i32" } else { "s_cmp_ge_u32" }
            || self.term(cmp, &src[0])? != Term::Value(next) {
            return None;
        }
        let Term::Value(count) = self.term(cmp, &src[1])? else {
            return None;
        };
        let mut entry = block.preds[init];
        // Walk straight-line preheaders, checking that the zero arm cannot
        // enter this loop without going back through the guarding branch.
        while let [parent] = self.p.blocks[entry].preds.as_slice() {
            if matches!(self.p.blocks[*parent].exit, Exit::Jump(dst) if dst == entry) {
                if self.rpo[*parent] >= self.rpo[entry] {
                    return None;
                }
                entry = *parent;
                continue;
            }
            let guard = &self.p.blocks[*parent];
            if !matches!(guard.exit, Exit::Branch { taken, not_taken } if not_taken == entry && taken != entry) {
                return None;
            }
            let skip = guard.insts.last()?;
            if skip.name() != "s_cbranch_scc1" {
                return None;
            }
            let test = self.def(skip.read(Reg::Scc)?)?;
            let Fields::Sop { src, .. } = &test.fields else {
                return None;
            };
            let a = self.term(test, &src[0])?;
            let b = self.term(test, &src[1])?;
            let no_entries = if signed {
                test.name() == "s_cmp_lt_i32" && (a, b) == (Term::Value(count), Term::Const(1))
            } else {
                test.name() == "s_cmp_eq_u32" && ((a, b) == (Term::Value(count), Term::Const(0))
                    || (b, a) == (Term::Value(count), Term::Const(0)))
            };
            return no_entries.then_some(count);
        }
        None
    }

    fn selected_records(&self, index: Value, mask: Value) -> Option<((u8, u32), (u8, u32))> {
        let test = self.def(self.saved_mask(mask, &mut 64)?)?;
        let [sentinel, Operand::Vector { reg, .. }] = plain(test)?.src.as_slice() else {
            return None;
        };
        if test.name() != "v_cmpx_ne_u32"
            || self.term(test, sentinel)? != Term::Const(u32::MAX)
            || test.read(Reg::V(*reg))? != index
        {
            return None;
        }
        let active = self.saved_mask(test.read(Reg::S(EXEC))?, &mut 64)?;
        let mut seen = std::collections::BTreeSet::new();
        let mut pending = vec![index];
        let mut bounds = None;
        while let Some(v) = pending.pop() {
            if !seen.insert(v) {
                continue;
            }
            if seen.len() > MAX_COPIES {
                return None;
            }
            if let Some((b, i)) = self.phis[v.0 as usize] {
                pending.extend(&self.p.blocks[b].phis[i].args);
                continue;
            }
            let mov = self.def(v)?;
            if mov.name() != "v_mov_b32" || mov.write(Reg::V(*reg)) != Some(v) {
                return None;
            }
            match self.term(mov, plain(mov)?.src.first()?)? {
                Term::Const(u32::MAX) => {}
                Term::Value(uniform) => {
                    let (_, [Term::Value(a), Term::Value(b)]) =
                        self.sop2(uniform, &["s_add_i32", "s_add_u32"])?
                    else {
                        return None;
                    };
                    let range = self
                        .root_word(a)
                        .zip(self.counter_bound(b))
                        .or_else(|| self.root_word(b).zip(self.counter_bound(a)))?;
                    if bounds.is_some_and(|old| old != range) {
                        return None;
                    }
                    bounds = Some(range);
                }
                _ => return None,
            }
            // Masked moves preserve the old VGPR in lanes they didn't cover.
            if self.saved_mask(mov.read(Reg::S(EXEC))?, &mut 64) != Some(active) {
                pending.push(mov.read(Reg::V(*reg))?);
            }
        }
        bounds
    }

    pub(super) fn structured_buffer_index(
        &self,
        load: Site,
        resources: &Resources,
        used: &[UsedResource],
    ) -> Option<BufferIndex> {
        if self.p.wave64 || used.is_empty() {
            return None;
        }
        let offset = self.first_lane_offset(load)?;
        let shifted = self.first_lane_source(offset)?;
        let shift = self.def(shifted)?;
        let [bits, Operand::Vector { reg, .. }] = plain(shift)?.src.as_slice() else {
            return None;
        };
        if shift.name() != "v_lshlrev_b32" || self.term(shift, bits)? != Term::Const(5) {
            return None;
        }
        let input = shift.read(Reg::V(*reg))?;
        let cvt = self.def(input).filter(|i| i.name() == "v_cvt_u32_f32");
        let word = if let Some(cvt) = cvt {
            let [Operand::Vector { reg, .. }] = plain(cvt)?.src.as_slice() else {
                return None;
            };
            cvt.read(Reg::V(*reg))?
        } else {
            input
        };
        let read_site = self.defs[word.0 as usize]?;
        let read = self.inst(read_site);
        let Fields::Buffer {
            data: Some(Operand::Vector { reg: dest, .. }),
            addr: Operand::Vector { reg: idx, .. },
            soffset,
            offset: field,
            offen: false,
            idxen: true,
            format: None,
            lds: false,
            tfe: false,
            ..
        } = &read.fields
        else {
            return None;
        };
        if read.name() != "buffer_load_dword"
            || read.write(Reg::V(*dest)) != Some(word)
            || self.term(read, soffset)? != Term::Const(0)
        {
            return None;
        }
        let (phi_block, phi_index) = self.phis[self.def(offset)?.read(Reg::S(EXEC))?.0 as usize]?;
        let block = &self.p.blocks[load.0];
        if phi_block != load.0 || block.preds.len() != 2 || !block.preds.contains(&load.0) {
            return None;
        }
        let pre = block.preds.iter().position(|&b| b != load.0)?;
        let mask = self.saved_mask(block.phis[phi_index].args[pre], &mut 64)?;
        for inst in [Some(read), cvt, Some(shift)].into_iter().flatten() {
            if self.saved_mask(inst.read(Reg::S(EXEC))?, &mut 64)? != mask {
                return None;
            }
        }
        let (table_slot, table_dword) = self.buffer_index_table(read_site, resources)?;
        let stride = self.record_stride(table_slot, table_dword, u32::from(*field), used)?;
        let heap = resources.scalar.iter().find(|(site, _)| *site == load)?.1;
        if self.layout.get(heap.param)?.root != RootType::ResourceHeap {
            return None;
        }
        let (records, word_step, offset) = if cvt.is_some() {
            let (first, count) = self.selected_records(read.read(Reg::V(*idx))?, mask)?;
            (Some(IndexRecords { first, count, stride }), 0, u32::from(*field))
        } else {
            // The raw shader address is element*stride + field in wrapping
            // 32-bit arithmetic. Only its power-of-two divisor is guaranteed.
            let step = 1u32 << stride.trailing_zeros();
            (None, step, u32::from(*field) % step)
        };
        Some(BufferIndex {
            table_slot,
            table_dword,
            word: HeapOffset {
                bytes: offset,
                constant: None,
            },
            word_step,
            bytes: heap.offset,
            guard: None,
            records,
            value_mask: u32::MAX,
        })
    }
}
