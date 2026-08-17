// A WORD OF CAUTION
//
// This entire file basically needs to be kept in sync with itself. It's not
// really possible to modify just one bit of this file without understanding
// all the other bits. Documentation tries to reference various bits here and
// there but try to make sure to read over everything before tweaking things!
//
// The ppc64 (ELFv2, little-endian) save area layout, relative to r1 after
// the switch routine's `addi 1, 1, -0x1f0`:
//
//   0x000..0x0b0   v20-v31   (callee-saved vector registers, 16 bytes each)
//   0x0c0..0x148   f14-f31   (callee-saved float registers)
//   0x150..0x1d8   r14-r31   (callee-saved general registers)
//   0x1e0          CR        (fields cr2-cr4 are callee-saved; save it all)
//   0x1e8          LR        (the return address)
//
// and above that, at the very top of the stack, the two words reserved by
// unix.rs: the saved stack pointer at top-0x10, and the run result at
// top-0x8. The initial stack image written by `wasmtime_fiber_init` spans
// exactly 0x200 bytes.
//
// Vector saves use `stxvd2x`/`lxvd2x` (the only VSX memory forms in the
// POWER8 baseline; the D-form `stxv` is ISA 3.0). They are indexed-only,
// so each save loads its offset into r0 first — r0 reads as zero only in
// the RA position, and here it is RB. On little-endian these instructions
// store a doubleword-swapped image, but the load performs the same swap,
// so a save/restore round-trip is correct.

use core::arch::naked_asm;

#[inline(never)] // FIXME(rust-lang/rust#148307)
pub(crate) unsafe extern "C" fn wasmtime_fiber_switch(top_of_stack: *mut u8) {
    unsafe { wasmtime_fiber_switch_(top_of_stack) }
}

#[unsafe(naked)]
unsafe extern "C" fn wasmtime_fiber_switch_(top_of_stack: *mut u8 /* r3 */) {
    naked_asm!(
        "
      // We're switching to arbitrary code somewhere else, so pessimistically
      // assume that all callee-save registers are clobbered. This means we
      // need to save/restore all of them.
      mflr 0
      addi 1, 1, -0x1f0
      std 0, 0x1e8(1)
      mfcr 0
      std 0, 0x1e0(1)
      std 14, 0x150(1)
      std 15, 0x158(1)
      std 16, 0x160(1)
      std 17, 0x168(1)
      std 18, 0x170(1)
      std 19, 0x178(1)
      std 20, 0x180(1)
      std 21, 0x188(1)
      std 22, 0x190(1)
      std 23, 0x198(1)
      std 24, 0x1a0(1)
      std 25, 0x1a8(1)
      std 26, 0x1b0(1)
      std 27, 0x1b8(1)
      std 28, 0x1c0(1)
      std 29, 0x1c8(1)
      std 30, 0x1d0(1)
      std 31, 0x1d8(1)
      stfd 14, 0xc0(1)
      stfd 15, 0xc8(1)
      stfd 16, 0xd0(1)
      stfd 17, 0xd8(1)
      stfd 18, 0xe0(1)
      stfd 19, 0xe8(1)
      stfd 20, 0xf0(1)
      stfd 21, 0xf8(1)
      stfd 22, 0x100(1)
      stfd 23, 0x108(1)
      stfd 24, 0x110(1)
      stfd 25, 0x118(1)
      stfd 26, 0x120(1)
      stfd 27, 0x128(1)
      stfd 28, 0x130(1)
      stfd 29, 0x138(1)
      stfd 30, 0x140(1)
      stfd 31, 0x148(1)
      li 0, 0x00
      stxvd2x 52, 1, 0
      li 0, 0x10
      stxvd2x 53, 1, 0
      li 0, 0x20
      stxvd2x 54, 1, 0
      li 0, 0x30
      stxvd2x 55, 1, 0
      li 0, 0x40
      stxvd2x 56, 1, 0
      li 0, 0x50
      stxvd2x 57, 1, 0
      li 0, 0x60
      stxvd2x 58, 1, 0
      li 0, 0x70
      stxvd2x 59, 1, 0
      li 0, 0x80
      stxvd2x 60, 1, 0
      li 0, 0x90
      stxvd2x 61, 1, 0
      li 0, 0xa0
      stxvd2x 62, 1, 0
      li 0, 0xb0
      stxvd2x 63, 1, 0

      // Swap the saved stack pointer at top-0x10 with ours.
      ld 0, -0x10(3)
      std 1, -0x10(3)
      mr 1, 0

      // Restore all callee-saved registers from the other stack.
      li 0, 0x00
      lxvd2x 52, 1, 0
      li 0, 0x10
      lxvd2x 53, 1, 0
      li 0, 0x20
      lxvd2x 54, 1, 0
      li 0, 0x30
      lxvd2x 55, 1, 0
      li 0, 0x40
      lxvd2x 56, 1, 0
      li 0, 0x50
      lxvd2x 57, 1, 0
      li 0, 0x60
      lxvd2x 58, 1, 0
      li 0, 0x70
      lxvd2x 59, 1, 0
      li 0, 0x80
      lxvd2x 60, 1, 0
      li 0, 0x90
      lxvd2x 61, 1, 0
      li 0, 0xa0
      lxvd2x 62, 1, 0
      li 0, 0xb0
      lxvd2x 63, 1, 0
      lfd 14, 0xc0(1)
      lfd 15, 0xc8(1)
      lfd 16, 0xd0(1)
      lfd 17, 0xd8(1)
      lfd 18, 0xe0(1)
      lfd 19, 0xe8(1)
      lfd 20, 0xf0(1)
      lfd 21, 0xf8(1)
      lfd 22, 0x100(1)
      lfd 23, 0x108(1)
      lfd 24, 0x110(1)
      lfd 25, 0x118(1)
      lfd 26, 0x120(1)
      lfd 27, 0x128(1)
      lfd 28, 0x130(1)
      lfd 29, 0x138(1)
      lfd 30, 0x140(1)
      lfd 31, 0x148(1)
      ld 14, 0x150(1)
      ld 15, 0x158(1)
      ld 16, 0x160(1)
      ld 17, 0x168(1)
      ld 18, 0x170(1)
      ld 19, 0x178(1)
      ld 20, 0x180(1)
      ld 21, 0x188(1)
      ld 22, 0x190(1)
      ld 23, 0x198(1)
      ld 24, 0x1a0(1)
      ld 25, 0x1a8(1)
      ld 26, 0x1b0(1)
      ld 27, 0x1b8(1)
      ld 28, 0x1c0(1)
      ld 29, 0x1c8(1)
      ld 30, 0x1d0(1)
      ld 31, 0x1d8(1)
      ld 0, 0x1e0(1)
      mtcr 0
      ld 0, 0x1e8(1)
      mtlr 0
      addi 1, 1, 0x1f0
      blr
        ",
    );
}

