//! Identify the cooperative loop in old, unmarked host translations. Its
//! final block publishes each lane's done/parked state with a shared atomic
//! OR, synchronizes, reads the result, and branches to exit or back to BB1.
//! Direct wave32 shaders may use all the same intrinsics elsewhere.

use victory_shader::{Container, FourCC, dxil::DxilProgram};

pub(super) fn scheduler(dxil: &[u8]) -> bool {
    let Ok(container) = Container::parse(dxil) else {
        return false;
    };
    // The cooperative emitter declares a mandatory 32-lane host wave in PSV0.
    if container
        .part(FourCC::PSV0)
        .and_then(|p| p.data.get(20..28))
        != Some(&[32, 0, 0, 0, 32, 0, 0, 0])
    {
        return false;
    }
    let Some(part) = container.part(FourCC::DXIL) else {
        return false;
    };
    let Ok(program) = DxilProgram::parse(part.data) else {
        return false;
    };
    scheduler_bitcode(program.bitcode)
}

fn scheduler_bitcode(code: &[u8]) -> bool {
    if !code.starts_with(b"BC\xc0\xde") || !code.len().is_multiple_of(4) {
        return false;
    }
    // Our LLVM writer ends with the main FUNCTION_BLOCK and the MODULE_BLOCK.
    // Each of the final records below has at most six 32-bit operands, each
    // taking at most seven six-bit VBR chunks. Only inspect this bounded tail.
    let tail = &code[code.len().saturating_sub(384)..];
    (0..tail.len() * 8).any(|position| poll_tail(Bits { tail, position }).is_some())
}

struct Bits<'a> {
    tail: &'a [u8],
    position: usize,
}

impl Bits<'_> {
    fn take(&mut self, width: usize) -> Option<u32> {
        let i = self.position / 8;
        let word = u16::from(*self.tail.get(i)?)
            | (u16::from(self.tail.get(i + 1).copied().unwrap_or(0)) << 8);
        if self.position + width > self.tail.len() * 8 {
            return None;
        }
        let value = (word >> (self.position % 8)) & ((1 << width) - 1);
        self.position += width;
        Some(u32::from(value))
    }

    fn operand(&mut self) -> Option<u32> {
        let mut value = 0;
        for shift in (0..32).step_by(5) {
            let group = self.take(6)?;
            if shift == 30 && group > 3 {
                return None;
            }
            value |= (group & 31) << shift;
            if group < 32 {
                return Some(value);
            }
        }
        None
    }

    fn record<const N: usize>(&mut self, opcode: u32) -> Option<[u32; N]> {
        // LLVM 3.7's UNABBREV_RECORD with the function block's four-bit width.
        if self.take(4)? != 3 || self.operand()? != opcode || self.operand()? != N as u32 {
            return None;
        }
        let mut operands = [0; N];
        for op in &mut operands {
            *op = self.operand()?;
        }
        Some(operands)
    }
}

fn poll_tail(mut b: Bits<'_>) -> Option<()> {
    // Numbers naming values are relative to the next instruction result.
    let [flag, 1, 5, 0, 6, 1] = b.record(38)? else {
        return None;
    }; // atomicrmw or
    let [0, 32768, _, _, _, _] = b.record(34)? else {
        return None;
    }; // dx.op.barrier
    let [ptr, _, 3, 0] = b.record(20)? else {
        return None;
    }; // load from same flag
    if flag.checked_add(1)? != ptr {
        return None;
    }
    let [1, _, 32] = b.record(28)? else {
        return None;
    }; // summary == 1
    let [_, _, 10] = b.record(2)? else {
        return None;
    }; // pc & resume_mask
    b.record::<3>(29)?; // done ? finished : unparked pc
    b.record::<3>(29)?; // all parked ? resumed pc : pc
    let [5, _, 32] = b.record(28)? else {
        return None;
    }; // summary == 0
    let [_, 1, 1] = b.record(11)? else {
        return None;
    }; // exit or sweep header
    b.record::<0>(10)?; // ret void
    if b.take(4)? != 0 {
        return None;
    } // END_BLOCK function
    b.position = b.position.next_multiple_of(32);
    if b.take(3)? != 0 {
        return None;
    } // END_BLOCK module
    (b.position.next_multiple_of(32) == b.tail.len() * 8).then_some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Emit only the final function records, with deliberately unrelated SSA
    // identifiers. Earlier basic blocks and SSA numbering may vary by shader.
    fn poll(flag_read: u32, loop_target: u32) -> Vec<u8> {
        let mut bits = Vec::new();
        let put = |bits: &mut Vec<bool>, value: u32, n: usize| {
            bits.extend((0..n).map(|i| (value >> i) & 1 != 0));
        };
        let mut record = |opcode: u32, operands: &[u32]| {
            put(&mut bits, 3, 4);
            for mut value in [opcode, operands.len() as u32]
                .into_iter()
                .chain(operands.iter().copied())
            {
                while value >= 32 {
                    put(&mut bits, 32 | (value & 31), 6);
                    value >>= 5;
                }
                put(&mut bits, value, 6);
            }
        };
        record(38, &[731, 1, 5, 0, 6, 1]);
        record(34, &[0, 32768, 12, 16000, 176, 19]);
        record(20, &[flag_read, 4, 3, 0]);
        record(28, &[1, 3726, 32]);
        record(2, &[47, 61, 10]);
        record(29, &[71, 1, 11]);
        record(29, &[1, 64, 4]);
        record(28, &[5, 3742, 32]);
        record(11, &[81, loop_target, 1]);
        record(10, &[]);
        bits.resize((bits.len() + 4).next_multiple_of(32) + 32, false);
        let mut code = b"BC\xc0\xde".to_vec();
        code.extend(bits.chunks(8).map(|bits| {
            bits.iter()
                .enumerate()
                .fold(0, |v, (i, b)| v | (u8::from(*b) << i))
        }));
        code
    }

    #[test]
    fn only_the_shared_state_poll_that_loops_back_to_the_sweep_matches() {
        assert!(scheduler_bitcode(&poll(732, 1)));
        assert!(!scheduler_bitcode(&poll(733, 1)));
        assert!(!scheduler_bitcode(&poll(732, 2)));
    }
}
