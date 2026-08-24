//! This module defines ppc64-specific machine instruction types.

use crate::binemit::{Addend, CodeOffset, Reloc};
pub use crate::ir::condcodes::{FloatCC, IntCC};
use crate::ir::types::{F32, F64, I8, I8X16, I16, I32, I64, I128};
pub use crate::ir::{MemFlagsData, Type};
use crate::isa::FunctionAlignment;
use crate::machinst::*;
use crate::{CodegenError, CodegenResult, settings};

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use regalloc2::RegClass;
use smallvec::SmallVec;

pub mod regs;
pub use self::regs::*;
pub mod args;
pub use self::args::*;
pub mod emit;
pub use self::emit::*;
pub mod encode;
pub(crate) use self::encode::*;
pub mod unwind;

use crate::isa::ppc64::abi::Ppc64MachineDeps;

//=============================================================================
// Instructions (top level): definition

pub use crate::isa::ppc64::lower::isle::generated_code::{
    AluImmOp, AluOp, BitOp, DivOp, FpuOp1, FpuOp2, FpuRoundMode, LoadOP, MInst as Inst, ShiftOp,
    StoreOP, VecAluOp, VecAluOp4, VecCvtOp, VecFpuOp1, VecFpuOp2, VecUnaryOp,
    UnaryOp,
};

/// Additional information for `return_call[_ind]` instructions, left out
/// of line to lower the size of the `Inst` enum.
#[derive(Clone, Debug)]
pub struct ReturnCallInfo<T> {
    pub dest: T,
    pub uses: CallArgList,
    pub new_stack_arg_size: u32,
}

impl Inst {
    /// Generic constructor for a load (zero-extending where appropriate).
    pub fn gen_load(into_reg: Writable<Reg>, mem: AMode, ty: Type, flags: MemFlagsData) -> Inst {
        if ty.is_vector() {
            return Inst::VecLoad {
                rd: into_reg,
                from: mem,
                flags,
            };
        }
        Inst::Load {
            rd: into_reg,
            op: LoadOP::from_type(ty),
            flags,
            from: mem,
        }
    }

    /// Generic constructor for a store.
    pub fn gen_store(mem: AMode, from_reg: Reg, ty: Type, flags: MemFlagsData) -> Inst {
        if ty.is_vector() {
            return Inst::VecStore {
                to: mem,
                rs: from_reg,
                flags,
            };
        }
        Inst::Store {
            to: mem,
            op: StoreOP::from_type(ty),
            flags,
            src: from_reg,
        }
    }
}

