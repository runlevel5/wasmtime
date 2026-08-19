//! Implementation of the standard ppc64 ELFv2 ABI (little-endian).
//!
//! # Frame layout
//!
//! ```text
//!   (high addresses)
//!   +---------------------------+
//!   | caller's frame            |
//!   |  ... incoming stack args  | <- entry SP + 32 + n
//!   |  32-byte reserved header  | <- entry SP (both conventions keep the
//!   +---------------------------+    ELFv2 header in the argument area)
//!   | return address (from LR)  | <- FP + 8
//!   | previous FP               | <- FP = entry SP - 16
//!   +---------------------------+
//!   | clobbered callee-saves    |
//!   +---------------------------+
//!   | spill slots, stack slots  |
//!   +---------------------------+
//!   | outgoing args             |
//!   +---------------------------+ <- SP (16-aligned, per ELFv2)
//!   (low addresses)
//! ```
//!
//! The `(previous FP, return address)` pair is the frame record that
//! Wasmtime's `crates/unwinder` walks: `[FP] = old FP`, `[FP+8] = return
//! address`, and the caller's SP is `FP + 16`. This deviates from the
//! native ELFv2 convention (back-chain word at `0(SP)`, LR save in the
//! *caller's* frame at `SP+16`): JIT frames are walked via the FP chain
//! and native unwinders see correct DWARF CFI, so the back-chain is not
//! maintained. Revisit if native tooling interop turns out to want it.
//!
//! Stack arguments are addressed at `entry SP + 32 + offset` in both the
//! SystemV (ELFv2) and Tail conventions: keeping the ELFv2 32-byte
//! header (back-chain, CR, LR, TOC save doublewords) reserved in the
//! Tail convention too costs 32 bytes per frame with stack args but
//! keeps the two layouts identical.
//!
//! Register conventions are documented in `inst/regs.rs`.

use crate::CodegenResult;
use crate::ir;
use crate::ir::Signature;
use crate::ir::types::*;
use crate::isa;
use crate::isa::CallConv;
use crate::isa::ppc64::inst::*;
use crate::isa::ppc64::settings::Flags as Ppc64Flags;
use crate::isa::unwind::UnwindInst;
use crate::machinst::*;
use crate::settings;
use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::vec::Vec;
use regalloc2::{MachineEnv, PRegSet};
use smallvec::{SmallVec, smallvec};

/// Support for the ppc64 ABI from the callee side (within a function body).
pub(crate) type Ppc64Callee = Callee<Ppc64MachineDeps>;

/// ppc64-specific ABI behavior. This struct just serves as an
/// implementation point for the trait; it is never actually instantiated.
pub struct Ppc64MachineDeps;

impl IsaFlags for Ppc64Flags {}

/// Offset of the incoming stack-argument area from the entry SP: the
/// ELFv2 reserved header (back-chain, CR save, LR save, TOC save).
const STACK_ARG_BASE: u32 = 32;

impl ABIMachineSpec for Ppc64MachineDeps {
    type I = Inst;
    type F = Ppc64Flags;

    /// Limit for the size of argument and return-value areas on the
    /// stack: 128 MiB, matching the other backends.
    const STACK_ARG_RET_SIZE_LIMIT: u32 = 128 * 1024 * 1024;

    fn word_bits() -> u32 {
        64
    }

    /// ELFv2 requires 16-byte ("quadword") stack alignment.
    fn stack_align(_call_conv: isa::CallConv) -> u32 {
        16
    }

