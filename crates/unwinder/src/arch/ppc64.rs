//! ppc64-specific definitions of architecture-specific functions in Wasmtime.
//!
//! The Cranelift ppc64 backend's prologue establishes the frame record
//! `[FP] = previous FP, [FP+8] = return address`, with r31 as the frame
//! pointer and the caller's SP at `FP + 16` — the same shape as x86-64
//! and riscv64, and deliberately *not* the native ELFv2 back-chain
//! layout.

#[inline]
pub fn get_stack_pointer() -> usize {
    let stack_pointer: usize;
    unsafe {
        core::arch::asm!(
            "mr {}, 1",
            out(reg) stack_pointer,
            options(nostack, nomem),
        );
    }
    stack_pointer
}

pub unsafe fn get_next_older_pc_from_fp(fp: usize) -> usize {
    // The return address (moved from LR by the prologue) lives directly
    // above the saved frame pointer.
    unsafe { *(fp as *mut usize).offset(1) }
}

pub unsafe fn resume_to_exception_handler(
    pc: usize,
    sp: usize,
    fp: usize,
    payload1: usize,
    payload2: usize,
) -> ! {
    unsafe {
        core::arch::asm!(
            // r31 (the frame pointer) and r1 cannot be named as asm!
            // operands, so move into them from ordinary registers. The
            // handler's address goes through CTR; exception payloads are
            // in r3/r4, matching `exception_payload_regs` in the
            // Cranelift backend.
            "mtctr {pc}",
            "mr 1, {sp}",
            "mr 31, {fp}",
            "bctr",
            pc = in(reg) pc,
            sp = in(reg) sp,
            fp = in(reg) fp,
            in("r3") payload1,
            in("r4") payload2,
            options(nostack, nomem, noreturn),
        );
    }
}

// The current frame pointer points to the next older frame pointer.
pub const NEXT_OLDER_FP_FROM_FP_OFFSET: usize = 0;

// SP of caller is FP in callee plus the size of the FP/return-address pair.
pub const NEXT_OLDER_SP_FROM_FP_OFFSET: usize = 16;

pub fn assert_fp_is_aligned(fp: usize) {
    assert_eq!(fp % 16, 0, "stack should always be aligned to 16");
}
