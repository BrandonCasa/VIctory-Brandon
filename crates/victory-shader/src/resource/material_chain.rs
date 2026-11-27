use super::*;
use crate::native::ComputeIo;

impl Tracer<'_> {
    /// One word and its byte offset in a scalar load from a static table
    /// descriptor. All additions here are the guest's 32-bit arithmetic.
    pub(super) fn chain_word(&self, value: Value, resources: &Resources) -> Option<((u8, u32), Value, u32)> {
        let site = self.defs[value.0 as usize]?;
        let read = self.inst(site);
        if !matches!(
            read.name(),
            "s_buffer_load_dword" | "s_buffer_load_dwordx2" | "s_buffer_load_dwordx4"
        ) {
            return None;
        }
        let Fields::Smem {
            data: Some(Operand::Scalar { code, dwords }),
            offset,
            soffset,
            ..
        } = &read.fields
        else {
            return None;
        };
        let component = (0..*dwords).find(|&i| read.write(Reg::S(code + i)) == Some(value))?;
        let Term::Value(base) = self.term(read, soffset)? else {
            return None;
        };
        let (base, field) = match self.sop2(base, &["s_add_i32", "s_add_u32"]) {
            Some((
                _,
                [Term::Value(base), Term::Const(k)] | [Term::Const(k), Term::Value(base)],
            )) => (base, k),
            _ => (base, 0),
        };
        let offset = field
            .wrapping_add(*offset as u32)
            .wrapping_add(4 * u32::from(component));
        Some((self.buffer_index_table(site, resources)?, base, offset))
    }

    fn chain_group_x(&self, group: Value, cs: &ComputeIo) -> Option<()> {
        let group_reg = Reg::S(cs.system_sgprs().0[0]?);
        if self.inputs[group.0 as usize] == Some(group_reg) {
            return Some(());
        }
        // readfirstlane(((group.x << k) + tid.x) >> k). Every lane has the
        // same group, and tid.x fits k bits; no per-lane values are inspected.
        let read = self.def(group)?;
        let shr = self.def(self.first_lane_source(group)?)?;
        let plain = |inst: &Inst| {
            matches!(&inst.fields, Fields::Valu(f) if f.sdwa.is_none() && f.dpp.is_none()
            && f.abs == 0 && f.neg == 0 && f.opsel == 0 && !f.clamp && f.omod == 0)
        };
        let Fields::Valu(f) = &shr.fields else {
            return None;
        };
        let [amount, Operand::Vector { reg, .. }] = f.src.as_slice() else {
            return None;
        };
        let Term::Const(k) = self.term(shr, amount)? else {
            return None;
        };
        if shr.name() != "v_lshrrev_b32" || !plain(shr) || k >= 32 || cs.threads[0] > 1u32 << k {
            return None;
        }
        let add = self.def(shr.read(Reg::V(*reg))?)?;
        let Fields::Valu(f) = &add.fields else {
            return None;
        };
        let [g, shift, Operand::Vector { reg, .. }] = f.src.as_slice() else {
            return None;
        };
        let Term::Value(g) = self.term(add, g)? else {
            return None;
        };
        let lane = add.read(Reg::V(*reg))?;
        let exec = add.read(Reg::S(EXEC))?;
        (add.name() == "v_lshl_add_u32"
            && plain(add)
            && self.term(add, shift)? == Term::Const(k)
            && self.inputs[g.0 as usize] == Some(group_reg)
            && self.inputs[lane.0 as usize] == Some(Reg::V(0))
            && shr.read(Reg::S(EXEC)) == Some(exec)
            && read.read(Reg::S(EXEC)) == Some(exec))
        .then_some(())
    }

    pub(super) fn material_chain(
        &self,
        heap: Address,
        resources: &Resources,
        cs: &ComputeIo,
    ) -> Option<MaterialChain> {
        if heap.offset != 0 {
            return None;
        }
        let (_, [Term::Value(word), Term::Const(5)]) = self.sop2(heap.dynamic?, &["s_lshl_b32"])?
        else {
            return None;
        };
        let (records, ptr, handle) = self.chain_word(word, resources)?;
        let (record_table, q, parent) = self.chain_word(ptr, resources)?;
        let (groups, byte, start) = self.chain_word(q, resources)?;
        let (_, [Term::Value(group), Term::Const(2)]) = self.sop2(byte, &["s_lshl_b32"])? else {
            return None;
        };
        self.chain_group_x(group, cs)?;
        (records == record_table && start == 0).then_some(MaterialChain {
            groups,
            records,
            parent,
            handle,
        })
    }
}