    fn compute_arg_locs(
        call_conv: isa::CallConv,
        flags: &settings::Flags,
        params: &[ir::AbiParam],
        args_or_rets: ArgsOrRets,
        add_ret_area_ptr: bool,
        mut args: ArgsAccumulator,
    ) -> CodegenResult<(u32, Option<usize>)> {
        assert_ne!(
            call_conv,
            isa::CallConv::Winch,
            "ppc64 does not support the 'winch' calling convention"
        );

        // Argument registers per ELFv2: ints in r3-r10, floats in
        // f1-f13. Return values in r3-r4 / f1-f2.
        //
        // For everything except our own tail convention the allocation is
        // *positional*, as ELFv2 requires for native interop: each
        // parameter owns a doubleword slot in the caller's parameter save
        // area starting at SP+32 (whether or not it is passed in a
        // register), an integer parameter uses the GPR corresponding to
        // its slot (r3 + slot index, while slots remain), and a float
        // parameter uses the next FPR but still consumes its slot, so a
        // later integer skips that GPR. Stack-passed parameters live at
        // their positional slot, NOT densely packed: a native callee
        // reads its ninth parameter at SP+32+64, unconditionally.
        //
        // The tail convention packs both register classes and the stack
        // densely instead, which is more efficient and private to code
        // this backend compiles.
        let positional = args_or_rets == ArgsOrRets::Args && call_conv != isa::CallConv::Tail;
        let (x_start, x_end, f_start, f_end, v_start, v_end) = match args_or_rets {
            ArgsOrRets::Args => (3, 10, 1, 13, 2, 13),
            ArgsOrRets::Rets => (3, 4, 1, 2, 2, 3),
        };
        let mut next_x_reg = x_start;
        let mut next_f_reg = f_start;
        let mut next_v_reg = v_start;
        let mut next_stack: u32 = 0;
        // Parameter doubleword index, for the positional scheme.
        let mut slot_idx: u32 = 0;

        let ret_area_ptr = if add_ret_area_ptr {
            assert!(ArgsOrRets::Args == args_or_rets);
            next_x_reg += 1;
            slot_idx += 1;
            Some(ABIArg::reg(
                gpr(x_start).to_real_reg().unwrap(),
                I64,
                ir::ArgumentExtension::None,
                ir::ArgumentPurpose::Normal,
            ))
        } else {
            None
        };

        for param in params {
            if let ir::ArgumentPurpose::StructArgument(_) = param.purpose {
                panic!(
                    "StructArgument parameters are not supported on ppc64. \
                     Use regular pointer arguments instead."
                );
            }

            let (rcs, reg_tys) = Inst::rc_for_type(&param.value_type)?;

            // An `i128` is simply two adjacent doubleword slots, low
            // half first; each half is allocated independently by the
            // loop below. ELFv2 packs `__int128` into whatever two
            // consecutive slots come next -- no quadword alignment, and
            // the pair may straddle the last GPR and the stack (GCC and
            // Clang both place the ninth doubleword of arguments in r10
            // and the tenth in the parameter save area, even when they
            // are halves of one `__int128`).
            debug_assert!(rcs.len() <= 2);

            // A vector parameter occupies a quadword-aligned pair of
            // doubleword slots and skips the corresponding GPRs -- the
            // opposite of `__int128`, which packs densely. (Verified
            // against GCC and Clang: in `(long, vector, long, ...)` the
            // vector's slots are 2-3, so the second long lands in r7.)
            // Vector *registers* are assigned by vector-parameter order,
            // v2 up, independent of GPR exhaustion, like the FPRs.
            let is_vector = rcs == [RegClass::Vector];
            if positional && is_vector {
                slot_idx = align_to(slot_idx, 2);
            }

            let mut slots = ABIArgSlotVec::new();
            for (rc, reg_ty) in rcs.iter().zip(reg_tys.iter()) {
                let next_reg = if positional {
                    match rc {
                        RegClass::Int if slot_idx < 8 => Some(gpr(3 + slot_idx as usize)),
                        // Floats use successive FPRs by float-parameter
                        // order, independent of the slot index. (Floats
                        // beyond f13 whose slot is still in r3-r10 would
                        // go in that GPR per ELFv2; that case cannot be
                        // represented here and falls to the stack slot,
                        // which no realistic signature reaches.)
                        RegClass::Float if next_f_reg <= f_end => {
                            let x = Some(fpr(next_f_reg));
                            next_f_reg += 1;
                            x
                        }
                        RegClass::Vector if next_v_reg <= v_end => {
                            let x = Some(vr(next_v_reg));
                            next_v_reg += 1;
                            x
                        }
                        _ => None,
                    }
                } else if (next_x_reg <= x_end) && *rc == RegClass::Int {
                    let x = Some(gpr(next_x_reg));
                    next_x_reg += 1;
                    x
                } else if (next_f_reg <= f_end) && *rc == RegClass::Float {
                    let x = Some(fpr(next_f_reg));
                    next_f_reg += 1;
                    x
                } else if (next_v_reg <= v_end) && *rc == RegClass::Vector {
                    let x = Some(vr(next_v_reg));
                    next_v_reg += 1;
                    x
                } else {
                    None
                };
                if let Some(reg) = next_reg {
                    slots.push(ABIArgSlot::Reg {
                        reg: reg.to_real_reg().unwrap(),
                        ty: *reg_ty,
                        extension: param.extension,
                    });
                } else {
                    if args_or_rets == ArgsOrRets::Rets && !flags.enable_multi_ret_implicit_sret() {
                        return Err(crate::CodegenError::Unsupported(
                            "Too many return values to fit in registers. \
                             Use a StructReturn argument instead. (#9510)"
                                .to_owned(),
                        ));
                    }

                    let offset = if positional {
                        // The parameter's own doubleword slot.
                        STACK_ARG_BASE + slot_idx * 8
                    } else {
                        // Keep the ELFv2 32-byte header reserved at the
                        // base of the outgoing-argument area (arguments
                        // only; the return area is a separate buffer).
                        if next_stack == 0 && args_or_rets == ArgsOrRets::Args {
                            next_stack = STACK_ARG_BASE;
                        }
                        let size = (reg_ty.bits() / 8).max(8);
                        debug_assert!(size.is_power_of_two());
                        next_stack = align_to(next_stack, size);
                        let off = next_stack;
                        next_stack += size;
                        off
                    };
                    slots.push(ABIArgSlot::Stack {
                        offset: offset as i64,
                        ty: *reg_ty,
                        extension: param.extension,
                    });
                }
                if positional {
                    slot_idx += if is_vector { 2 } else { 1 };
                }
            }
            args.push(ABIArg::Slots {
                slots,
                purpose: param.purpose,
            });
        }

        // Under the positional scheme, any stack-passed parameter implies
        // the parameter save area covers every slot from the first.
        if positional && slot_idx > 8 {
            next_stack = STACK_ARG_BASE + slot_idx * 8;
        }

        let pos = if let Some(ret_area_ptr) = ret_area_ptr {
            args.push_non_formal(ret_area_ptr);
            Some(args.args().len() - 1)
        } else {
            None
        };

        // An ELFv2 callee owns a 32-byte header at the bottom of its
        // *caller's* frame: the back-chain word, and the CR, LR and TOC
        // save doublewords. Native code writes there unconditionally --
        // saving LR at entry SP + 16 is the very first thing a gcc
        // prologue does -- so every call that may land in native code
        // must reserve the header even when no arguments are passed on
        // the stack. Without this, a callee's LR save lands on whatever
        // the calling frame keeps at SP + 0, e.g. its first spill slot.
        //
        // Our own tail-convention callees never write to the caller's
        // frame, so tail calls skip the reservation.
        if args_or_rets == ArgsOrRets::Args && call_conv != isa::CallConv::Tail {
            next_stack = next_stack.max(STACK_ARG_BASE);
        }

        next_stack = align_to(next_stack, Self::stack_align(call_conv));

        Ok((next_stack, pos))
    }

