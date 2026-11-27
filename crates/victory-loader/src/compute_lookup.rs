//! Supply the ordinary compute description as the fourth argument of the retail cache lookup.
const SITE: usize = 0x247fe25;
pub const TARGET: usize = 0x2480050;

/// The caller owns an unstarted, mapped image. A different call site keeps the title fallback.
pub unsafe fn install(base: usize, size: usize, wrapper: usize) -> bool {
    use windows_sys::Win32::System::{
        Diagnostics::Debug::FlushInstructionCache,
        Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect},
        Threading::GetCurrentProcess,
    };
    let signature = [
        0x4c, 0x89, 0x44, 0x24, 0x40, 0xc7, 0x44, 0x24, 0x58, 1, 0, 0, 0, 0x48, 0x85, 0xc9, 0x74,
        0x18, 0x48, 0x8b, 0x56, 0x18, 0xe8, 0x26, 0x02, 0, 0, 0x40, 0xb7, 1,
    ];
    if TARGET + 0x90 >= size
        || unsafe { std::slice::from_raw_parts((base + SITE - 22) as *const u8, signature.len()) }
            != signature
    {
        return false;
    }
    let Some(stub) = crate::console_insn::near(base + SITE) else {
        return false;
    };
    let Some(mut call) = crate::console_insn::jump(base + SITE, stub) else {
        return false;
    };
    call[0] = 0xe8;
    // The pushed return address shifts the caller's [rsp+40h] to [rsp+48h].
    // lea r9,[rsp+48h]; mov rax,wrapper; jmp rax. Tail call preserves the Win64 home area.
    let mut code = vec![0x4c, 0x8d, 0x4c, 0x24, 0x48, 0x48, 0xb8];
    code.extend_from_slice(&wrapper.to_le_bytes());
    code.extend_from_slice(&[0xff, 0xe0]);
    let at = (base + SITE) as *mut u8;
    let mut old = 0;
    unsafe {
        if VirtualProtect(at.cast(), 5, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return false;
        }
        std::ptr::copy_nonoverlapping(code.as_ptr(), stub as *mut u8, code.len());
        std::ptr::copy_nonoverlapping(call.as_ptr(), at, call.len());
        VirtualProtect(at.cast(), 5, old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), stub as *const _, code.len());
        FlushInstructionCache(GetCurrentProcess(), at.cast(), 5);
    }
    true
}

/// Install only at the retail calls that supply a cache node and its exact fallback request.
pub unsafe fn graphics(base: usize, size: usize, seed: usize, stream: usize) {
    use windows_sys::Win32::System::{
        Diagnostics::Debug::FlushInstructionCache,
        Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect},
        Threading::GetCurrentProcess,
    };
    let sites: [(usize, &[u8], usize, bool); 2] = [
        (
            0x2477578,
            &[
                0x48, 0x8b, 0x01, 0xc5, 0xf8, 0x77, 0xff, 0x90, 0xc8, 0x01, 0, 0,
            ],
            seed,
            false,
        ),
        (
            0x2479a87,
            &[0x8d, 0x58, 0x06, 0, 0, 0xff, 0x90, 0xb0, 0x02, 0, 0],
            stream,
            true,
        ),
    ];
    for &(rva, signature, _, _) in &sites {
        if rva + 6 > size
            || unsafe {
                std::slice::from_raw_parts(
                    (base + rva + 6 - signature.len()) as *const u8,
                    signature.len(),
                )
            } != signature
        {
            return;
        }
    }
    for (rva, _, wrapper, with_request) in sites {
        let at = base + rva;
        let Some(stub) = crate::console_insn::near(at) else {
            return;
        };
        let Some(mut call) = crate::console_insn::jump(at, stub) else {
            return;
        };
        call[0] = 0xe8;
        let mut code = Vec::new();
        if with_request {
            // Win64 home space plus the fifth argument, the request in RSI.
            code.extend_from_slice(&[0x48, 0x83, 0xec, 0x28, 0x48, 0x89, 0x74, 0x24, 0x20]);
        }
        code.extend_from_slice(&[0x48, 0xb8]);
        code.extend_from_slice(&wrapper.to_le_bytes());
        if with_request {
            code.extend_from_slice(&[0xff, 0xd0, 0x48, 0x83, 0xc4, 0x28, 0xc3]);
        } else {
            code.extend_from_slice(&[0xff, 0xe0]);
        }
        let mut old = 0;
        unsafe {
            if VirtualProtect(at as *const _, 6, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
                return;
            }
            std::ptr::copy_nonoverlapping(code.as_ptr(), stub as *mut u8, code.len());
            std::ptr::copy_nonoverlapping(call.as_ptr(), at as *mut u8, 5);
            ((at + 5) as *mut u8).write(0x90);
            VirtualProtect(at as *const _, 6, old, &mut old);
            FlushInstructionCache(GetCurrentProcess(), stub as *const _, code.len());
            FlushInstructionCache(GetCurrentProcess(), at as *const _, 6);
        }
    }
}