fn ppc64_get_operands(inst: &mut Inst, collector: &mut impl OperandVisitor) {
    match inst {
        Inst::Nop0 | Inst::Nop4 | Inst::Ret | Inst::Udf { .. } | Inst::Jump { .. } => {}
        Inst::LoadConst64 { rd, .. } => collector.reg_def(rd),
        Inst::AluRRR { rd, ra, rb, .. } => {
            collector.reg_use(ra);
            collector.reg_use(rb);
            collector.reg_def(rd);
        }
        Inst::Extend { rd, rn, .. }
        | Inst::AluRRImm16 { rd, ra: rn, .. }
        | Inst::UnaryRR { rd, rn, .. }
        | Inst::BitCount { rd, rn, .. } => {
            collector.reg_use(rn);
            collector.reg_def(rd);
        }
        Inst::FpuRRR { rd, ra, rb, .. } => {
            collector.reg_use(ra);
            collector.reg_use(rb);
            collector.reg_def(rd);
        }
        Inst::FpuRound { rd, rn, .. } => {
            collector.reg_use(rn);
            collector.reg_def(rd);
        }
        Inst::Bswap { rd, rn, .. } => {
            collector.reg_use(rn);
            collector.reg_def(rd);
        }
        Inst::FpuRR { rd, rn, .. }
        | Inst::IntToFpu { rd, rn, .. }
        | Inst::MovToFpr { rd, rn }
        | Inst::MovFromFpr { rd, rn } => {
            collector.reg_use(rn);
            collector.reg_def(rd);
        }
        Inst::FpuFma { rd, ra, rc, rb, .. } => {
            collector.reg_use(ra);
            collector.reg_use(rc);
            collector.reg_use(rb);
            collector.reg_def(rd);
        }
        Inst::FpuToInt { rd, tmp, rn, .. } => {
            collector.reg_use(rn);
            collector.reg_early_def(rd);
            collector.reg_early_def(tmp);
        }
        Inst::FpuMinMax { rd, ra, rb, .. } => {
            collector.reg_use(ra);
            collector.reg_use(rb);
            // The NaN path re-reads the inputs after rd is written.
            collector.reg_early_def(rd);
        }
        Inst::FpuCmpSet { rd, kind } => {
            collector.reg_use(&mut kind.rs1);
            collector.reg_use(&mut kind.rs2);
            collector.reg_def(rd);
        }
        Inst::FpuCondBr { kind, .. } => {
            collector.reg_use(&mut kind.rs1);
            collector.reg_use(&mut kind.rs2);
        }
        Inst::ShiftRRImm { rd, ra, .. } => {
            collector.reg_use(ra);
            collector.reg_def(rd);
        }
        Inst::ShiftRRR { rd, ra, rb, .. } => {
            collector.reg_use(ra);
            collector.reg_use(rb);
            collector.reg_def(rd);
        }
        Inst::DivRem { rd, ra, rb, .. } => {
            collector.reg_use(ra);
            collector.reg_use(rb);
            // The expansion branches around the divide, so `rd` may be
            // written on some paths only; an early def keeps it distinct
            // from the still-live inputs.
            collector.reg_early_def(rd);
        }
        Inst::FpuSelect { rd, kind, rt, rf } => {
            collector.reg_use(&mut kind.rs1);
            collector.reg_use(&mut kind.rs2);
            collector.reg_use(rt);
            collector.reg_use(rf);
            collector.reg_def(rd);
        }
        Inst::EmitIsland { .. } => {}
        Inst::BrTable {
            index,
            tmp1,
            tmp2,
            ..
        } => {
            collector.reg_use(index);
            collector.reg_early_def(tmp1);
            collector.reg_early_def(tmp2);
        }
        Inst::Select { rd, kind, rt, rf } => {
            collector.reg_use(&mut kind.rs1);
            collector.reg_use(&mut kind.rs2);
            collector.reg_use(rt);
            collector.reg_use(rf);
            collector.reg_def(rd);
        }
        Inst::Load { rd, from, .. } => {
            from.get_operands(collector);
            collector.reg_def(rd);
        }
        Inst::VecLoad { rd, from, .. } => {
            from.get_operands(collector);
            collector.reg_def(rd);
        }
        Inst::VecStore { to, rs, .. } => {
            to.get_operands(collector);
            collector.reg_use(rs);
        }
        Inst::VecAluRRR { rd, ra, rb, .. } => {
            collector.reg_use(ra);
            collector.reg_use(rb);
            collector.reg_def(rd);
        }
        Inst::VecExtractLaneInt { rd, rn, .. } | Inst::VecExtractLaneFpu { rd, rn, .. } => {
            collector.reg_use(rn);
            collector.reg_def(rd);
        }
        Inst::VecInsertLane { rd, rv, rs, .. } => {
            collector.reg_use(rv);
            collector.reg_use(rs);
            collector.reg_def(rd);
        }
        Inst::VecFpuFma { rd, ra, rb, rc, .. } => {
            collector.reg_use(ra);
            collector.reg_use(rb);
            collector.reg_use(rc);
            // `rd` is the accumulator, which VSX both reads and writes.
            collector.reg_reuse_def(rd, 2);
        }
        Inst::VecAluRRRR { rd, ra, rb, rc, .. } => {
            collector.reg_use(ra);
            collector.reg_use(rb);
            collector.reg_use(rc);
            collector.reg_def(rd);
        }
        Inst::VecCvt { rd, rn, .. } => {
            collector.reg_use(rn);
            collector.reg_def(rd);
        }
        Inst::VecSpltImm { rd, .. } => {
            collector.reg_def(rd);
        }
        Inst::VecUnary { rd, rn, .. } | Inst::VecRound { rd, rn, .. } => {
            collector.reg_use(rn);
            collector.reg_def(rd);
        }
        Inst::VecSel {
            rd,
            if_set,
            if_clear,
            mask,
        } => {
            collector.reg_use(if_set);
            collector.reg_use(if_clear);
            collector.reg_use(mask);
            collector.reg_def(rd);
        }
        Inst::VecTestLanes { rd, rn, .. } => {
            collector.reg_use(rn);
            collector.reg_def(rd);
        }
        Inst::VecFpuRRR { rd, ra, rb, .. } => {
            collector.reg_use(ra);
            collector.reg_use(rb);
            collector.reg_def(rd);
        }
        Inst::VecFpuRR { rd, rn, .. } => {
            collector.reg_use(rn);
            collector.reg_def(rd);
        }
        Inst::VecZero { rd } => {
            collector.reg_def(rd);
        }
        Inst::MovToVec { rd, rn } | Inst::VecSplatLane { rd, rn, .. } => {
            collector.reg_use(rn);
            collector.reg_def(rd);
        }
        Inst::VecSplatFpr { rd, rn, .. } => {
            collector.reg_use(rn);
            collector.reg_def(rd);
        }
        Inst::VecPermDi { rd, ra, rb, .. } => {
            collector.reg_use(ra);
            collector.reg_use(rb);
            collector.reg_def(rd);
        }
        Inst::Store { to, src, .. } => {
            to.get_operands(collector);
            collector.reg_use(src);
        }
        Inst::LoadAddr { rd, mem } => {
            mem.get_operands(collector);
            collector.reg_early_def(rd);
        }
        Inst::Args { args } => {
            for ArgPair { vreg, preg } in args {
                collector.reg_fixed_def(vreg, *preg);
            }
        }
        Inst::Rets { rets } => {
            for RetPair { vreg, preg } in rets {
                collector.reg_fixed_use(vreg, *preg);
            }
        }
        Inst::Mov { rd, rm, .. } => {
            collector.reg_use(rm);
            collector.reg_def(rd);
        }
        Inst::CmpSet { rd, kind } => {
            collector.reg_use(&mut kind.rs1);
            collector.reg_use(&mut kind.rs2);
            collector.reg_def(rd);
        }
        Inst::CondBr { kind, .. } => {
            collector.reg_use(&mut kind.rs1);
            collector.reg_use(&mut kind.rs2);
        }
        Inst::TrapIf { kind, .. } => {
            collector.reg_use(&mut kind.rs1);
            collector.reg_use(&mut kind.rs2);
        }
        Inst::Call { info } => {
            let CallInfo { uses, defs, .. } = &mut **info;
            for CallArgPair { vreg, preg } in uses {
                collector.reg_fixed_use(vreg, *preg);
            }
            for CallRetPair { vreg, location } in defs {
                match location {
                    RetLocation::Reg(preg, ..) => collector.reg_fixed_def(vreg, *preg),
                    RetLocation::Stack(..) => collector.any_def(vreg),
                }
            }
            collector.reg_clobbers(info.clobbers);
            if let Some(try_call_info) = &mut info.try_call_info {
                try_call_info.collect_operands(collector);
            }
        }
        Inst::CallInd { info } => {
            let CallInfo {
                dest, uses, defs, ..
            } = &mut **info;
            // The ELFv2 global-entry convention requires the callee's
            // address in r12 at an indirect call.
            collector.reg_fixed_use(dest, call_target_reg());
            for CallArgPair { vreg, preg } in uses {
                collector.reg_fixed_use(vreg, *preg);
            }
            for CallRetPair { vreg, location } in defs {
                match location {
                    RetLocation::Reg(preg, ..) => collector.reg_fixed_def(vreg, *preg),
                    RetLocation::Stack(..) => collector.any_def(vreg),
                }
            }
            collector.reg_clobbers(info.clobbers);
            if let Some(try_call_info) = &mut info.try_call_info {
                try_call_info.collect_operands(collector);
            }
        }
        Inst::AtomicLoad { rd, addr, .. } => {
            collector.reg_use(addr);
            collector.reg_def(rd);
        }
        Inst::AtomicStore { src, addr, .. } => {
            collector.reg_use(src);
            collector.reg_use(addr);
        }
        Inst::AtomicRmw { rd, addr, src, .. } => {
            collector.reg_use(addr);
            collector.reg_use(src);
            // The loop re-reads addr and src after rd is written.
            collector.reg_early_def(rd);
        }
        Inst::AtomicCas {
            rd,
            addr,
            expected,
            new,
            ..
        } => {
            collector.reg_use(addr);
            collector.reg_use(expected);
            collector.reg_use(new);
            collector.reg_early_def(rd);
        }
        Inst::AtomicLoad128 { rd_lo, rd_hi, addr } => {
            // The address must be pinned away from the destination
            // pair: defs are late-position, so a free address could
            // legally share r8/r9 -- and `lqarx` with RA or RB inside
            // the target pair is an invalid form (SIGILL in practice).
            collector.reg_fixed_use(addr, gpr(3));
            collector.reg_fixed_def(rd_hi, gpr(8));
            collector.reg_fixed_def(rd_lo, gpr(9));
        }
        Inst::AtomicStore128 {
            rs_lo,
            rs_hi,
            tmp_lo,
            tmp_hi,
            addr,
        } => {
            // `stqcx.` needs an even:odd source pair; r4:r5 with the
            // high half in the even register, matching `lqarx`'s view
            // of a little-endian quadword. The discarded reservation
            // load lands in r8:r9.
            collector.reg_fixed_use(addr, gpr(3));
            collector.reg_fixed_use(rs_hi, gpr(4));
            collector.reg_fixed_use(rs_lo, gpr(5));
            collector.reg_fixed_def(tmp_hi, gpr(8));
            collector.reg_fixed_def(tmp_lo, gpr(9));
        }
        Inst::AtomicRmw128 {
            rd_lo,
            rd_hi,
            tmp_hi,
            addr,
            src_lo,
            src_hi,
            ..
        } => {
            collector.reg_fixed_use(addr, gpr(3));
            collector.reg_fixed_use(src_hi, gpr(4));
            collector.reg_fixed_use(src_lo, gpr(5));
            collector.reg_fixed_def(rd_hi, gpr(8));
            collector.reg_fixed_def(rd_lo, gpr(9));
            // The computed value goes in r10:r11; r11 is the spill
            // temp, permanently free, so only r10 needs reserving.
            collector.reg_fixed_def(tmp_hi, gpr(10));
        }
        Inst::AtomicCas128 {
            rd_lo,
            rd_hi,
            addr,
            exp_lo,
            exp_hi,
            new_lo,
            new_hi,
        } => {
            collector.reg_fixed_use(addr, gpr(3));
            collector.reg_fixed_use(exp_hi, gpr(4));
            collector.reg_fixed_use(exp_lo, gpr(5));
            collector.reg_fixed_use(new_hi, gpr(6));
            collector.reg_fixed_use(new_lo, gpr(7));
            collector.reg_fixed_def(rd_hi, gpr(8));
            collector.reg_fixed_def(rd_lo, gpr(9));
        }
        Inst::Fence => {}
        Inst::ReturnCall { info } => {
            for CallArgPair { vreg, preg } in &mut info.uses {
                collector.reg_fixed_use(vreg, *preg);
            }
        }
        Inst::ReturnCallInd { info } => {
            // r12 is a volatile non-argument register, so it cannot
            // collide with the fixed argument constraints; it is also
            // what the ELFv2 global-entry convention wants.
            collector.reg_fixed_use(&mut info.dest, call_target_reg());
            for CallArgPair { vreg, preg } in &mut info.uses {
                collector.reg_fixed_use(vreg, *preg);
            }
        }
        Inst::LoadExtName { rd, .. }
        | Inst::LabelAddress { rd, .. }
        | Inst::MovFromPReg { rd, .. } => collector.reg_def(rd),
        Inst::Mflr { rd } => collector.reg_def(rd),
        Inst::Mtlr { rs } => collector.reg_use(rs),
        Inst::Unwind { .. } => {}
        Inst::DummyUse { reg } => collector.reg_use(reg),
    }
}