    fn gen_load_stack(mem: StackAMode, into_reg: Writable<Reg>, ty: Type) -> Inst {
        Inst::gen_load(into_reg, mem.into(), ty, MemFlagsData::trusted())
    }

    fn gen_store_stack(mem: StackAMode, from_reg: Reg, ty: Type) -> Inst {
        Inst::gen_store(mem.into(), from_reg, ty, MemFlagsData::trusted())
    }

    fn gen_move(to_reg: Writable<Reg>, from_reg: Reg, ty: Type) -> Inst {
        Inst::gen_move(to_reg, from_reg, ty)
    }

    fn gen_extend(
        to_reg: Writable<Reg>,
        from_reg: Reg,
        signed: bool,
        from_bits: u8,
        to_bits: u8,
    ) -> Inst {
        assert!(from_bits < to_bits);
        Inst::Extend {
            rd: to_reg,
            rn: from_reg,
            signed,
            from_bits,
            to_bits,
        }
    }

    fn get_ext_mode(
        _call_conv: isa::CallConv,
        specified: ir::ArgumentExtension,
    ) -> ir::ArgumentExtension {
        specified
    }

    fn gen_args(args: Vec<ArgPair>) -> Inst {
        Inst::Args { args }
    }

    fn gen_rets(rets: Vec<RetPair>) -> Inst {
        Inst::Rets { rets }
    }

    fn get_stacklimit_reg(_call_conv: isa::CallConv) -> Reg {
        spilltmp_reg()
    }

    fn gen_add_imm(
        _call_conv: isa::CallConv,
        into_reg: Writable<Reg>,
        from_reg: Reg,
        imm: u32,
    ) -> SmallInstVec<Inst> {
        // LoadAddr handles both the addi-range and the materialize-into-r0
        // cases at emission time.
        smallvec![Inst::LoadAddr {
            rd: into_reg,
            mem: AMode::RegOffset(from_reg, i64::from(imm)),
        }]
    }

    fn gen_stack_lower_bound_trap(limit_reg: Reg) -> SmallInstVec<Inst> {
        smallvec![Inst::TrapIf {
            kind: IntegerCompare {
                kind: IntCC::UnsignedLessThan,
                rs1: stack_reg(),
                rs2: limit_reg,
                is_64: true,
            },
            trap_code: ir::TrapCode::STACK_OVERFLOW,
        }]
    }

    fn gen_get_stack_addr(mem: StackAMode, into_reg: Writable<Reg>) -> Inst {
        Inst::LoadAddr {
            rd: into_reg,
            mem: mem.into(),
        }
    }

    fn gen_load_base_offset(into_reg: Writable<Reg>, base: Reg, offset: i32, ty: Type) -> Inst {
        Inst::gen_load(
            into_reg,
            AMode::RegOffset(base, offset.into()),
            ty,
            MemFlagsData::trusted(),
        )
    }

    fn gen_store_base_offset(base: Reg, offset: i32, from_reg: Reg, ty: Type) -> Inst {
        Inst::gen_store(
            AMode::RegOffset(base, offset.into()),
            from_reg,
            ty,
            MemFlagsData::trusted(),
        )
    }

