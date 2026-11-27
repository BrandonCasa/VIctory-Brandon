use super::*;
use crate::ir::VCC;
use crate::native::{ComputeIo, ResourceType, UsedResource};

type Table = (u8, u32);

const PARENT: Table = (0, 288);
const RECORDS: Table = (0, 648);
const TAG: Table = (0, 416);
const STRIDE: u32 = 116;
const ROW_MASK: u32 = 0x3fff;

impl<'a> Tracer<'a> {
    pub(super) fn indirect_record_columns(
        &self, resources: &Resources, used: &[UsedResource], cs: &ComputeIo,
    ) -> Vec<(Site, Site, IndirectRecordColumn)> {
        // On this native shape, EXEC on entry contains all 32 hardware lanes.
        if self.p.wave64 || cs.threads != [32, 1, 1]
            || !srv(used, PARENT, STRIDE) || !srv(used, RECORDS, 1)
        {
            return Vec::new();
        }
        let mut columns = Vec::new();
        for descriptor in &resources.descriptors {
            if descriptor.role != Role::Resource || !matches!(self.inst(descriptor.site).fields, Fields::Image { .. }) {
                continue;
            }
            let Source::Loaded { load, first: 0 } = descriptor.source else { continue; };
            let Ok(i) = resources.scalar.binary_search_by_key(&load, |(site, _)| *site) else { continue; };
            let address = resources.scalar[i].1;
            if self.layout.get(address.param).is_some_and(|p| p.root == RootType::ResourceHeap)
                && let Some(column) = self.indirect_record_column(address, descriptor.site, true, resources, used)
            {
                columns.push((load, descriptor.site, column));
            }
        }
        for &(load, address) in &resources.scalar {
            if !self.layout.get(address.param).is_some_and(|p| p.root == RootType::ResourceHeap)
                || self.inst(load).name() != "s_load_dwordx8"
            {
                continue;
            }
            // Lowering requests descriptor data even when every original
            // consumer simply uses the loaded descriptor as an image. Those
            // synthetic dimension results are dead. A live dimension query
            // needs its own exact proof at the SMEM instruction.
            let dead = columns.iter().find(|(site, _, _)| *site == load)
                .filter(|_| self.image_descriptor_only(load, resources))
                .map(|&(_, _, column)| column);
            if let Some(column) = dead.or_else(|| self.indirect_record_column(address, load, false, resources, used)) {
                columns.push((load, load, column));
            }
        }
        columns
    }

    fn indirect_record_column(
        &self, heap: Address, consumer: Site, active_image: bool, resources: &Resources, used: &[UsedResource],
    ) -> Option<IndirectRecordColumn> {
        let (_, [Term::Value(handle), Term::Const(5)]) = self.sop2(heap.dynamic?, &["s_lshl_b32"])? else {
            return None;
        };
        let (records, pointer, handle_offset) = self.chain_word(handle, resources)?;
        if records != RECORDS || !handle_offset.is_multiple_of(4) || !heap.offset.is_multiple_of(32) {
            return None;
        }
        let pointer = self.broadcast_pointer(pointer, consumer, active_image).unwrap_or(pointer);
        let (parent, row, pointer_field) = self.chain_word(pointer, resources)?;
        let (index, stride) = self.scalar_scale(row)?;
        let (_, bits) = self.sop2(index, &["s_and_b32"])?;
        if parent != PARENT || stride != STRIDE || !matches!(pointer_field, 64 | 96)
            || !bits.contains(&Term::Const(ROW_MASK))
        {
            return None;
        }
        let guard = srv(used, TAG, 2048).then(|| self.record_tag(row, consumer, active_image, resources)).flatten();
        Some(IndirectRecordColumn {
            parent, count: ROW_MASK + 1, stride, pointer_field, records, handle_offset,
            heap_bytes: heap.offset, guard,
        })
    }

