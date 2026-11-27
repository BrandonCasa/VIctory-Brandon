//! Schedule divergent logical wave64s on a host with wave32. Each lane
//! carries its original program counter and SSA registers around a uniform
//! sweep. A collective at an exchange boundary always includes the entire
//! group. A guest barrier parks its wave until the other live waves arrive.

use super::*;

const PARKED: u32 = 1 << 31;
const DONE: u32 = u32::MAX;
const PROLOGUE: usize = usize::MAX;

struct Part {
    original: usize,
    phis: Vec<u32>,
    body: Vec<u32>,
    barrier: Option<(V, bool)>,
    term: Option<Term>,
}

fn visit(blocks: &[Block], b: usize, seen: &mut [bool], order: &mut Vec<usize>) {
    if std::mem::replace(&mut seen[b], true) {
        return;
    }
    match blocks[b].term {
        Some(Term::Br(t)) => visit(blocks, t, seen, order),
        Some(Term::CondBr(_, a, c)) => {
            visit(blocks, c, seen, order);
            visit(blocks, a, seen, order);
        }
        _ => {}
    }
    order.push(b);
}

impl Module {
    // Values before a guest definition are never observed. A concrete zero
    // also keeps undefined operands out of the generated DXIL register loop.
    fn register_zero(&mut self, ty: Ty) -> Result<V, &'static str> {
        match self.types[ty.0 as usize] {
            TypeDef::Int(_) => Ok(self.int_const(ty, 0)),
            TypeDef::Float | TypeDef::Double => Ok(self.constant(ty, Const::Float(0))),
            _ => Err("a non-scalar logical wave register"),
        }
    }

    fn exchange_barrier(&self, i: u32) -> Option<(V, bool)> {
        let Op::Call(f, args) = &self.insts[i as usize].op else {
            return None;
        };
        if self.funcs[*f as usize].name != "dx.op.barrier" {
            return None;
        }
        let mode = *args.get(1)?;
        (self.int_value(mode)? & 1 != 0).then(|| (mode, self.guest_barriers.contains(&i)))
    }

    /// DXIL resource handles and pointers cannot be carried by a generated
    /// phi. Rematerialize a pure handle or address from its scalar inputs.
    fn rematerializable(&self, i: usize) -> bool {
        match &self.insts[i].op {
            Op::Gep(..) => true,
            Op::Call(f, _) => {
                let name = &self.funcs[*f as usize].name;
                name.starts_with("dx.op.createHandle") || name == "dx.op.annotateHandle"
            }
            _ => false,
        }
    }

    fn carry_value(
        &self,
        v: V,
        owners: &[usize],
        carry: &mut [bool],
        rebuild: &mut [bool],
    ) -> Result<(), &'static str> {
        let V::Inst(id) = v else { return Ok(()) };
        let i = id as usize;
        if owners[i] == PROLOGUE || carry[i] || rebuild[i] {
            return Ok(());
        }
        if matches!(
            self.types[self.insts[i].ty.0 as usize],
            TypeDef::Int(_) | TypeDef::Float | TypeDef::Double
        ) {
            carry[i] = true;
            return Ok(());
        }
        if !self.rematerializable(i) {
            return Err("a non-scalar value live across a logical wave boundary");
        }
        rebuild[i] = true;
        let mut operands = Vec::new();
        for_each_operand(&self.insts[i].op, |v| operands.push(v));
        for operand in operands {
            self.carry_value(operand, owners, carry, rebuild)?;
        }
        Ok(())
    }

    fn scheduled_value(&mut self, b: usize, v: V, values: &[V], rebuild: &[bool]) -> V {
        let V::Inst(id) = v else { return v };
        let i = id as usize;
        if !rebuild[i] {
            return values[i];
        }
        let Inst { ty, mut op } = self.insts[i].clone();
        opt::map_operands(&mut op, |v| self.scheduled_value(b, v, values, rebuild));
        self.push(b, ty, op)
    }

    fn scheduled_merge(&mut self, b: usize, executed: usize, skipped: usize, a: V, old: V) -> V {
        if a == old {
            return a;
        }
        let phi = self.phi(b, self.type_of(a));
        self.set_phi(phi, vec![(a, executed), (old, skipped)]);
        phi
    }

    pub(crate) fn schedule_waves(&mut self, flat_id: V, flag: V) -> Result<(), &'static str> {
        let Some(Term::Br(start)) = self.blocks[0].term else {
            return Err("a logical wave with a conditional prologue");
        };
        let Some(&barrier) = self.func_ids.get("dx.op.barrier") else {
            return Err("a logical wave without an exchange barrier");
        };
        let mut seen = vec![false; self.blocks.len()];
        let mut order = Vec::new();
        visit(&self.blocks, start, &mut seen, &mut order);
        if seen[0] {
            return Err("a logical wave with a backedge into its prologue");
        }
        let mut first = vec![0; self.blocks.len()];
        let mut parts = Vec::new();
        for original in order.into_iter().rev() {
            first[original] = parts.len();
            let block = &self.blocks[original];
            let mut part = Part {
                original,
                phis: block.phis.clone(),
                body: Vec::new(),
                barrier: None,
                term: None,
            };
            for &i in &block.body {
                if let Some(sync) = self.exchange_barrier(i) {
                    part.barrier = Some(sync);
                    parts.push(part);
                    part = Part {
                        original,
                        phis: Vec::new(),
                        body: Vec::new(),
                        barrier: None,
                        term: None,
                    };
                } else {
                    part.body.push(i);
                }
            }
            part.term = block.term;
            parts.push(part);
        }
        let n = self.insts.len();
        let mut owners = vec![PROLOGUE; n];
        for (b, part) in parts.iter().enumerate() {
            for &i in part.phis.iter().chain(&part.body) {
                owners[i as usize] = b;
            }
        }
        let mut carry = vec![false; n];
        let mut rebuild = vec![false; n];
        for (b, part) in parts.iter().enumerate() {
            let mut uses = Vec::new();
            for &i in &part.phis {
                if let Op::Phi(incoming) = &self.insts[i as usize].op {
                    uses.extend(
                        incoming
                            .iter()
                            .filter(|(_, p)| *p == 0 || seen[*p])
                            .map(|&(v, _)| v),
                    );
                }
            }
            for &i in &part.body {
                for_each_operand(&self.insts[i as usize].op, |v| {
                    if matches!(v, V::Inst(id) if owners[id as usize] != b) {
                        uses.push(v);
                    }
                });
            }
            if let Some(Term::CondBr(v, _, _)) = part.term
                && matches!(v, V::Inst(id) if owners[id as usize] != b)
            {
                uses.push(v);
            }
            for v in uses {
                self.carry_value(v, &owners, &mut carry, &mut rebuild)?;
            }
        }
        self.blocks.truncate(1);
        let header = self.new_block();
        self.terminate(0, Term::Br(header));
        let i32t = self.int(32);
        let zero = self.i32(0);
        let one = self.i32(1);
        let start_pc = self.i32(first[start] as u32);
        let finished = self.i32(DONE);
        let parked = self.i32(PARKED);
        let barrier_op = self.i32(80);
        let fence = self.i32(11);
        let sync = |m: &mut Self, b, mode| {
            m.call(b, V::Func(barrier), vec![barrier_op, mode]);
        };
        let pc_header = self.phi(header, i32t);
        let prev_header = self.phi(header, i32t);
        let (mut pc, mut prev) = (pc_header, prev_header);
        let mut values: Vec<_> = (0..n).map(|i| V::Inst(i as u32)).collect();
        let mut registers = Vec::new();
        for (i, &needed) in carry.iter().enumerate() {
            if needed {
                let ty = self.insts[i].ty;
                let phi = self.phi(header, ty);
                registers.push((i, phi, self.register_zero(ty)?));
                values[i] = phi;
            }
        }
        let mut cur = header;
        for (b, part) in parts.iter().enumerate() {
            let exe = self.new_block();
            let join = self.new_block();
            let which = self.i32(b as u32);
            let active = self.cmp(cur, pred::EQ, pc, which);
            self.terminate(cur, Term::CondBr(active, exe, join));

            // Parallel assignment on a phi edge, including loop-carried swaps.
            let mut written = Vec::new();
            let mut phis = Vec::new();
            for &id in &part.phis {
                let i = id as usize;
                let Op::Phi(incoming) = self.insts[i].op.clone() else {
                    unreachable!()
                };
                let mut v = self.register_zero(self.insts[i].ty)?;
                for (input, pred_block) in incoming {
                    if pred_block != 0 && !seen[pred_block] {
                        continue;
                    }
                    let from = self.i32(pred_block as u32);
                    let here = self.cmp(exe, pred::EQ, prev, from);
                    let input = self.scheduled_value(exe, input, &values, &rebuild);
                    v = self.select(exe, here, input, v);
                }
                phis.push((i, v));
            }
            for (i, v) in phis {
                if carry[i] {
                    written.push((i, values[i]));
                }
                values[i] = v;
            }
            for &id in &part.body {
                let i = id as usize;
                let Inst { ty, mut op } = self.insts[i].clone();
                opt::map_operands(&mut op, |v| self.scheduled_value(exe, v, &values, &rebuild));
                if carry[i] {
                    written.push((i, values[i]));
                }
                values[i] = self.push(exe, ty, op);
            }
            let next = match part.term {
                Some(Term::Br(t)) => self.i32(first[t] as u32),
                Some(Term::CondBr(c, a, z)) => {
                    let (a, z) = (self.i32(first[a] as u32), self.i32(first[z] as u32));
                    let c = self.scheduled_value(exe, c, &values, &rebuild);
                    self.select(exe, c, a, z)
                }
                Some(Term::Ret) => finished,
                None if part.barrier.is_some() => {
                    let park = part.barrier.is_some_and(|(_, guest)| guest);
                    self.i32((b as u32 + 1) | if park { PARKED } else { 0 })
                }
                None => return Err("an unterminated logical wave block"),
            };
            self.terminate(exe, Term::Br(join));
            pc = self.scheduled_merge(join, exe, cur, next, pc);
            let original = self.i32(part.original as u32);
            prev = self.scheduled_merge(join, exe, cur, original, prev);
            for (i, old) in written {
                values[i] = self.scheduled_merge(join, exe, cur, values[i], old);
            }
            if let Some((mode, false)) = part.barrier {
                sync(self, join, mode);
            }
            cur = join;
        }

        // All waves have reached the end of this sweep. Publish whether
        // any can still run, and release a guest barrier only when every
        // unfinished wave has parked. No lane exits the host loop early.
        sync(self, cur, fence);
        let clear = self.new_block();
        let poll = self.new_block();
        let first_lane = self.cmp(cur, pred::EQ, flat_id, zero);
        self.terminate(cur, Term::CondBr(first_lane, clear, poll));
        self.store(clear, flag, zero);
        self.terminate(clear, Term::Br(poll));
        sync(self, poll, fence);
        let done = self.cmp(poll, pred::EQ, pc, finished);
        let high = self.bin(poll, Bin::And, pc, parked);
        let waiting = self.cmp(poll, pred::NE, high, zero);
        let two = self.i32(2);
        let live = self.select(poll, waiting, one, two);
        let state = self.select(poll, done, zero, live);
        self.atomic(poll, Rmw::Or, flag, state);
        sync(self, poll, fence);
        let summary = self.load(poll, flag);
        let all_parked = self.cmp(poll, pred::EQ, summary, one);
        let mask = self.i32(!PARKED);
        let resume = self.bin(poll, Bin::And, pc, mask);
        let resume = self.select(poll, done, finished, resume);
        let pc_next = self.select(poll, all_parked, resume, pc);
        self.set_phi(pc_header, vec![(start_pc, 0), (pc_next, poll)]);
        self.set_phi(prev_header, vec![(zero, 0), (prev, poll)]);
        for (i, phi, initial) in registers {
            self.set_phi(phi, vec![(initial, 0), (values[i], poll)]);
        }
        let exit = self.new_block();
        let end = self.cmp(poll, pred::EQ, summary, zero);
        self.terminate(poll, Term::CondBr(end, exit, header));
        self.terminate(exit, Term::Ret);
        Ok(())
    }
}