impl MachInst for Inst {
    type LabelUse = LabelUse;
    type ABIMachineSpec = Ppc64MachineDeps;

    /// The all-zeros word: permanently invalid, raises SIGILL (which
    /// Wasmtime's signal handler listens for, unlike the SIGTRAP that the
    /// `trap` instruction would raise).
    const TRAP_OPCODE: &'static [u8] = &[0; 4];

    fn gen_dummy_use(reg: Reg) -> Self {
        Inst::DummyUse { reg }
    }

    fn canonical_type_for_rc(rc: RegClass) -> Type {
        match rc {
            RegClass::Int => I64,
            RegClass::Float => F64,
            RegClass::Vector => crate::ir::types::I8X16,
        }
    }

    fn is_safepoint(&self) -> bool {
        match self {
            Inst::Call { .. } | Inst::CallInd { .. } => true,
            _ => false,
        }
    }

    fn get_operands(&mut self, collector: &mut impl OperandVisitor) {
        ppc64_get_operands(self, collector);
    }

    fn is_move(&self) -> Option<(Writable<Reg>, Reg)> {
        match self {
            Inst::Mov { rd, rm, .. } => Some((*rd, *rm)),
            Inst::FpuRR {
                op: FpuOp1::Mov,
                rd,
                rn,
                ..
            } => Some((*rd, *rn)),
            _ => None,
        }
    }

    fn is_included_in_clobbers(&self) -> bool {
        match self {
            Inst::Args { .. } => false,
            _ => true,
        }
    }

    fn is_trap(&self) -> bool {
        match self {
            Inst::Udf { .. } => true,
            _ => false,
        }
    }

    fn is_args(&self) -> bool {
        match self {
            Inst::Args { .. } => true,
            _ => false,
        }
    }

    fn call_type(&self) -> CallType {
        match self {
            Inst::Call { .. } | Inst::CallInd { .. } => CallType::Regular,
            Inst::ReturnCall { .. } | Inst::ReturnCallInd { .. } => CallType::TailCall,
            _ => CallType::None,
        }
    }

    fn is_term(&self) -> MachTerminator {
        match self {
            Inst::Jump { .. }
            | Inst::CondBr { .. }
            | Inst::FpuCondBr { .. }
            | Inst::BrTable { .. } => MachTerminator::Branch,
            Inst::Rets { .. } => MachTerminator::Ret,
            Inst::ReturnCall { .. } | Inst::ReturnCallInd { .. } => MachTerminator::RetCall,
            Inst::Call { info } if info.try_call_info.is_some() => MachTerminator::Branch,
            Inst::CallInd { info } if info.try_call_info.is_some() => MachTerminator::Branch,
            _ => MachTerminator::None,
        }
    }