    fn gen_sp_reg_adjust(amount: i32) -> SmallInstVec<Inst> {
        if amount == 0 {
            return smallvec![];
        }
        smallvec![Inst::LoadAddr {
            rd: writable_stack_reg(),
            mem: AMode::SPOffset(amount.into()),
        }]
    }

    fn gen_prologue_frame_setup(
        _call_conv: isa::CallConv,
        flags: &settings::Flags,
        _isa_flags: &Ppc64Flags,
        frame_layout: &FrameLayout,
    ) -> SmallInstVec<Inst> {
        let mut insts = SmallVec::new();

        if frame_layout.setup_area_size > 0 {
            // mflr r0           ;; return address to the emission scratch
            // addi sp, sp, -16  ;; allocate the frame record
            // std  r0, 8(sp)    ;; save return address
            // std  fp, 0(sp)    ;; save previous FP
            // mr   fp, sp       ;; establish the new frame record
            insts.push(Inst::Mflr {
                rd: Writable::from_reg(zero_scratch_reg()),
            });
            insts.extend(Self::gen_sp_reg_adjust(-16));
            insts.push(Inst::gen_store(
                AMode::SPOffset(8),
                zero_scratch_reg(),
                I64,
                MemFlagsData::trusted(),
            ));
            insts.push(Inst::gen_store(
                AMode::SPOffset(0),
                fp_reg(),
                I64,
                MemFlagsData::trusted(),
            ));

            if flags.unwind_info() {
                insts.push(Inst::Unwind {
                    inst: UnwindInst::PushFrameRegs {
                        offset_upward_to_caller_sp: frame_layout.setup_area_size,
                    },
                });
            }
            insts.push(Inst::Mov {
                rd: writable_fp_reg(),
                rm: stack_reg(),
                ty: I64,
            });
        }

        insts
    }

    /// Reverse of `gen_prologue_frame_setup`.
    fn gen_epilogue_frame_restore(
        call_conv: isa::CallConv,
        _flags: &settings::Flags,
        _isa_flags: &Ppc64Flags,
        frame_layout: &FrameLayout,
    ) -> SmallInstVec<Inst> {
        let mut insts = SmallVec::new();

        if frame_layout.setup_area_size > 0 {
            insts.push(Inst::gen_load(
                Writable::from_reg(zero_scratch_reg()),
                AMode::SPOffset(8),
                I64,
                MemFlagsData::trusted(),
            ));
            insts.push(Inst::Mtlr {
                rs: zero_scratch_reg(),
            });
            insts.push(Inst::gen_load(
                writable_fp_reg(),
                AMode::SPOffset(0),
                I64,
                MemFlagsData::trusted(),
            ));
            insts.extend(Self::gen_sp_reg_adjust(16));
        }

        if call_conv == isa::CallConv::Tail && frame_layout.tail_args_size > 0 {
            insts.extend(Self::gen_sp_reg_adjust(
                frame_layout.tail_args_size.try_into().unwrap(),
            ));
        }

        insts
    }

    fn gen_return(
        _call_conv: isa::CallConv,
        _isa_flags: &Ppc64Flags,
        _frame_layout: &FrameLayout,
    ) -> SmallInstVec<Inst> {
        smallvec![Inst::Ret]
    }

    fn gen_probestack(insts: &mut SmallInstVec<Self::I>, frame_size: u32) {
        insts.push(Inst::LoadConst64 {
            rd: writable_a0(),
            imm: u64::from(frame_size),
        });
        let mut info = CallInfo::empty(
            ir::ExternalName::LibCall(ir::LibCall::Probestack),
            CallConv::SystemV,
        );
        info.uses.push(CallArgPair {
            vreg: a0(),
            preg: a0(),
        });
        // This call happens during the prologue, before any outgoing-args
        // area exists, and a native callee stores its LR/CR/TOC saves into
        // the 32 bytes above its entry SP. Give it a scratch header so
        // those writes cannot land on the frame record just established.
        insts.extend(Self::gen_sp_reg_adjust(-(STACK_ARG_BASE as i32)));
        insts.push(Inst::Call {
            info: Box::new(info),
        });
        insts.extend(Self::gen_sp_reg_adjust(STACK_ARG_BASE as i32));
    }

    fn gen_inline_probestack(
        insts: &mut SmallInstVec<Self::I>,
        _call_conv: isa::CallConv,
        frame_size: u32,
        guard_size: u32,
    ) {
        // Number of guard-size regions we need to touch. Rounds down: the
        // remainder is covered by the frame's own first access.
        let probe_count = frame_size / guard_size;

        // Unrolled probes: move SP down one guard at a time and store a
        // word at the new SP (valgrind objects to writes below SP).
        for _ in 0..probe_count {
            insts.extend(Self::gen_sp_reg_adjust(-(guard_size as i32)));
            insts.push(Inst::gen_store(
                AMode::SPOffset(0),
                zero_scratch_reg(),
                I32,
                MemFlagsData::trusted(),
            ));
        }
        if probe_count > 0 {
            insts.extend(Self::gen_sp_reg_adjust((guard_size * probe_count) as i32));
        }
    }

