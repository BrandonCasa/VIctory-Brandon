use super::structured_index::plain;
use super::*;
use crate::UsedResource;

impl<'a> Tracer<'a> {
    /// Each lane that can supply this record must have executed the
    /// halfword extraction, its structured load, and the bounded row index.
    pub(super) fn packed_material_column(
        &self,
        read: &Inst,
        known: &Known,
        resources: &Resources,
        used: &[UsedResource],
        index: BufferIndex,
        field: u32,
        stride: u32,
    ) -> Option<PackedMaterialColumn> {
        if self.stage != HwStage::Ps || stride != 64 {
            return None;
        }
        let Fields::Buffer {
            addr: Operand::Vector { reg, .. },
            ..
        } = &read.fields
        else {
            return None;
        };
        let half = self.selected_lane_def(read.read(Reg::V(*reg))?, known)?;
        let args = &plain(half)?.src;
        let [left, right] = args.as_slice() else {
            return None;
        };
        let (shift, value) = match half.name() {
            "v_lshrrev_b32" if self.term(half, left) == Some(Term::Const(16)) => (16, right),
            "v_and_b32" if self.term(half, left) == Some(Term::Const(0xffff)) => (0, right),
            "v_and_b32" if self.term(half, right) == Some(Term::Const(0xffff)) => (0, left),
            _ => return None,
        };
        let Operand::Vector { reg, .. } = value else {
            return None;
        };
        let packed_value = half.read(Reg::V(*reg))?;
        let packed_site = self.defs[packed_value.0 as usize]?;
        let packed = self.selected_lane_def(packed_value, known)?;
        let Fields::Buffer {
            data: Some(Operand::Vector {
                reg: packed_reg, ..
            }),
            addr: Operand::Vector { reg: row, .. },
            offset,
            soffset,
            offen: false,
            idxen: true,
            format: None,
            lds: false,
            tfe: false,
            ..
        } = &packed.fields
        else {
            return None;
        };
        if packed.name() != "buffer_load_dword"
            || packed_reg != reg
            || self.term(packed, soffset)? != Term::Const(0)
        {
            return None;
        }
        let parent_field = u32::from(*offset);
        let parent = self.buffer_index_table(packed_site, resources)?;
        let parent_stride = self.record_stride(parent.0, parent.1, parent_field, used)?;
        if parent_stride != 16 {
            return None;
        }

        let row = self.selected_lane_def(packed.read(Reg::V(*row))?, known)?;
        let [Operand::Vector { .. }, start, width] = plain(row)?.src.as_slice() else {
            return None;
        };
        let Term::Const(start) = self.term(row, start)? else {
            return None;
        };
        let Term::Const(width @ 1..=8) = self.term(row, width)? else {
            return None;
        };
        if row.name() != "v_bfe_u32" || start.checked_add(width)? > 32 {
            return None;
        }
        Some(PackedMaterialColumn {
            parent,
            records: (index.table_slot, index.table_dword),
            parent_count: 1 << width,
            parent_stride,
            packed_field: parent_field,
            shift,
            index_mask: 0xffff,
            record_stride: stride,
            handle_field: field,
            bytes: index.bytes,
        })
    }

    fn selected_lane_def(&self, value: Value, known: &Known) -> Option<&'a Inst> {
        let inst = self.def(value)?;
        let lanes = self.lanes?;
        let low = inst.read(Reg::S(EXEC))?;
        let high = if self.p.wave64 {
            Some(inst.read(Reg::S(EXEC + 1))?)
        } else {
            None
        };
        let mask = lanes.mask_bits(low, high)?;
        let mut skipped = known.clone();
        skipped.push((mask, false));
        (!self.meet(lanes, &skipped)).then_some(inst)
    }
}
