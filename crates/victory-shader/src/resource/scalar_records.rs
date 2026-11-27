use super::*;

impl Tracer<'_> {
    pub(super) fn fixed_record_column(
        &self,
        heap: Address,
        index: BufferIndex,
        resources: &Resources,
    ) -> Option<FixedRecordColumn> {
        if self.stage != HwStage::Ps || index.word_step == 0 || index.guard.is_some() {
            return None;
        }
        let (_, [Term::Value(handle), Term::Const(5)]) = self.sop2(heap.dynamic?, &["s_lshl_b32"])? else {
            return None;
        };
        let handle = match self.sop2(handle, &["s_and_b32"]) {
            Some((_, [Term::Value(word), Term::Const(mask)] | [Term::Const(mask), Term::Value(word)]))
                if mask == index.value_mask => word,
            _ => handle,
        };
        let (table, address, field) = self.chain_word(handle, resources)?;
        let (row, stride) = self.scalar_scale(address)?;
        let (_, [_, control]) = self.sop2(row, &["s_bfe_u32"])?;
        let control = match control {
            Term::Const(control) => control,
            Term::Value(value) => {
                let (_, [Term::Const(low), Term::Const(high)]) = self.sop2(value, &["s_or_b32"])? else {
                    return None;
                };
                low | high
            }
        };
        let start = control & 31;
        let width = (control >> 16) & 127;
        if !(1..=9).contains(&width) || start + width > 32
            || !stride.is_multiple_of(4) || !field.is_multiple_of(4) || field.checked_add(4)? > stride
        {
            return None;
        }
        let count = 1u32 << width;
        (count - 1).checked_mul(stride)?.checked_add(field)?.checked_add(3)?;
        Some(FixedRecordColumn { table, field, stride, count, bytes: index.bytes, value_mask: index.value_mask })
    }

    pub(super) fn pointer_field(&self, heap: Address, r: &Resources) -> Option<PointerField> {
        if heap.offset != 0 {
            return None;
        }
        let (_, [Term::Value(handle), Term::Const(5)]) = self.sop2(heap.dynamic?, &["s_lshl_b32"])? else {
            return None;
        };
        let (records, pointer, field) = self.chain_word(handle, r)?;
        let (pointer, pointer_offset) = self.constant_buffer_word(pointer, r)?;
        Some(PointerField {
            records,
            pointer,
            field,
            pointer_offset,
        })
    }

    pub(super) fn constant_buffer_word(&self, v: Value, r: &Resources) -> Option<((u8, u32), u32)> {
        let site = self.defs[v.0 as usize]?;
        let read = self.inst(site);
        if !read.name().starts_with("s_buffer_load_dword") {
            return None;
        }
        let Fields::Smem {
            data: Some(Operand::Scalar { code, dwords }),
            soffset,
            offset,
            ..
        } = &read.fields
        else {
            return None;
        };
        let component = (0..*dwords).find(|&i| read.write(Reg::S(code + i)) == Some(v))?;
        let Term::Const(fixed) = self.term(read, soffset)? else {
            return None;
        };
        Some((
            self.buffer_index_table(site, r)?,
            fixed
                .wrapping_add(*offset as u32)
                .wrapping_add(4 * u32::from(component)),
        ))
    }

    /// Recognize uniform addresses built from a loaded instance index, a packed
    /// record number, and a zero-based loop with a proven signed record count.
    pub(super) fn scalar_record_chain(&self, heap: Address, r: &Resources) -> Option<ScalarRecordChain> {
        if heap.offset != 0 {
            return None;
        }
        let (_, [Term::Value(handle), Term::Const(5)]) = self.sop2(heap.dynamic?, &["s_lshl_b32"])? else {
            return None;
        };
        let (records, address, component) = self.chain_word(handle, r)?;
        let (_, [Term::Value(left), Term::Value(right)]) = self.sop2(address, &["s_add_u32", "s_add_i32"])?
        else {
            return None;
        };
        for (base, member) in [(left, right), (right, left)] {
            if let Some(chain) = self.record_member(base, member, records, component, r) {
                return Some(chain);
            }
        }
        None
    }

    fn record_member(
        &self,
        base: Value,
        member: Value,
        records: (u8, u32),
        component: u32,
        r: &Resources,
    ) -> Option<ScalarRecordChain> {
        let (_, [Term::Value(scaled), Term::Const(field)] | [Term::Const(field), Term::Value(scaled)]) =
            self.sop2(member, &["s_add_u32", "s_add_i32"])?
        else {
            return None;
        };
        let (_, [Term::Value(counter), Term::Const(shift)]) = self.sop2(scaled, &["s_lshl_b32"])? else {
            return None;
        };
        let handle_stride = 1u32.checked_shl(shift)?;
        let (count_buffer, count_base, count_offset) =
            self.chain_word(self.guarded_counter_limit(counter, true)?, r)?;
        if (count_buffer, count_base) != (records, base) {
            return None;
        }

        let (record_number, record_stride) = self.scalar_scale(base)?;
        let (_, [Term::Value(packed), Term::Const(shift)]) = self.sop2(record_number, &["s_lshr_b32"])? else {
            return None;
        };
        let (instances, instance_base, packed_offset) = self.chain_word(packed, r)?;
        let (value, instance_stride) = self.scalar_scale(instance_base)?;
        let index_read = self.defs[value.0 as usize]?;
        let read = self.inst(index_read);
        if read.name() != "s_buffer_load_dword" {
            return None;
        }
        let Fields::Smem { soffset, offset, .. } = &read.fields else {
            return None;
        };
        let Term::Const(fixed) = self.term(read, soffset)? else {
            return None;
        };
        Some(ScalarRecordChain {
            index: self.buffer_index_table(index_read, r)?,
            instances,
            records,
            index_offset: fixed.wrapping_add(*offset as u32),
            instance_stride,
            packed_offset,
            record_shift: (shift & 31) as u8,
            record_stride,
            count_offset,
            handle_offset: field.wrapping_add(component),
            handle_stride,
        })
    }

    fn scalar_scale(&self, v: Value) -> Option<(Value, u32)> {
        if let Some((_, [Term::Value(input), Term::Const(scale)] | [Term::Const(scale), Term::Value(input)])) =
            self.sop2(v, &["s_mul_i32"])
        {
            return Some((input, scale));
        }
        let inst = self.def(v)?;
        let Fields::Sop {
            dst: Some(Operand::Scalar { code, .. }),
            imm: Some(k),
            ..
        } = &inst.fields
        else {
            return None;
        };
        if inst.name() != "s_mulk_i32" || inst.write(Reg::S(*code)) != Some(v) {
            return None;
        }
        Some((inst.read(Reg::S(*code))?, i32::from(*k as i16) as u32))
    }
}