    fn is_mem_access(&self) -> bool {
        match self {
            Inst::Load { .. } | Inst::Store { .. } => true,
            _ => false,
        }
    }

    fn gen_move(to_reg: Writable<Reg>, from_reg: Reg, ty: Type) -> Inst {
        Inst::Mov {
            rd: to_reg,
            rm: from_reg,
            ty,
        }
    }

    fn gen_nop(preferred_size: usize) -> Inst {
        if preferred_size == 0 {
            return Inst::Nop0;
        }
        assert!(preferred_size >= 4);
        Inst::Nop4
    }

    fn gen_nop_units() -> Vec<Vec<u8>> {
        vec![NOP_INSTRUCTION.to_le_bytes().to_vec()]
    }

    fn rc_for_type(ty: &Type) -> CodegenResult<(&[RegClass], &[Type])> {
        match *ty {
            I8 | I16 | I32 | I64 => Ok((&[RegClass::Int], core::slice::from_ref(ty))),
            // An `i128` lives in a pair of GPRs, low doubleword first,
            // matching both little-endian memory order and the ELFv2
            // convention that the lower-numbered register of a pair
            // holds the least-significant half.
            I128 => Ok((&[RegClass::Int, RegClass::Int], &[I64, I64])),
            // An `f32` is held in a floating-point register in double
            // format, because that is what `lfs` produces and what the
            // arithmetic instructions operate on. Reporting the *stored*
            // type as `f64` keeps that consistent everywhere a value is
            // copied by size rather than by lowering rule: spills and
            // reloads, and the ABI's argument and return slots. Without
            // it, the shared code that moves a stack-returned value
            // through an integer register would copy only the four bytes
            // of the single-precision encoding into a slot that is later
            // read back as a double.
            //
            // The cost is that a stack-passed `f32` occupies its 8-byte
            // slot in double format rather than as a single in the low
            // four bytes, which deviates from ELFv2 for calls into C code
            // that run out of floating-point argument registers.
            F32 => Ok((&[RegClass::Float], &[F64])),
            F64 => Ok((&[RegClass::Float], core::slice::from_ref(ty))),
            _ if ty.is_vector() && ty.bits() == 128 => {
                Ok((&[RegClass::Vector], core::slice::from_ref(ty)))
            }
            // A 64-bit vector occupies big-endian doubleword 0 of a
            // vector register -- the same bit positions the high lanes
            // of the corresponding 128-bit type would use -- with the
            // other doubleword left undefined. See the lowering rules
            // for why that half is the useful one.
            //
            // The *stored* type is the full 128-bit vector so that
            // spills and reloads move the whole register. Copying only
            // the meaningful eight bytes would be enough, but the
            // register is the natural unit here and nothing reads the
            // undefined half.
            _ if ty.is_vector() && ty.bits() == 64 => Ok((&[RegClass::Vector], &[I8X16])),
            _ => Err(CodegenError::Unsupported(alloc::format!(
                "type not yet supported by the ppc64 backend: {ty}"
            ))),
        }
    }

    fn gen_jump(target: MachLabel) -> Inst {
        Inst::Jump { label: target }
    }

    fn worst_case_size() -> CodeOffset {
        // The largest expansion is a trapping float-to-integer
        // conversion: a NaN check and two bound checks, each of which
        // materializes a 64-bit constant (up to five instructions), come
        // to 22 instructions. Leave headroom above that.
        96
    }

    fn worst_case_island_growth() -> CodeOffset {
        // Branch16 veneers are 4 bytes; allow several per instruction.
        32
    }

    fn function_alignment() -> FunctionAlignment {
        FunctionAlignment {
            minimum: 4,
            preferred: 16,
        }
    }
}

//=============================================================================
// Pretty-printing of instructions.