pub(crate) unsafe fn wasmtime_fiber_init(
    top_of_stack: *mut u8,
    entry_point: extern "C" fn(*mut u8, *mut u8) -> *mut u8,
    entry_arg0: *mut u8,
) {
    // The fake save area consumed by the first switch to this fiber. The
    // switch's restore path reads r14 (entry point), r15 (its argument),
    // r16 (the switch routine, for the final switch away), r31 (the top
    // of stack, doubling as the frame-pointer slot) and LR (where `blr`
    // then transfers: the start trampoline below).
    #[repr(C)]
    struct InitialStack {
        vrs: [u128; 12],
        fprs: [u64; 18],
        gprs: [usize; 18],
        cr: usize,
        lr: usize,

        // unix.rs reserved space
        last_sp: usize,
        run_result: usize,
    }
    const _: () = assert!(size_of::<InitialStack>() == 0x200);

    unsafe {
        let initial_stack = top_of_stack.cast::<InitialStack>().sub(1);
        let mut gprs = [0; 18];
        gprs[14 - 14] = entry_point as *const () as usize;
        gprs[15 - 14] = entry_arg0 as usize;
        gprs[16 - 14] = wasmtime_fiber_switch_ as *const () as usize;
        gprs[31 - 14] = top_of_stack as usize;
        initial_stack.write(InitialStack {
            vrs: [0; 12],
            fprs: [0; 18],
            gprs,
            cr: 0,
            lr: wasmtime_fiber_start as *const () as usize,
            last_sp: initial_stack as usize,
            run_result: 0,
        });
    }
}

#[unsafe(naked)]
unsafe extern "C" fn wasmtime_fiber_start() -> ! {
    naked_asm!(
        "
      .cfi_startproc simple
      .cfi_def_cfa_offset 0

      // Entered via the switch routine's `blr` with r1 = top-0x10 (the
      // slot holding the original thread's saved stack pointer). ELFv2
      // callees store their return address into the *caller's* frame at
      // r1+16, which from here would land past the top of the stack, so
      // establish a minimum 32-byte frame first. `stdu` also writes the
      // back chain, which the CFA expression below follows: the original
      // stack pointer saved by the switch is two dereferences away, and
      // the switch's save area sits 0x1f0 above it.
      stdu 1, -32(1)

      .cfi_escape 0x0f, /* DW_CFA_def_cfa_expression */ \
          8,            /* the byte length of this expression */ \
          0x71, 0x00,   /* DW_OP_breg1 (r1) + 0 */ \
          0x06,         /* DW_OP_deref */ \
          0x06,         /* DW_OP_deref */ \
          0x0a, 0xf0, 0x01, /* DW_OP_const2u 0x1f0 */ \
          0x22          /* DW_OP_plus */

      // The switch routine saved the original thread's registers at fixed
      // offsets below its entry SP (= the CFA computed above). 65 is the
      // DWARF number for LR.
      .cfi_rel_offset 65, -0x8
      .cfi_rel_offset 31, -0x18
      .cfi_rel_offset 30, -0x20
      .cfi_rel_offset 29, -0x28
      .cfi_rel_offset 28, -0x30
      .cfi_rel_offset 27, -0x38
      .cfi_rel_offset 26, -0x40
      .cfi_rel_offset 25, -0x48
      .cfi_rel_offset 24, -0x50
      .cfi_rel_offset 23, -0x58
      .cfi_rel_offset 22, -0x60
      .cfi_rel_offset 21, -0x68
      .cfi_rel_offset 20, -0x70
      .cfi_rel_offset 19, -0x78
      .cfi_rel_offset 18, -0x80
      .cfi_rel_offset 17, -0x88
      .cfi_rel_offset 16, -0x90
      .cfi_rel_offset 15, -0x98
      .cfi_rel_offset 14, -0xa0

      // entry_point(entry_arg0, top_of_stack). Indirect calls into ELFv2
      // code must carry the callee's address in r12: the callee's global
      // entry point derives its TOC pointer from it.
      mr 3, 15
      mr 4, 31
      mr 12, 14
      mtctr 12
      bctrl

      // Switch back to the original stack, and should we ever be resumed
      // again, fall through to the trap word below.
      mr 3, 31
      mr 12, 16
      mtctr 12
      bctrl

      // The all-zeros word is a permanently invalid instruction.
      .long 0
      .cfi_endproc
        ",
    );
}