    /// Some variants broadcast a scalar table word to active lanes, then
    /// recover it through the first active lane. A restored consumer must
    /// still imply that the broadcast had a lane in its own EXEC.
    fn broadcast_pointer(&self, pointer: Value, consumer: Site, active_image: bool) -> Option<Value> {
        let read = self.def(pointer)?;
        let [Operand::Vector { reg, dwords: 1 }, lane] = self.plain_valu(read, "v_readlane_b32")? else {
            return None;
        };
        let lane = self.term(read, lane)?;
        let Term::Value(lane) = lane else { return None; };
        let first = self.def(lane)?;
        let Fields::Sop { src, .. } = &first.fields else { return None; };
        if first.name() != "s_ff1_i32_b32" || first.writes.first()?.1 != lane { return None; }
        let Term::Value(mask) = self.term(first, src.first()?)? else { return None; };

        let broadcast_site = self.defs[read.read(Reg::V(*reg))?.0 as usize]?;
        if (broadcast_site.0 == consumer.0 && consumer.1 <= broadcast_site.1)
            || (broadcast_site.0 != consumer.0 && self.reaches(consumer.0, broadcast_site.0))
        {
            return None;
        }
        let broadcast = self.inst(broadcast_site);
        let [word @ Operand::Scalar { dwords: 1, .. }] = self.plain_valu(broadcast, "v_mov_b32")? else {
            return None;
        };
        let exec = broadcast.read(Reg::S(EXEC))?;
        let active = self.origin(exec);
        if Term::Value(mask) != active {
            let (_, [left, right]) = self.sop2(mask, &["s_and_b32"])?;
            let entry_exec = |t| matches!(t,
                Term::Value(v) if self.inputs[v.0 as usize] == Some(Reg::S(EXEC)));
            if !(left == active && entry_exec(right) || right == active && entry_exec(left)) {
                return None;
            }
        }
        if !self.exec_is_nonempty(consumer, exec, active_image) { return None; }
        match self.term(broadcast, word)? { Term::Value(v) => Some(v), Term::Const(_) => None }
    }

    // A descriptor-only load can create a dead GetDimensions call during
    // emission. Prove there are no machine reads of its output words other
    // than the exact resource operand of image instructions. A move, join,
    // or data calculation makes this proof inapplicable.
    pub(super) fn image_descriptor_only(&self, load: Site, resources: &Resources) -> bool {
        let words = &self.inst(load).writes;
        let reads_word = |value: Value| words.iter().any(|&(_, word)| word == value);
        for (b, block) in self.p.blocks.iter().enumerate() {
            if block.phis.iter().any(|phi| phi.args.iter().copied().any(reads_word)) { return false; }
            for (i, inst) in block.insts.iter().enumerate() {
                if !inst.reads.iter().any(|&(_, value)| reads_word(value)) { continue; }
                if !matches!(inst.fields, Fields::Image { .. }) || !resources.descriptors.iter().any(|d| {
                    d.site == (b, i) && d.role == Role::Resource
                        && matches!(d.source, Source::Loaded { load: from, first: 0 } if from == load)
                }) {
                    return false;
                }
            }
        }
        true
    }