    fn gen_clobber_save(
        _call_conv: isa::CallConv,
        flags: &settings::Flags,
        frame_layout: &FrameLayout,
    ) -> SmallVec<[Inst; 16]> {
        let mut insts = SmallVec::new();
        let setup_frame = frame_layout.setup_area_size > 0;

        let incoming_args_diff = frame_layout.tail_args_size - frame_layout.incoming_args_size;
        if incoming_args_diff > 0 {
            // Decrement SP by the amount of additional incoming argument
            // space we need, and re-establish the frame record below it.
            insts.extend(Self::gen_sp_reg_adjust(-(incoming_args_diff as i32)));

            if setup_frame {
                insts.push(Inst::gen_load(
                    Writable::from_reg(zero_scratch_reg()),
                    AMode::SPOffset(i64::from(incoming_args_diff) + 8),
                    I64,
                    MemFlagsData::trusted(),
                ));
                insts.push(Inst::gen_store(
                    AMode::SPOffset(8),
                    zero_scratch_reg(),
                    I64,
                    MemFlagsData::trusted(),
                ));
                insts.push(Inst::gen_load(
                    writable_fp_reg(),
                    AMode::SPOffset(i64::from(incoming_args_diff)),
                    I64,
                    MemFlagsData::trusted(),
                ));
                insts.push(Inst::gen_store(
                    AMode::SPOffset(0),
                    fp_reg(),
                    I64,
                    MemFlagsData::trusted(),
                ));
                insts.push(Inst::gen_move(writable_fp_reg(), stack_reg(), I64));
            }
        }

        if flags.unwind_info() && setup_frame {
            // The *unwind* frame (but not the actual frame) starts at the
            // clobbers, just below the saved FP/return-address pair.
            insts.push(Inst::Unwind {
                inst: UnwindInst::DefineNewFrame {
                    offset_downward_to_clobbers: frame_layout.clobber_size,
                    offset_upward_to_caller_sp: frame_layout.setup_area_size,
                },
            });
        }

        // Adjust the stack pointer downward for clobbers, the function
        // fixed frame (spillslots and storage slots), and outgoing args.
        let stack_size = frame_layout.clobber_size
            + frame_layout.fixed_frame_storage_size
            + frame_layout.outgoing_args_size;

        if stack_size > 0 {
            insts.extend(Self::gen_sp_reg_adjust(-(stack_size as i32)));

            let mut cur_offset = 0;
            for reg in &frame_layout.clobbered_callee_saves {
                let r_reg = reg.to_reg();
                let ty = match r_reg.class() {
                    RegClass::Int => I64,
                    RegClass::Float => F64,
                    RegClass::Vector => I8X16,
                };
                cur_offset = align_to(cur_offset, ty.bytes());
                insts.push(Inst::gen_store(
                    AMode::SPOffset(i64::from(stack_size - cur_offset - ty.bytes())),
                    Reg::from(reg.to_reg()),
                    ty,
                    MemFlagsData::trusted(),
                ));

                if flags.unwind_info() {
                    insts.push(Inst::Unwind {
                        inst: UnwindInst::SaveReg {
                            clobber_offset: frame_layout.clobber_size - cur_offset - ty.bytes(),
                            reg: r_reg,
                        },
                    });
                }

                cur_offset += ty.bytes();
                assert!(cur_offset <= stack_size);
            }
        }
        insts
    }

    fn gen_clobber_restore(
        _call_conv: isa::CallConv,
        _flags: &settings::Flags,
        frame_layout: &FrameLayout,
    ) -> SmallVec<[Inst; 16]> {
        let mut insts = SmallVec::new();

        let stack_size = frame_layout.clobber_size
            + frame_layout.fixed_frame_storage_size
            + frame_layout.outgoing_args_size;
        let mut cur_offset = 0;

        for reg in &frame_layout.clobbered_callee_saves {
            let rreg = reg.to_reg();
            let ty = match rreg.class() {
                RegClass::Int => I64,
                RegClass::Float => F64,
                RegClass::Vector => I8X16,
            };
            cur_offset = align_to(cur_offset, ty.bytes());
            insts.push(Inst::gen_load(
                reg.map(Reg::from),
                AMode::SPOffset(i64::from(stack_size - cur_offset - ty.bytes())),
                ty,
                MemFlagsData::trusted(),
            ));
            cur_offset += ty.bytes();
        }

        if stack_size > 0 {
            insts.extend(Self::gen_sp_reg_adjust(stack_size as i32));
        }

        insts
    }

