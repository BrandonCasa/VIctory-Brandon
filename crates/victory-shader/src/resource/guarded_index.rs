use super::*;
use crate::ir::VCC;

impl Tracer<'_> {
    /// Compilers keep the old scalar registers when they skip a material load.
    /// A separately merged vector predicate then gates every image instruction.
    /// Only the loaded arm supplies a handle to an active sample.
    pub(super) fn guarded_handle(&self, site: Site, heap: Address, resources: &Resources) -> Option<Value> {
        let (_, [Term::Value(index), Term::Const(5)]) = self.sop2(heap.dynamic?, &["s_lshl_b32"])? else {
            return None;
        };
        // Some programs park the uniform buffer word in a VGPR in the
        // taken arm and read it back with readfirstlane at the join.
        let first_lane = self.first_lane_source(index);
        let input = first_lane.unwrap_or(index);
        let (join, item) = self.phis[input.0 as usize]?;
        let block = &self.p.blocks[join];
        let phi = &block.phis[item];
        let Exit::Branch { taken, not_taken } = block.exit else {
            return None;
        };
        let branch = block.insts.last()?;
        if !self.p.wave64
            || phi.args.len() != 2
            || taken == not_taken
            || branch.name() != "s_cbranch_vccnz"
            || self.p.blocks[not_taken].preds.as_slice() != [join]
        {
            return None;
        }
        let dom = self.dominators.get_or_init(|| {
            crate::ir::idoms(
                &self.p.blocks,
                &crate::ir::reverse_postorder(&self.p.blocks),
                &self.rpo,
            )
        });
        let mut b = site.0;
        while b != not_taken {
            let parent = *dom.get(b)?;
            if parent == usize::MAX || parent == b {
                return None;
            }
            b = parent;
        }
        let pair = |i: &Inst, r: u8| Some([i.read(Reg::S(r))?, i.read(Reg::S(r + 1))?]);
        let def = |[lo, hi]: [Value; 2]| -> Option<&Inst> {
            let at = self.defs[lo.0 as usize]?;
            (self.defs[hi.0 as usize] == Some(at)).then(|| self.inst(at))
        };
        let regs = |i: &Inst| -> Option<(u8, u8)> {
            let Fields::Sop { src, .. } = &i.fields else {
                return None;
            };
            let [
                Operand::Scalar { code: a, dwords: 2 },
                Operand::Scalar { code: b, dwords: 2 },
            ] = src.as_slice()
            else {
                return None;
            };
            Some((*a, *b))
        };
        // VCC contains every active lane where (sentinel != -1). Some
        // variants OR in another rejection condition before masking by EXEC.
        let mask = def(pair(branch, VCC)?)?;
        let (exec, term) = regs(mask)?;
        if exec != EXEC {
            return None;
        }
        let active = pair(mask, EXEC)?;
        let compare = match mask.name() {
            "s_andn2_b64" => def(pair(mask, term)?)?,
            "s_and_b64" => {
                let or = def(pair(mask, term)?)?;
                if or.name() != "s_or_b64" {
                    return None;
                }
                let (a, b) = regs(or)?;
                [a, b].into_iter().find_map(|r| {
                    let xor = def(pair(or, r)?)?;
                    let Fields::Sop { src, .. } = &xor.fields else {
                        return None;
                    };
                    let [Operand::Scalar { code, dwords: 2 }, ones] = src.as_slice() else {
                        return None;
                    };
                    if xor.name() != "s_xor_b64" || self.term(xor, ones)? != Term::Const(u32::MAX) {
                        return None;
                    }
                    def(pair(xor, *code)?)
                })?
            }
            _ => return None,
        };
        if compare.name() != "v_cmp_eq_u32" || pair(compare, EXEC)? != active {
            return None;
        }
        let Fields::Valu(v) = &compare.fields else {
            return None;
        };
        let [ones, Operand::Vector { reg, .. }] = v.src.as_slice() else {
            return None;
        };
        if v.dpp.is_some()
            || v.sdwa.is_some()
            || v.neg != 0
            || v.abs != 0
            || v.opsel != 0
            || self.term(compare, ones)? != Term::Const(u32::MAX)
        {
            return None;
        }
        let (p, flag) = self.phis[compare.read(Reg::V(*reg))?.0 as usize]?;
        if p != join {
            return None;
        }
        let flags = &block.phis[flag];
        let skipped = flags.args.iter().position(|&value| {
            let Some(i) = self.def(value) else {
                return false;
            };
            let Fields::Valu(v) = &i.fields else {
                return false;
            };
            i.name() == "v_mov_b32"
                && pair(i, EXEC) == Some(active)
                && v.dpp.is_none()
                && v.sdwa.is_none()
                && v.neg == 0
                && v.abs == 0
                && v.opsel == 0
                && v.src.len() == 1
                && self.term(i, &v.src[0]) == Some(Term::Const(0))
        })?;
        // With an empty EXEC the scalar branch itself can fall through. All
        // real consumers must still have zero active lanes in that case.
        let lanes = self.lanes?;
        let outer = lanes.mask_bits(active[0], Some(active[1]))?;
        let mut seen = false;
        for d in &resources.descriptors {
            if !d
                .source
                .leaves()
                .iter()
                .any(|l| matches!(l, Source::Loaded {load,..} if *load == site))
            {
                continue;
            }
            let i = self.inst(d.site);
            if d.site.0 != site.0 || d.site.1 <= site.1 || !i.name().starts_with("image_") {
                return None;
            }
            let mask = pair(i, EXEC)?;
            if mask != active
                && self.meet(
                    lanes,
                    &[(lanes.mask_bits(mask[0], Some(mask[1]))?, true), (outer, false)],
                )
            {
                return None;
            }
            seen = true;
        }
        if !seen {
            return None;
        }
        let selected = phi.args[1 - skipped];
        if first_lane.is_some() {
            let reader = self.def(index)?;
            let copy = self.def(selected)?;
            if pair(reader, EXEC)? != active || pair(copy, EXEC)? != active || copy.name() != "v_mov_b32" {
                return None;
            }
            let Fields::Valu(op) = &copy.fields else {
                return None;
            };
            let [source @ Operand::Scalar { .. }] = op.src.as_slice() else {
                return None;
            };
            if op.dpp.is_some()
                || op.sdwa.is_some()
                || op.neg != 0
                || op.abs != 0
                || op.omod != 0
                || op.opsel != 0
                || op.clamp
            {
                return None;
            }
            let Term::Value(word) = self.term(copy, source)? else {
                return None;
            };
            Some(word)
        } else {
            Some(selected)
        }
    }

    pub(super) fn scalar_handle_program(&self, value: Value, resources: &Resources) -> Option<Vec<ScalarWordOp>> {
        let mut ops = Vec::new();
        let root = self.scalar_word(Term::Value(value), resources, &mut ops, 0)?;
        ops.truncate(usize::from(root) + 1);
        matches!(ops.last(), Some(ScalarWordOp::Read { .. })).then_some(ops)
    }

    fn scalar_word(
        &self,
        term: Term,
        resources: &Resources,
        ops: &mut Vec<ScalarWordOp>,
        depth: u8,
    ) -> Option<u8> {
        use ScalarWordOp::*;
        fn push(ops: &mut Vec<ScalarWordOp>, op: ScalarWordOp) -> Option<u8> {
            if let Some(at) = ops.iter().position(|v| *v == op) {
                return Some(at as u8);
            }
            if ops.len() >= 32 {
                return None;
            }
            ops.push(op);
            Some((ops.len() - 1) as u8)
        }
        if depth >= 18 {
            return None;
        }
        let v = match term {
            Term::Value(v) => v,
            Term::Const(k) => return push(ops, Constant(k)),
        };
        let site = self.defs[v.0 as usize]?;
        let i = self.inst(site);
        let child = |term, ops: &mut Vec<_>| self.scalar_word(term, resources, ops, depth + 1);
        let op = match &i.fields {
            Fields::Smem {
                data: Some(Operand::Scalar { code, dwords }),
                offset,
                soffset,
                ..
            } if i.name().starts_with("s_buffer_load_dword") => {
                let n = (0..*dwords).find(|&n| i.write(Reg::S(code + n)) == Some(v))?;
                let add = (*offset as u32).wrapping_add(u32::from(n) * 4);
                let mut address = child(self.term(i, soffset)?, ops)?;
                if add != 0 {
                    let offset = push(ops, Constant(add))?;
                    address = push(ops, Add(address, offset))?;
                }
                Read {
                    table: self.buffer_index_table(site, resources)?,
                    address,
                }
            }
            Fields::Sop { dst, src, imm, .. } if i.writes.first()?.1 == v => {
                let name = i.name();
                let (a, b) = if matches!(name, "s_mulk_i32" | "s_addk_i32") {
                    let Operand::Scalar { code, .. } = dst.as_ref()? else {
                        return None;
                    };
                    (
                        self.origin(i.read(Reg::S(*code))?),
                        Term::Const(i32::from((*imm)? as i16) as u32),
                    )
                } else {
                    let [a, b] = src.as_slice() else { return None };
                    (self.term(i, a)?, self.term(i, b)?)
                };
                let a = child(a, ops)?;
                let b = child(b, ops)?;
                match name {
                    "s_add_i32" | "s_add_u32" | "s_addk_i32" => Add(a, b),
                    "s_mul_i32" | "s_mulk_i32" => Multiply(a, b),
                    "s_and_b32" => And(a, b),
                    "s_or_b32" => Or(a, b),
                    "s_lshl_b32" => ShiftLeft(a, b),
                    "s_lshr_b32" => ShiftRight(a, b),
                    _ => return None,
                }
            }
            _ => return None,
        };
        push(ops, op)
    }
}