impl Inst {
    fn print_with_state(&self, _state: &mut EmitState) -> String {
        use alloc::format;
        let reg = |r: Reg| -> String { reg_name(r) };
        let wreg = |r: Writable<Reg>| -> String { reg_name(r.to_reg()) };
        match self {
            Inst::Nop0 => "##zero length nop".to_string(),
            Inst::Nop4 => "nop".to_string(),
            Inst::LoadConst64 { rd, imm } => format!("load_const {}, {imm:#x}", wreg(*rd)),
            Inst::AluRRR { op, rd, ra, rb } => {
                let mnemonic = match op {
                    AluOp::Add => "add",
                    AluOp::Sub => "sub",
                    AluOp::Addc => "addc",
                    AluOp::Adde => "adde",
                    AluOp::Subfc => "subc",
                    AluOp::Subfe => "sube",
                    AluOp::And => "and",
                    AluOp::Or => "or",
                    AluOp::Xor => "xor",
                    AluOp::Mulld => "mulld",
                    AluOp::Mulhd => "mulhd",
                    AluOp::Mulhdu => "mulhdu",
                    AluOp::Mulhw => "mulhw",
                    AluOp::Mulhwu => "mulhwu",
                };
                format!("{mnemonic} {}, {}, {}", wreg(*rd), reg(*ra), reg(*rb))
            }
            Inst::AluRRImm16 { op, rd, ra, imm } => {
                let (mnemonic, imm) = match op {
                    AluImmOp::Addi => ("addi", format!("{}", *imm as i16)),
                    AluImmOp::Andi => ("andi.", format!("{imm}")),
                };
                format!("{mnemonic} {}, {}, {imm}", wreg(*rd), reg(*ra))
            }
            Inst::UnaryRR { op, rd, rn } => {
                let mnemonic = match op {
                    UnaryOp::Neg => "neg",
                    UnaryOp::Not => "not",
                };
                format!("{mnemonic} {}, {}", wreg(*rd), reg(*rn))
            }
            Inst::ShiftRRImm { op, rd, ra, imm } => {
                let mnemonic = match op {
                    ShiftOp::Slw => "slwi",
                    ShiftOp::Srw => "srwi",
                    ShiftOp::Sraw => "srawi",
                    ShiftOp::Sld => "sldi",
                    ShiftOp::Srd => "srdi",
                    ShiftOp::Srad => "sradi",
                    ShiftOp::Rotlw => "rotlwi",
                    ShiftOp::Rotld => "rotldi",
                };
                format!("{mnemonic} {}, {}, {imm}", wreg(*rd), reg(*ra))
            }
            Inst::ShiftRRR { op, rd, ra, rb } => {
                let mnemonic = match op {
                    ShiftOp::Slw => "slw",
                    ShiftOp::Srw => "srw",
                    ShiftOp::Sraw => "sraw",
                    ShiftOp::Sld => "sld",
                    ShiftOp::Srd => "srd",
                    ShiftOp::Srad => "srad",
                    ShiftOp::Rotlw => "rotlw",
                    ShiftOp::Rotld => "rotld",
                };
                format!("{mnemonic} {}, {}, {}", wreg(*rd), reg(*ra), reg(*rb))
            }
            Inst::BitCount { op, rd, rn, ty } => {
                let mnemonic = match op {
                    BitOp::Clz => "clz",
                    BitOp::Ctz => "ctz",
                    BitOp::Popcnt => "popcnt",
                };
                format!("{mnemonic}.{ty} {}, {}", wreg(*rd), reg(*rn))
            }
            Inst::DivRem { op, rd, ra, rb, ty } => {
                let mnemonic = match op {
                    DivOp::SDiv => "sdiv",
                    DivOp::UDiv => "udiv",
                    DivOp::SRem => "srem",
                    DivOp::URem => "urem",
                };
                format!(
                    "{mnemonic}.{ty} {}, {}, {}",
                    wreg(*rd),
                    reg(*ra),
                    reg(*rb)
                )
            }
            Inst::EmitIsland { needed_space } => format!("emit_island {needed_space}"),
            Inst::BrTable { index, targets, .. } => {
                format!("br_table {} # {} targets", reg(*index), targets.len() - 1)
            }
            Inst::FpuSelect { rd, kind, rt, rf } => format!(
                "fselect.{} {}, {}, {} # cmp {}, {}",
                kind.kind,
                wreg(*rd),
                reg(*rt),
                reg(*rf),
                reg(kind.rs1),
                reg(kind.rs2)
            ),
            Inst::Select { rd, kind, rt, rf } => format!(
                "select.{} {}, {}, {} # cmp {}, {}",
                kind.kind,
                wreg(*rd),
                reg(*rt),
                reg(*rf),
                reg(kind.rs1),
                reg(kind.rs2)
            ),
            Inst::FpuRRR { op, rd, ra, rb, ty } => {
                let mnemonic = match op {
                    FpuOp2::Add => "fadd",
                    FpuOp2::Sub => "fsub",
                    FpuOp2::Mul => "fmul",
                    FpuOp2::Div => "fdiv",
                    FpuOp2::CopySign => "fcpsgn",
                };
                format!("{mnemonic}.{ty} {}, {}, {}", wreg(*rd), reg(*ra), reg(*rb))
            }
            Inst::FpuRound { rd, rn, mode } => {
                let mnemonic = match mode {
                    FpuRoundMode::Floor => "frim",
                    FpuRoundMode::Ceil => "frip",
                    FpuRoundMode::Trunc => "friz",
                    FpuRoundMode::Nearest => "xsrdpic",
                };
                format!("{mnemonic} {}, {}", wreg(*rd), reg(*rn))
            }
            Inst::Bswap { rd, rn, ty } => {
                format!("bswap{} {}, {}", ty.bits(), wreg(*rd), reg(*rn))
            }
            Inst::FpuRR { op, rd, rn, ty } => {
                let mnemonic = match op {
                    FpuOp1::Neg => "fneg",
                    FpuOp1::Abs => "fabs",
                    FpuOp1::Sqrt => "fsqrt",
                    FpuOp1::Mov => "fmr",
                    FpuOp1::Demote => "frsp",
                    FpuOp1::CvtToSingleBits => "xscvdpspn",
                    FpuOp1::CvtFromSingleBits => "xscvspdpn",
                };
                format!("{mnemonic}.{ty} {}, {}", wreg(*rd), reg(*rn))
            }
            Inst::FpuFma {
                rd,
                ra,
                rc,
                rb,
                ty,
            } => format!(
                "fmadd.{ty} {}, {}, {}, {}",
                wreg(*rd),
                reg(*ra),
                reg(*rc),
                reg(*rb)
            ),
            Inst::FpuCmpSet { rd, kind } => format!(
                "fcmp_set.{} {}, {}, {}",
                kind.kind,
                wreg(*rd),
                reg(kind.rs1),
                reg(kind.rs2)
            ),
            Inst::FpuCondBr {
                taken,
                not_taken,
                kind,
            } => format!(
                "fbr.{} {}, {} # {}, {}",
                kind.kind,
                reg(kind.rs1),
                reg(kind.rs2),
                taken,
                not_taken
            ),
            Inst::IntToFpu { rd, rn, signed, ty } => {
                let mnemonic = if *signed { "fcfid" } else { "fcfidu" };
                format!("{mnemonic}.{ty} {}, {}", wreg(*rd), reg(*rn))
            }
            Inst::FpuToInt {
                rd,
                rn,
                signed,
                sat,
                out_ty,
                ..
            } => {
                let mnemonic = match (signed, sat) {
                    (true, false) => "fcvt_to_sint",
                    (false, false) => "fcvt_to_uint",
                    (true, true) => "fcvt_to_sint_sat",
                    (false, true) => "fcvt_to_uint_sat",
                };
                format!("{mnemonic}.{out_ty} {}, {}", wreg(*rd), reg(*rn))
            }
            Inst::FpuMinMax { rd, ra, rb, is_max } => format!(
                "{} {}, {}, {}",
                if *is_max { "fmax" } else { "fmin" },
                wreg(*rd),
                reg(*ra),
                reg(*rb)
            ),
            Inst::MovToFpr { rd, rn } => format!("mtvsrd {}, {}", wreg(*rd), reg(*rn)),
            Inst::MovFromFpr { rd, rn } => format!("mfvsrd {}, {}", wreg(*rd), reg(*rn)),
            Inst::Extend {
                rd,
                rn,
                signed,
                from_bits,
                to_bits,
            } => {
                let op = if *signed { "sext" } else { "zext" };
                format!("{op}.{from_bits}->{to_bits} {}, {}", wreg(*rd), reg(*rn))
            }
            Inst::Load { rd, op, from, .. } => {
                let mnemonic = match op {
                    LoadOP::Lbz => "lbz",
                    LoadOP::Lhz => "lhz",
                    LoadOP::Lwz => "lwz",
                    LoadOP::Ld => "ld",
                    LoadOP::Lfs => "lfs",
                    LoadOP::Lfd => "lfd",
                };
                format!("{mnemonic} {}, {from}", wreg(*rd))
            }
            Inst::Store { to, op, src, .. } => {
                let mnemonic = match op {
                    StoreOP::Stb => "stb",
                    StoreOP::Sth => "sth",
                    StoreOP::Stw => "stw",
                    StoreOP::Std => "std",
                    StoreOP::Stfs => "stfs",
                    StoreOP::Stfd => "stfd",
                };
                format!("{mnemonic} {}, {to}", reg(*src))
            }
            Inst::LoadAddr { rd, mem } => format!("load_addr {}, {mem}", wreg(*rd)),
            Inst::Args { args } => {
                let mut s = "args".to_string();
                for arg in args {
                    s.push_str(&format!(" {}={}", reg(arg.vreg.to_reg()), reg(arg.preg)));
                }
                s
            }
            Inst::Rets { rets } => {
                let mut s = "rets".to_string();
                for ret in rets {
                    s.push_str(&format!(" {}={}", reg(ret.vreg), reg(ret.preg)));
                }
                s
            }
            Inst::Ret => "blr".to_string(),
            Inst::Mov { rd, rm, .. } => format!("mr {}, {}", wreg(*rd), reg(*rm)),
            Inst::CmpSet { rd, kind } => format!(
                "cmp_set.{} {}, {}, {}",
                kind.kind,
                wreg(*rd),
                reg(kind.rs1),
                reg(kind.rs2)
            ),
            Inst::Jump { label } => format!("b {label:?}"),
            Inst::CondBr {
                taken,
                not_taken,
                kind,
            } => format!(
                "b{}.{} {}, {} # {}, {}",
                kind.kind,
                if kind.is_64 { "d" } else { "w" },
                reg(kind.rs1),
                reg(kind.rs2),
                taken,
                not_taken
            ),
            Inst::Udf { trap_code } => format!("trap # {trap_code}"),
            Inst::TrapIf { kind, trap_code } => format!(
                "trap_if.{} {}, {} # {trap_code}",
                kind.kind,
                reg(kind.rs1),
                reg(kind.rs2)
            ),
            Inst::AtomicLoad { rd, addr, ty } => {
                format!("atomic_load.{ty} {}, ({})", wreg(*rd), reg(*addr))
            }
            Inst::AtomicStore { src, addr, ty } => {
                format!("atomic_store.{ty} {}, ({})", reg(*src), reg(*addr))
            }
            Inst::AtomicRmw {
                op,
                rd,
                addr,
                src,
                ty,
            } => format!(
                "atomic_rmw.{ty} {op:?} {}, {}, ({})",
                wreg(*rd),
                reg(*src),
                reg(*addr)
            ),
            Inst::AtomicCas {
                rd,
                addr,
                expected,
                new,
                ty,
            } => format!(
                "atomic_cas.{ty} {}, {}, {}, ({})",
                wreg(*rd),
                reg(*expected),
                reg(*new),
                reg(*addr)
            ),
            Inst::AtomicLoad128 { rd_lo, rd_hi, addr } => format!(
                "atomic_load128 {}, {}, ({})",
                wreg(*rd_lo),
                wreg(*rd_hi),
                reg(*addr)
            ),
            Inst::AtomicStore128 {
                rs_lo, rs_hi, addr, ..
            } => format!(
                "atomic_store128 {}, {}, ({})",
                reg(*rs_lo),
                reg(*rs_hi),
                reg(*addr)
            ),
            Inst::AtomicRmw128 {
                op,
                rd_lo,
                rd_hi,
                addr,
                src_lo,
                src_hi,
                ..
            } => format!(
                "atomic_rmw128.{op:?} {}, {}, {}, {}, ({})",
                wreg(*rd_lo),
                wreg(*rd_hi),
                reg(*src_lo),
                reg(*src_hi),
                reg(*addr)
            ),
            Inst::AtomicCas128 {
                rd_lo,
                rd_hi,
                addr,
                exp_lo,
                exp_hi,
                new_lo,
                new_hi,
            } => format!(
                "atomic_cas128 {}, {}, {}, {}, {}, {}, ({})",
                wreg(*rd_lo),
                wreg(*rd_hi),
                reg(*exp_lo),
                reg(*exp_hi),
                reg(*new_lo),
                reg(*new_hi),
                reg(*addr)
            ),
            Inst::VecLoad { rd, from, .. } => {
                format!("vec_load {}, {from}", wreg(*rd))
            }
            Inst::VecStore { to, rs, .. } => {
                format!("vec_store {}, {to}", reg(*rs))
            }
            Inst::VecAluRRR { op, rd, ra, rb, ty } => {
                let mnemonic = match op {
                    VecAluOp::Add => "vadd",
                    VecAluOp::Sub => "vsub",
                    VecAluOp::CmpEq => "vcmpeq",
                    VecAluOp::CmpGtS => "vcmpgts",
                    VecAluOp::CmpGtU => "vcmpgtu",
                    VecAluOp::MinS => "vmins",
                    VecAluOp::MinU => "vminu",
                    VecAluOp::MaxS => "vmaxs",
                    VecAluOp::MaxU => "vmaxu",
                    VecAluOp::Shl => "vsl",
                    VecAluOp::ShrU => "vsr",
                    VecAluOp::ShrS => "vsra",
                    VecAluOp::AvgRoundS => "vavgs",
                    VecAluOp::AvgRoundU => "vavgu",
                    VecAluOp::SAddSat => "vaddsat_s",
                    VecAluOp::UAddSat => "vaddsat_u",
                    VecAluOp::SSubSat => "vsubsat_s",
                    VecAluOp::USubSat => "vsubsat_u",
                    VecAluOp::MergeLow => "vmrgl",
                    VecAluOp::MergeHigh => "vmrgh",
                    VecAluOp::PackSS => "vpk_ss",
                    VecAluOp::PackSU => "vpk_su",
                    VecAluOp::PackUU => "vpk_uu",
                    VecAluOp::MulWord => "vmuluwm",
                    VecAluOp::MulEvenS => "vmules",
                    VecAluOp::MulOddS => "vmulos",
                    VecAluOp::MulEvenU => "vmuleu",
                    VecAluOp::MulOddU => "vmulou",
                    VecAluOp::PackMod => "vpk_um",
                    VecAluOp::MergeEvenWord => "vmrgew",
                    VecAluOp::RotlDword => "vrld",
                    VecAluOp::And => "xxland",
                    VecAluOp::Or => "xxlor",
                    VecAluOp::Xor => "xxlxor",
                    VecAluOp::Nor => "xxlnor",
                    VecAluOp::AndC => "xxlandc",
                };
                match op {
                    VecAluOp::And
                    | VecAluOp::Or
                    | VecAluOp::Xor
                    | VecAluOp::Nor
                    | VecAluOp::AndC => {
                        format!("{mnemonic} {}, {}, {}", wreg(*rd), reg(*ra), reg(*rb))
                    }
                    _ => format!(
                        "{mnemonic}{} {}, {}, {}",
                        ty.lane_bits(),
                        wreg(*rd),
                        reg(*ra),
                        reg(*rb)
                    ),
                }
            }
            Inst::VecExtractLaneInt { rd, rn, ty, lane }
            | Inst::VecExtractLaneFpu { rd, rn, ty, lane } => {
                format!(
                    "vec_extract{} {}, {}, lane {lane}",
                    ty.lane_bits(),
                    wreg(*rd),
                    reg(*rn)
                )
            }
            Inst::VecInsertLane {
                rd,
                rv,
                rs,
                ty,
                lane,
            } => {
                format!(
                    "vec_insert{} {}, {}, {}, lane {lane}",
                    ty.lane_bits(),
                    wreg(*rd),
                    reg(*rv),
                    reg(*rs)
                )
            }
            Inst::VecAluRRRR {
                op,
                rd,
                ra,
                rb,
                rc,
            } => {
                let mnemonic = match op {
                    VecAluOp4::MulAddUH => "vmladduhm",
                    VecAluOp4::MulHiRoundAddSHS => "vmhraddshs",
                    VecAluOp4::Perm => "vperm",
                };
                format!(
                    "{mnemonic} {}, {}, {}, {}",
                    wreg(*rd),
                    reg(*ra),
                    reg(*rb),
                    reg(*rc)
                )
            }
            Inst::VecCvt { op, rd, rn } => {
                let mnemonic = match op {
                    VecCvtOp::F32ToI32S => "xvcvspsxws",
                    VecCvtOp::F32ToI32U => "xvcvspuxws",
                    VecCvtOp::I32ToF32S => "xvcvsxwsp",
                    VecCvtOp::I32ToF32U => "xvcvuxwsp",
                    VecCvtOp::F64ToI64S => "xvcvdpsxds",
                    VecCvtOp::F64ToI64U => "xvcvdpuxds",
                    VecCvtOp::I64ToF64S => "xvcvsxddp",
                    VecCvtOp::I64ToF64U => "xvcvuxddp",
                    VecCvtOp::F64ToF32 => "xvcvdpsp",
                    VecCvtOp::F32ToF64 => "xvcvspdp",
                };
                format!("{mnemonic} {}, {}", wreg(*rd), reg(*rn))
            }
            Inst::VecSpltImm { rd, imm, ty } => {
                format!("vspltis{} {}, {imm}", ty.lane_bits(), wreg(*rd))
            }
            Inst::VecUnary { op, rd, rn, ty } => {
                let mnemonic = match op {
                    VecUnaryOp::Popcnt => "vpopcnt",
                    VecUnaryOp::WidenSLow => "vupkl_s",
                    VecUnaryOp::WidenSHigh => "vupkh_s",
                };
                format!("{mnemonic}{} {}, {}", ty.lane_bits(), wreg(*rd), reg(*rn))
            }
            Inst::VecRound { rd, rn, mode, ty } => {
                let m = match mode {
                    FpuRoundMode::Ceil => "ip",
                    FpuRoundMode::Floor => "im",
                    FpuRoundMode::Trunc => "iz",
                    FpuRoundMode::Nearest => "ic",
                };
                let sfx = if ty.lane_bits() == 32 { "sp" } else { "dp" };
                format!("xvr{sfx}{m} {}, {}", wreg(*rd), reg(*rn))
            }
            Inst::VecSel {
                rd,
                if_set,
                if_clear,
                mask,
            } => format!(
                "xxsel {}, {}, {}, {}",
                wreg(*rd),
                reg(*if_clear),
                reg(*if_set),
                reg(*mask)
            ),
            Inst::VecTestLanes { rd, rn, ty, all } => {
                let kind = if *all { "all" } else { "any" };
                format!(
                    "vec_{kind}_true{} {}, {}",
                    ty.lane_bits(),
                    wreg(*rd),
                    reg(*rn)
                )
            }
            Inst::VecFpuFma { rd, ra, rb, ty, .. } => {
                let suffix = if ty.lane_bits() == 32 { "sp" } else { "dp" };
                format!(
                    "xvmadda{suffix} {}, {}, {}",
                    wreg(*rd),
                    reg(*ra),
                    reg(*rb)
                )
            }
            Inst::VecFpuRRR { op, rd, ra, rb, ty } => {
                let mnemonic = match op {
                    VecFpuOp2::Add => "xvadd",
                    VecFpuOp2::Sub => "xvsub",
                    VecFpuOp2::Mul => "xvmul",
                    VecFpuOp2::Div => "xvdiv",
                    VecFpuOp2::CmpEq => "xvcmpeq",
                    VecFpuOp2::CmpGt => "xvcmpgt",
                    VecFpuOp2::CmpGe => "xvcmpge",
                    VecFpuOp2::Min => "xvmin",
                    VecFpuOp2::Max => "xvmax",
                    VecFpuOp2::CopySign => "xvcpsgn",
                };
                let sfx = if ty.lane_bits() == 32 { "sp" } else { "dp" };
                format!(
                    "{mnemonic}{sfx} {}, {}, {}",
                    wreg(*rd),
                    reg(*ra),
                    reg(*rb)
                )
            }
            Inst::VecFpuRR { op, rd, rn, ty } => {
                let mnemonic = match op {
                    VecFpuOp1::Sqrt => "xvsqrt",
                    VecFpuOp1::Neg => "xvneg",
                    VecFpuOp1::Abs => "xvabs",
                };
                let sfx = if ty.lane_bits() == 32 { "sp" } else { "dp" };
                format!("{mnemonic}{sfx} {}, {}", wreg(*rd), reg(*rn))
            }
            Inst::VecZero { rd } => format!("vec_zero {}", wreg(*rd)),
            Inst::MovToVec { rd, rn } => format!("mtvsrd {}, {}", wreg(*rd), reg(*rn)),
            Inst::VecSplatLane { rd, rn, ty } => {
                format!("vec_splat{} {}, {}", ty.lane_bits(), wreg(*rd), reg(*rn))
            }
            Inst::VecSplatFpr { rd, rn, is_f32 } => {
                let mnemonic = if *is_f32 { "xxspltw" } else { "xxpermdi0" };
                format!("{mnemonic} {}, {}", wreg(*rd), reg(*rn))
            }
            Inst::VecPermDi { rd, ra, rb, dm } => {
                format!("xxpermdi {}, {}, {}, {dm}", wreg(*rd), reg(*ra), reg(*rb))
            }
            Inst::Fence => "sync".to_string(),
            Inst::Call { info } => format!("bl {:?}", info.dest),
            Inst::ReturnCall { info } => format!("return_call {:?}", info.dest),
            Inst::ReturnCallInd { info } => {
                format!("mtctr {}; bctr # tail call", reg(info.dest))
            }
            Inst::CallInd { info } => format!("mtctr {}; bctrl", reg(info.dest)),
            Inst::LoadExtName { rd, name, offset } => {
                format!("load_ext_name {}, {name:?}+{offset}", wreg(*rd))
            }
            Inst::MovFromPReg { rd, rm } => format!(
                "mr {}, {}",
                wreg(*rd),
                if *rm == 1 { "sp" } else { "fp" }
            ),
            Inst::LabelAddress { rd, label } => {
                format!("label_address {}, {label:?}", wreg(*rd))
            }
            Inst::Mflr { rd } => format!("mflr {}", wreg(*rd)),
            Inst::Mtlr { rs } => format!("mtlr {}", reg(*rs)),
            Inst::Unwind { inst } => format!("unwind {inst:?}"),
            Inst::DummyUse { reg: r } => format!("dummy_use {}", reg(*r)),
        }
    }
}

