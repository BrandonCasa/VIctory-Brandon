//! The title's descriptor tables copy whole 32-byte slots. Keep the tag and
//! both words of a published view together when our calls write, copy or
//! mirror those slots. The fourth word remains the title's.

use std::sync::Mutex;

static FALLBACK: Mutex<()> = Mutex::new(());

#[cfg(target_arch = "x86_64")]
#[inline]
fn wide() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let cpu = std::arch::x86_64::__cpuid(0);
        matches!(
            (cpu.ebx, cpu.edx, cpu.ecx),
            (0x756e6547, 0x49656e69, 0x6c65746e) | (0x68747541, 0x69746e65, 0x444d4163)
        ) && std::arch::is_x86_feature_detected!("avx")
    })
}

// Intel SDM vol. 3A, Guaranteed Atomic Operations, and AMD's AVX guarantee
// (GCC PR104688): aligned VEX.128 VMOVDQA is atomic on these processors. Spell the
// instruction out: the compiler can otherwise split a 12- or 16-byte copy.
#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn load(at: *const u8) -> std::arch::x86_64::__m128i {
    let data;
    unsafe {
        std::arch::asm!("vmovdqa {data}, [{at}]", data = out(xmm_reg) data, at = in(reg) at,
        options(nostack, preserves_flags, readonly))
    };
    data
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn store(at: *mut u8, data: std::arch::x86_64::__m128i) {
    unsafe {
        std::arch::asm!("vmovdqa [{at}], {data}", at = in(reg) at, data = in(xmm_reg) data,
        options(nostack, preserves_flags))
    };
}

/// The caller owns a readable descriptor slot, at least 16 bytes.
#[inline]
pub(super) unsafe fn read(at: *const u8) -> [u32; 3] {
    #[cfg(target_arch = "x86_64")]
    if (at as usize).is_multiple_of(16) && wide() {
        let [tag, lo, hi, _] = unsafe { std::mem::transmute::<_, [u32; 4]>(load(at)) };
        return [tag, lo, hi];
    }
    let _held = FALLBACK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { at.cast::<[u32; 3]>().read_unaligned() }
}

/// The caller owns a writable descriptor slot, at least 16 bytes.
#[inline]
pub(super) unsafe fn write(at: *mut u8, words: [u32; 3]) {
    #[cfg(target_arch = "x86_64")]
    if (at as usize).is_multiple_of(16) && wide() {
        let [tag, lo, hi] = words;
        let last = unsafe { at.add(12).cast::<u32>().read() };
        unsafe { store(at, std::mem::transmute::<[u32; 4], _>([tag, lo, hi, last])) };
        return;
    }
    let _held = FALLBACK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { at.cast::<[u32; 3]>().write_unaligned(words) };
}

/// The title owns these ranges. As with memmove, they may overlap.
pub(crate) unsafe fn copy(to: *mut u8, from: *const u8, len: usize) {
    #[cfg(target_arch = "x86_64")]
    if len.is_multiple_of(16) && wide() {
        let from_aligned = (from as usize).is_multiple_of(16);
        let to_aligned = (to as usize).is_multiple_of(16);
        let _held = (!(from_aligned && to_aligned))
            .then(|| FALLBACK.lock().unwrap_or_else(|e| e.into_inner()));
        let backward =
            (from as usize..(from as usize).saturating_add(len)).contains(&(to as usize));
        for i in 0..len / 16 {
            let offset = if backward { len - (i + 1) * 16 } else { i * 16 };
            unsafe {
                let src = from.add(offset);
                let dst = to.add(offset);
                let data = if from_aligned {
                    load(src)
                } else {
                    src.cast::<std::arch::x86_64::__m128i>().read_unaligned()
                };
                if to_aligned {
                    store(dst, data);
                } else {
                    dst.cast::<std::arch::x86_64::__m128i>()
                        .write_unaligned(data);
                }
            }
        }
        return;
    }
    let _held = FALLBACK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::ptr::copy(from, to, len) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_ranges_move_overlapping_and_unaligned_bytes() {
        #[repr(align(32))]
        struct Bytes([u8; 96]);
        for (from, to) in [(0, 16), (16, 0), (4, 16), (16, 4), (4, 8)] {
            let mut actual = Bytes(std::array::from_fn(|i| i as u8));
            let mut expected = actual.0;
            expected.copy_within(from..from + 48, to);
            let p = actual.0.as_mut_ptr();
            unsafe { copy(p.add(to), p.add(from), 48) };
            assert_eq!(actual.0, expected);
        }
    }

    #[test]
    fn published_and_copied_records_keep_the_key_whole() {
        // Keys from two different kinds of view. Mixing the table entry 8
        // with the upper bits of the placed address would point at 0xb00000008.
        let keys = [
            [super::super::VIEW_TAG, 8, 1],
            [super::super::VIEW_TAG, 0x67558000, 0x8000000b],
        ];
        #[repr(align(32))]
        struct Slots([u32; 16]);
        let mut slots = Slots([0xacc0ffee; 16]);
        let address = slots.0.as_mut_ptr() as usize;
        unsafe { write(address as *mut u8, keys[0]) };
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|s| {
            s.spawn(|| {
                barrier.wait();
                for i in 0..200_000 {
                    unsafe { write(address as *mut u8, keys[i % 2]) };
                }
            });
            barrier.wait();
            for _ in 0..200_000 {
                let copied = unsafe {
                    copy((address + 32) as *mut u8, address as *const u8, 32);
                    read((address + 32) as *const u8)
                };
                assert!(keys.contains(&copied), "mixed payload {copied:x?}");
            }
        });
        assert_eq!(slots.0[3], 0xacc0ffee);
        assert_eq!(slots.0[11], 0xacc0ffee);
        assert_eq!(slots.0[12..], slots.0[4..8]);
    }
}