    fn gen_memcpy<F: FnMut(Type) -> Writable<Reg>>(
        call_conv: isa::CallConv,
        dst: Reg,
        src: Reg,
        size: usize,
        mut alloc_tmp: F,
    ) -> SmallVec<[Self::I; 8]> {
        let mut insts = SmallVec::new();
        let arg0 = gpr(3);
        let arg1 = gpr(4);
        let arg2 = gpr(5);
        let tmp = alloc_tmp(Self::word_type());
        insts.push(Inst::LoadConst64 {
            rd: tmp,
            imm: size as u64,
        });
        insts.push(Inst::Call {
            info: Box::new(CallInfo {
                dest: ir::ExternalName::LibCall(ir::LibCall::Memcpy),
                uses: smallvec![
                    CallArgPair {
                        vreg: dst,
                        preg: arg0
                    },
                    CallArgPair {
                        vreg: src,
                        preg: arg1
                    },
                    CallArgPair {
                        vreg: tmp.to_reg(),
                        preg: arg2
                    }
                ],
                defs: smallvec![],
                clobbers: Self::get_regs_clobbered_by_call(call_conv, false),
                caller_conv: call_conv,
                callee_conv: call_conv,
                callee_pop_size: 0,
                try_call_info: None,
                patchable: false,
            }),
        });
        insts
    }

    fn get_number_of_spillslots_for_value(
        rc: RegClass,
        _target_vector_bytes: u32,
        _isa_flags: &Ppc64Flags,
    ) -> u32 {
        // We allocate in terms of 8-byte slots.
        match rc {
            RegClass::Int => 1,
            RegClass::Float => 1,
            RegClass::Vector => 2,
        }
    }

    fn get_machine_env(_flags: &settings::Flags, _call_conv: isa::CallConv) -> &MachineEnv {
        static MACHINE_ENV: MachineEnv = create_reg_environment();
        &MACHINE_ENV
    }

    fn get_regs_clobbered_by_call(
        call_conv_of_callee: isa::CallConv,
        is_exception: bool,
    ) -> PRegSet {
        match call_conv_of_callee {
            isa::CallConv::Tail if is_exception => ALL_CLOBBERS,
            isa::CallConv::PreserveAll if is_exception => ALL_CLOBBERS,
            isa::CallConv::PreserveAll => NO_CLOBBERS,
            _ => DEFAULT_CLOBBERS,
        }
    }

    fn compute_frame_layout(
        call_conv: isa::CallConv,
        flags: &settings::Flags,
        _sig: &Signature,
        regs: &[Writable<RealReg>],
        function_calls: FunctionCalls,
        incoming_args_size: u32,
        tail_args_size: u32,
        stackslots_size: u32,
        fixed_frame_storage_size: u32,
        outgoing_args_size: u32,
    ) -> FrameLayout {
        let is_callee_saved = |reg: &Writable<RealReg>| match call_conv {
            isa::CallConv::PreserveAll => true,
            _ => DEFAULT_CALLEE_SAVES.contains(reg.to_reg().into()),
        };
        let mut regs: Vec<Writable<RealReg>> =
            regs.iter().cloned().filter(is_callee_saved).collect();
        regs.sort_unstable();

        let clobber_size = compute_clobber_size(&regs);

        let setup_area_size = if flags.preserve_frame_pointers()
            || function_calls != FunctionCalls::None
            // The function arguments that are passed on the stack are
            // addressed relative to the Frame Pointer.
            || incoming_args_size > 0
            || clobber_size > 0
            || fixed_frame_storage_size > 0
        {
            16 // FP, return address
        } else {
            0
        };

        FrameLayout {
            word_bytes: 8,
            incoming_args_size,
            tail_args_size,
            setup_area_size,
            clobber_size,
            fixed_frame_storage_size,
            stackslots_size,
            outgoing_args_size,
            clobbered_callee_saves: regs,
            function_calls,
        }
    }

    fn retval_temp_reg(_call_conv_of_callee: isa::CallConv) -> Writable<Reg> {
        // r10: clobbered at calls, never a return value register.
        Writable::from_reg(gpr(10))
    }

    fn exception_payload_regs(call_conv: isa::CallConv) -> &'static [Reg] {
        const PAYLOAD_REGS: &'static [Reg] = &[gpr(3), gpr(4)];
        match call_conv {
            isa::CallConv::SystemV | isa::CallConv::Tail | isa::CallConv::PreserveAll => {
                PAYLOAD_REGS
            }
            _ => &[],
        }
    }
}

