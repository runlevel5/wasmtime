//! ppc64 ISA definitions: registers.
//!
//! Register conventions for this backend (ELFv2, little-endian):
//!
//! | Reg     | Role                                | Allocatable? |
//! |---------|-------------------------------------|--------------|
//! | r0      | Reads as zero when used as a base   | no (emission scratch) |
//! | r1      | Stack pointer                       | no |
//! | r2      | TOC pointer (reserved; this backend | no |
//! |         | is TOC-free but never touches r2)   |    |
//! | r3-r10  | Arguments / return values, volatile | yes (preferred) |
//! | r11     | Backend scratch (`spilltmp`)        | no |
//! | r12     | Indirect-call target (ELFv2 global- | yes (preferred) |
//! |         | entry convention); fixed at calls   |    |
//! | r13     | Thread pointer (TLS); never written | no |
//! | r14-r30 | Callee-saved                        | yes (non-preferred) |
//! | r31     | Frame pointer                       | no |
//! | f0-f13  | FP scratch / args, volatile         | yes (preferred) |
//! | f14-f31 | FP callee-saved                     | yes (non-preferred) |
//! | v0-v19  | Vector, volatile                    | yes (preferred) |
//! | v20-v31 | Vector, callee-saved                | yes (non-preferred) |
//!
//! LR and CTR are SPRs, not modelled by the register allocator: LR is
//! saved by the prologue and thereafter dead (calls and the LoadExtName
//! pseudo-inst may clobber it freely); CTR is only used momentarily by
//! indirect calls. Condition-register fields are likewise not allocated:
//! every compare-and-consume sequence lives inside a single MInst and
//! uses cr0.

use crate::machinst::{Reg, Writable};
use regalloc2::{PReg, RegClass, VReg};

#[inline]
pub const fn gpr(enc: usize) -> Reg {
    let p_reg = PReg::new(enc, RegClass::Int);
    let v_reg = VReg::new(p_reg.index(), p_reg.class());
    Reg::from_virtual_reg(v_reg)
}

pub const fn pgpr(enc: usize) -> PReg {
    PReg::new(enc, RegClass::Int)
}

#[inline]
pub fn fpr(enc: usize) -> Reg {
    let p_reg = PReg::new(enc, RegClass::Float);
    let v_reg = VReg::new(p_reg.index(), p_reg.class());
    Reg::from(v_reg)
}

pub const fn pfpr(enc: usize) -> PReg {
    PReg::new(enc, RegClass::Float)
}

pub const fn pvr(enc: usize) -> PReg {
    PReg::new(enc, RegClass::Vector)
}

/// r0: usable as a plain register in most contexts, but reads as literal
/// zero when used as the base of a load/store or as the RA operand of
/// `addi`/`addis`. Reserved for use as an emission-time scratch register
/// within single MInst expansions.
#[inline]
pub const fn zero_scratch_reg() -> Reg {
    gpr(0)
}

/// r1: the stack pointer.
#[inline]
pub const fn stack_reg() -> Reg {
    gpr(1)
}

#[inline]
pub fn writable_stack_reg() -> Writable<Reg> {
    Writable::from_reg(stack_reg())
}

/// r31: the frame pointer. The prologue establishes the frame record
/// `[FP] = previous FP, [FP+8] = return address`, matching what
/// Wasmtime's `crates/unwinder` walks.
#[inline]
pub const fn fp_reg() -> Reg {
    gpr(31)
}

#[inline]
pub fn writable_fp_reg() -> Writable<Reg> {
    Writable::from_reg(fp_reg())
}

/// r11: reserved backend scratch register.
#[inline]
pub const fn spilltmp_reg() -> Reg {
    gpr(11)
}

#[inline]
#[expect(dead_code, reason = "will be used by future lowerings")]
pub fn writable_spilltmp_reg() -> Writable<Reg> {
    Writable::from_reg(spilltmp_reg())
}

/// r12: indirect-call target register. The ELFv2 global-entry convention
/// requires r12 to hold the callee's entry address at an indirect call;
/// harmless for calls into our own (TOC-free) code.
#[inline]
pub const fn call_target_reg() -> Reg {
    gpr(12)
}

/// First integer argument/return register (r3).
#[inline]
pub const fn a0() -> Reg {
    gpr(3)
}

#[inline]
pub fn writable_a0() -> Writable<Reg> {
    Writable::from_reg(a0())
}

pub fn reg_name(reg: Reg) -> alloc::string::String {
    use alloc::format;
    use alloc::string::String;
    match reg.to_real_reg() {
        Some(real) => match real.class() {
            RegClass::Int => match real.hw_enc() {
                1 => String::from("sp"),
                31 => String::from("fp"),
                n => format!("r{n}"),
            },
            RegClass::Float => format!("f{}", real.hw_enc()),
            RegClass::Vector => format!("v{}", real.hw_enc()),
        },
        None => format!("{reg:?}"),
    }
}