//=============================================================================
// Label uses: branch fixups and veneers.

/// Different forms of label references for different instruction formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelUse {
    /// I-form branch (`b`/`bl`): 24-bit word offset in bits 25:2, range
    /// ±32 MiB.
    Branch26,
    /// B-form conditional branch (`bc`): 14-bit word offset in bits
    /// 15:2, range ±32 KiB. Promoted to a `b` veneer when out of range.
    Branch16,
    /// An `addis`/`addi` pair adding a label's offset (relative to the
    /// instruction 4 bytes before the pair, where a `bcl 20,31,$+4;
    /// mflr` sequence read the PC) to a register. Range ±2 GiB.
    PCRelHiLo,
}

impl MachInstLabelUse for LabelUse {
    /// Every ppc64 instruction is 4 bytes.
    const ALIGN: CodeOffset = 4;

    fn max_pos_range(self) -> CodeOffset {
        match self {
            LabelUse::Branch26 => (1 << 25) - 4,
            LabelUse::Branch16 => (1 << 15) - 4,
            LabelUse::PCRelHiLo => i32::MAX as CodeOffset - 4,
        }
    }

    fn max_neg_range(self) -> CodeOffset {
        match self {
            LabelUse::Branch26 => 1 << 25,
            LabelUse::Branch16 => 1 << 15,
            LabelUse::PCRelHiLo => 1 << 31,
        }
    }