/// Callee-saved registers per ELFv2: r14-r31, f14-f31 and v20-v31.
/// r2 (TOC) and r13 (thread pointer) are dedicated registers that this
/// backend never writes, so they are preserved for free and
/// deliberately not listed.
const DEFAULT_CALLEE_SAVES: PRegSet = PRegSet::empty()
    .with(pgpr(14))
    .with(pgpr(15))
    .with(pgpr(16))
    .with(pgpr(17))
    .with(pgpr(18))
    .with(pgpr(19))
    .with(pgpr(20))
    .with(pgpr(21))
    .with(pgpr(22))
    .with(pgpr(23))
    .with(pgpr(24))
    .with(pgpr(25))
    .with(pgpr(26))
    .with(pgpr(27))
    .with(pgpr(28))
    .with(pgpr(29))
    .with(pgpr(30))
    .with(pgpr(31))
    .with(pfpr(14))
    .with(pfpr(15))
    .with(pfpr(16))
    .with(pfpr(17))
    .with(pfpr(18))
    .with(pfpr(19))
    .with(pfpr(20))
    .with(pfpr(21))
    .with(pfpr(22))
    .with(pfpr(23))
    .with(pfpr(24))
    .with(pfpr(25))
    .with(pfpr(26))
    .with(pfpr(27))
    .with(pfpr(28))
    .with(pfpr(29))
    .with(pfpr(30))
    .with(pfpr(31))
    .with(pvr(20))
    .with(pvr(21))
    .with(pvr(22))
    .with(pvr(23))
    .with(pvr(24))
    .with(pvr(25))
    .with(pvr(26))
    .with(pvr(27))
    .with(pvr(28))
    .with(pvr(29))
    .with(pvr(30))
    .with(pvr(31));

fn compute_clobber_size(clobbers: &[Writable<RealReg>]) -> u32 {
    let mut clobbered_size = 0;
    for reg in clobbers {
        match reg.to_reg().class() {
            RegClass::Int | RegClass::Float => {
                clobbered_size += 8;
            }
            RegClass::Vector => {
                clobbered_size = align_to(clobbered_size, 16);
                clobbered_size += 16;
            }
        }
    }
    align_to(clobbered_size, 16)
}

/// Volatile registers per ELFv2: r0, r3-r12, f0-f13, v0-v19, plus LR and
/// CTR (which aren't allocatable and so don't appear here).
const DEFAULT_CLOBBERS: PRegSet = PRegSet::empty()
    .with(pgpr(0))
    .with(pgpr(3))
    .with(pgpr(4))
    .with(pgpr(5))
    .with(pgpr(6))
    .with(pgpr(7))
    .with(pgpr(8))
    .with(pgpr(9))
    .with(pgpr(10))
    .with(pgpr(11))
    .with(pgpr(12))
    .with(pfpr(0))
    .with(pfpr(1))
    .with(pfpr(2))
    .with(pfpr(3))
    .with(pfpr(4))
    .with(pfpr(5))
    .with(pfpr(6))
    .with(pfpr(7))
    .with(pfpr(8))
    .with(pfpr(9))
    .with(pfpr(10))
    .with(pfpr(11))
    .with(pfpr(12))
    .with(pfpr(13))
    .with(pvr(0))
    .with(pvr(1))
    .with(pvr(2))
    .with(pvr(3))
    .with(pvr(4))
    .with(pvr(5))
    .with(pvr(6))
    .with(pvr(7))
    .with(pvr(8))
    .with(pvr(9))
    .with(pvr(10))
    .with(pvr(11))
    .with(pvr(12))
    .with(pvr(13))
    .with(pvr(14))
    .with(pvr(15))
    .with(pvr(16))
    .with(pvr(17))
    .with(pvr(18))
    .with(pvr(19));

/// Everything except SP (r1), the TOC (r2), and the thread pointer (r13).
const ALL_CLOBBERS: PRegSet = PRegSet::empty()
    .with(pgpr(0))
    .with(pgpr(3))
    .with(pgpr(4))
    .with(pgpr(5))
    .with(pgpr(6))
    .with(pgpr(7))
    .with(pgpr(8))
    .with(pgpr(9))
    .with(pgpr(10))
    .with(pgpr(11))
    .with(pgpr(12))
    .with(pgpr(14))
    .with(pgpr(15))
    .with(pgpr(16))
    .with(pgpr(17))
    .with(pgpr(18))
    .with(pgpr(19))
    .with(pgpr(20))
    .with(pgpr(21))
    .with(pgpr(22))
    .with(pgpr(23))
    .with(pgpr(24))
    .with(pgpr(25))
    .with(pgpr(26))
    .with(pgpr(27))
    .with(pgpr(28))
    .with(pgpr(29))
    .with(pgpr(30))
    .with(pgpr(31))
    .with(pfpr(0))
    .with(pfpr(1))
    .with(pfpr(2))
    .with(pfpr(3))
    .with(pfpr(4))
    .with(pfpr(5))
    .with(pfpr(6))
    .with(pfpr(7))
    .with(pfpr(8))
    .with(pfpr(9))
    .with(pfpr(10))
    .with(pfpr(11))
    .with(pfpr(12))
    .with(pfpr(13))
    .with(pfpr(14))
    .with(pfpr(15))
    .with(pfpr(16))
    .with(pfpr(17))
    .with(pfpr(18))
    .with(pfpr(19))
    .with(pfpr(20))
    .with(pfpr(21))
    .with(pfpr(22))
    .with(pfpr(23))
    .with(pfpr(24))
    .with(pfpr(25))
    .with(pfpr(26))
    .with(pfpr(27))
    .with(pfpr(28))
    .with(pfpr(29))
    .with(pfpr(30))
    .with(pfpr(31))
    .with(pvr(0))
    .with(pvr(1))
    .with(pvr(2))
    .with(pvr(3))
    .with(pvr(4))
    .with(pvr(5))
    .with(pvr(6))
    .with(pvr(7))
    .with(pvr(8))
    .with(pvr(9))
    .with(pvr(10))
    .with(pvr(11))
    .with(pvr(12))
    .with(pvr(13))
    .with(pvr(14))
    .with(pvr(15))
    .with(pvr(16))
    .with(pvr(17))
    .with(pvr(18))
    .with(pvr(19))
    .with(pvr(20))
    .with(pvr(21))
    .with(pvr(22))
    .with(pvr(23))
    .with(pvr(24))
    .with(pvr(25))
    .with(pvr(26))
    .with(pvr(27))
    .with(pvr(28))
    .with(pvr(29))
    .with(pvr(30))
    .with(pvr(31));