    fn plain_valu(&self, inst: &'a Inst, name: &str) -> Option<&'a [Operand]> {
        let Fields::Valu(f) = &inst.fields else { return None; };
        (inst.name() == name && f.sdwa.is_none() && f.dpp.is_none() && f.abs == 0 && f.neg == 0
            && f.opsel == 0 && f.omod == 0 && !f.clamp).then_some(f.src.as_slice())
    }

    pub(super) fn active_implies_exec(&self, site: Site, exec: Value) -> bool {
        if self.inst(site).read(Reg::S(EXEC)).is_some_and(|v| self.origin(v) == self.origin(exec)) {
            return true;
        }
        let Some(lanes) = self.lanes else { return false; };
        let Some(&mask) = lanes.masks.get(&exec) else { return false; };
        let mut known = vec![(lanes.reach[site.0], true), (mask, false)];
        if let Some(guard) = lanes.guards[site.0][site.1] { known.push((guard, true)); }
        !self.meet(lanes, &known)
    }

    pub(super) fn exec_is_nonempty(&self, site: Site, exec: Value, active_image: bool) -> bool {
        if active_image { return self.active_implies_exec(site, exec); }
        let exec = self.origin(exec);
        if matches!(exec, Term::Value(v) if self.inputs[v.0 as usize] == Some(Reg::S(EXEC))) {
            return true;
        }
        // A scalar load has no active-lane premise. Require the very same
        // mask to have passed a wave-nonzero branch on every path to it.
        self.dominating_arm(site.0, |branch, arm| {
            let block = &self.p.blocks[branch];
            let Exit::Branch { taken, not_taken } = block.exit else { return false; };
            if taken == not_taken { return false; }
            let Some(check) = block.insts.last() else { return false; };
            match check.name() {
                "s_cbranch_execz" if not_taken == arm => {
                    check.read(Reg::S(EXEC)).is_some_and(|v| self.origin(v) == exec)
                }
                "s_cbranch_execnz" if taken == arm => {
                    check.read(Reg::S(EXEC)).is_some_and(|v| self.origin(v) == exec)
                }
                "s_cbranch_vccz" if not_taken == arm => {
                    check.read(Reg::S(VCC)).and_then(|v| self.sop2(v, &["s_and_b32"]))
                        .is_some_and(|(_, bits)| bits.contains(&exec))
                }
                "s_cbranch_vccnz" if taken == arm => {
                    check.read(Reg::S(VCC)).and_then(|v| self.sop2(v, &["s_and_b32"]))
                        .is_some_and(|(_, bits)| bits.contains(&exec))
                }
                _ => false,
            }
        })
    }

    fn dominating_arm(&self, target: usize, accepts: impl Fn(usize, usize) -> bool) -> bool {
        let dom = self.dominators.get_or_init(|| {
            crate::ir::idoms(&self.p.blocks, &crate::ir::reverse_postorder(&self.p.blocks), &self.rpo)
        });
        let mut arm = target;
        while arm != 0 {
            let Some(&branch) = dom.get(arm) else { return false; };
            if branch == arm || branch == usize::MAX { break; }
            if self.p.blocks[arm].preds.as_slice() == [branch]
                && accepts(branch, arm) && !self.reaches(target, branch)
            {
                return true;
            }
            arm = branch;
        }
        false
    }

    fn record_tag(&self, row: Value, consumer: Site, active_image: bool, resources: &Resources) -> Option<RecordTag> {
        self.dominating_arm(consumer.0, |branch, arm| self.tag_arm(branch, arm, row, consumer, active_image, resources))
            .then_some(RecordTag { parent_field: 8, table: TAG, field: 0x710 })
    }

    fn tag_arm(&self, branch: usize, arm: usize, row: Value, consumer: Site, active_image: bool, resources: &Resources) -> bool {
        let block = &self.p.blocks[branch];
        let Exit::Branch { taken, not_taken } = block.exit else { return false; };
        if taken == not_taken { return false; }
        let Some(branch) = block.insts.last() else { return false; };
        let nonzero = match branch.name() {
            "s_cbranch_vccz" => not_taken == arm,
            "s_cbranch_vccnz" => taken == arm,
            _ => return false,
        };
        let Some(vcc) = branch.read(Reg::S(VCC)) else { return false; };
        let Some((and, bits)) = self.sop2(vcc, &["s_and_b32"]) else { return false; };
        let Some(exec) = and.read(Reg::S(EXEC)) else { return false; };
        let flag = match bits {
            [a, Term::Value(v)] if a == self.origin(exec) => v,
            [Term::Value(v), a] if a == self.origin(exec) => v,
            _ => return false,
        };
        if !nonzero && !self.exec_is_nonempty(consumer, exec, active_image) { return false; }
        let matches = |v| self.tag_flag(v, row, self.origin(exec), nonzero, resources).is_some();
        if matches(flag) { return true; }
        let join = if nonzero { "s_and_b32" } else { "s_or_b32" };
        self.sop2(flag, &[join]).is_some_and(|(_, operands)| operands.into_iter().any(|term| {
            matches!(term, Term::Value(v) if matches(v))
        }))
    }

    fn tag_flag(&self, flag: Value, row: Value, exec: Term, nonzero: bool, resources: &Resources) -> Option<()> {
        let (select, [set, Term::Const(0)]) = self.sop2(flag, &["s_cselect_b32"])? else { return None; };
        if set != exec { return None; }
        let scc = select.read(Reg::Scc)?;
        let comparison = if nonzero { "s_cmp_eq_u32" } else { "s_cmp_lg_u32" };
        let (_, [Term::Value(a), Term::Value(b)]) = self.sop2(scc, &[comparison])? else { return None; };
        [(a, b), (b, a)].into_iter().any(|(value, expected)| {
            self.chain_word(value, resources) == Some((PARENT, row, 8))
                && self.constant_buffer_word(expected, resources) == Some((TAG, 0x710))
        }).then_some(())
    }
}

fn srv(used: &[UsedResource], table: Table, stride: u32) -> bool {
    used.iter().any(|r| (r.slot, r.table_offset_dwords) == table && r.root == RootType::Table
        && r.kind == ResourceType::Srv && r.stride == stride && r.dwords == 4)
}