    fn patch_size(self) -> CodeOffset {
        match self {
            LabelUse::PCRelHiLo => 8,
            _ => 4,
        }
    }

    fn patch(self, buffer: &mut [u8], use_offset: CodeOffset, label_offset: CodeOffset) {
        let offset = (label_offset as i64) - (use_offset as i64);
        debug_assert!(
            offset >= -(self.max_neg_range() as i64) && offset <= (self.max_pos_range() as i64)
        );
        debug_assert_eq!(offset & 3, 0);
        if self == LabelUse::PCRelHiLo {
            // The pair sits 4 bytes after the `mflr` whose value it
            // adjusts, so the delta is relative to use_offset - 4. Split
            // into a high-adjusted/low pair such that
            // (ha << 16) + sign_extend(lo) == delta.
            let delta = offset + 4;
            let lo = delta as i16;
            let ha = ((delta - i64::from(lo)) >> 16) as u16;
            let addis = u32::from_le_bytes(buffer[0..4].try_into().unwrap());
            let addi = u32::from_le_bytes(buffer[4..8].try_into().unwrap());
            buffer[0..4].copy_from_slice(&(addis | u32::from(ha)).to_le_bytes());
            buffer[4..8].copy_from_slice(&(addi | u32::from(lo as u16)).to_le_bytes());
            return;
        }
        let insn = u32::from_le_bytes(buffer[0..4].try_into().unwrap());
        let field_mask = match self {
            LabelUse::Branch26 => 0x03FF_FFFC,
            LabelUse::Branch16 => 0x0000_FFFC,
            LabelUse::PCRelHiLo => unreachable!(),
        };
        let patched = (insn & !field_mask) | ((offset as u32) & field_mask);
        buffer[0..4].copy_from_slice(&patched.to_le_bytes());
    }

    fn supports_veneer(self) -> bool {
        match self {
            LabelUse::Branch26 | LabelUse::PCRelHiLo => false,
            LabelUse::Branch16 => true,
        }
    }

    fn veneer_size(self) -> CodeOffset {
        4
    }

    fn worst_case_veneer_size() -> CodeOffset {
        4
    }

    fn generate_veneer(self, buffer: &mut [u8], veneer_offset: CodeOffset) -> (CodeOffset, Self) {
        match self {
            LabelUse::Branch16 => {
                // The veneer is a plain `b` that the conditional branch
                // now targets instead.
                buffer[0..4].copy_from_slice(&enc_b(0, false).to_le_bytes());
                (veneer_offset, LabelUse::Branch26)
            }
            LabelUse::Branch26 | LabelUse::PCRelHiLo => unreachable!(),
        }
    }

    fn from_reloc(reloc: Reloc, addend: Addend) -> Option<LabelUse> {
        match (reloc, addend) {
            (Reloc::Ppc64Call, 0) => Some(LabelUse::Branch26),
            _ => None,
        }
    }
}