const NO_CLOBBERS: PRegSet = PRegSet::empty();

const fn create_reg_environment() -> MachineEnv {
    // Prefer volatile registers (no save/restore cost), then fall back
    // to callee-saved ones. Non-allocatable: r0 (reads-as-zero quirk,
    // emission scratch), r1 (SP), r2 (TOC), r11 (spilltmp), r13 (thread
    // pointer), r31 (FP). r12 is allocatable but is additionally the
    // fixed indirect-call target register (regalloc2 requires fixed-use
    // registers to be allocatable).
    let preferred_regs_by_class: [PRegSet; 3] = [
        PRegSet::empty()
            .with(pgpr(3))
            .with(pgpr(4))
            .with(pgpr(5))
            .with(pgpr(6))
            .with(pgpr(7))
            .with(pgpr(8))
            .with(pgpr(9))
            .with(pgpr(10))
            .with(pgpr(12)),
        PRegSet::empty()
            .with(pfpr(0))
            .with(pfpr(1))
            .with(pfpr(2))
            .with(pfpr(3))
            .with(pfpr(4))
            .with(pfpr(5))
            .with(pfpr(6))
            .with(pfpr(7))
            .with(pfpr(8))
            .with(pfpr(9))
            .with(pfpr(10))
            .with(pfpr(11))
            .with(pfpr(12))
            .with(pfpr(13)),
        PRegSet::empty()
            .with(pvr(1))
            .with(pvr(2))
            .with(pvr(3))
            .with(pvr(4))
            .with(pvr(5))
            .with(pvr(6))
            .with(pvr(7))
            .with(pvr(8))
            .with(pvr(9))
            .with(pvr(10))
            .with(pvr(11))
            .with(pvr(12))
            .with(pvr(13))
            .with(pvr(14))
            .with(pvr(15))
            .with(pvr(16))
            .with(pvr(17))
            .with(pvr(18))
            .with(pvr(19)),
    ];

    let non_preferred_regs_by_class: [PRegSet; 3] = [
        PRegSet::empty()
            .with(pgpr(14))
            .with(pgpr(15))
            .with(pgpr(16))
            .with(pgpr(17))
            .with(pgpr(18))
            .with(pgpr(19))
            .with(pgpr(20))
            .with(pgpr(21))
            .with(pgpr(22))
            .with(pgpr(23))
            .with(pgpr(24))
            .with(pgpr(25))
            .with(pgpr(26))
            .with(pgpr(27))
            .with(pgpr(28))
            .with(pgpr(29))
            .with(pgpr(30)),
        PRegSet::empty()
            .with(pfpr(14))
            .with(pfpr(15))
            .with(pfpr(16))
            .with(pfpr(17))
            .with(pfpr(18))
            .with(pfpr(19))
            .with(pfpr(20))
            .with(pfpr(21))
            .with(pfpr(22))
            .with(pfpr(23))
            .with(pfpr(24))
            .with(pfpr(25))
            .with(pfpr(26))
            .with(pfpr(27))
            .with(pfpr(28))
            .with(pfpr(29))
            .with(pfpr(30))
            .with(pfpr(31)),
        PRegSet::empty()
            .with(pvr(20))
            .with(pvr(21))
            .with(pvr(22))
            .with(pvr(23))
            .with(pvr(24))
            .with(pvr(25))
            .with(pvr(26))
            .with(pvr(27))
            .with(pvr(28))
            .with(pvr(29))
            .with(pvr(30)),
    ];

    MachineEnv {
        preferred_regs_by_class,
        non_preferred_regs_by_class,
        fixed_stack_slots: vec![],
        scratch_by_class: [None, None, None],
    }
}
