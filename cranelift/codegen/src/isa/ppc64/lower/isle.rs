//! ISLE integration glue code for ppc64 lowering.

// Pull in the ISLE generated code.
pub mod generated_code;
use generated_code::MInst;

// Types that the generated ISLE code uses via `use super::*`.
use crate::ir::condcodes::{FloatCC, IntCC};
use crate::ir::{
    BlockCall, ExternalName, Inst, InstructionData, MemFlagsData, Opcode, TrapCode, Value,
    ValueList, immediates::*, types::*,
};
use crate::isa::ppc64::Ppc64Backend;
use crate::isa::ppc64::inst::*;
use crate::machinst::{
    ArgPair, CallArgList, CallInfo, CallRetList, InstOutput, MachInst, Reg, RetPair,
    VCodeConstant, VCodeConstantData, isle::*,
};
use alloc::boxed::Box;
use alloc::vec::Vec;
use regalloc2::PReg;

type BoxCallInfo = Box<CallInfo<ExternalName>>;
type BoxCallIndInfo = Box<CallInfo<Reg>>;
type BoxReturnCallInfo = Box<ReturnCallInfo<ExternalName>>;
type BoxReturnCallIndInfo = Box<ReturnCallInfo<Reg>>;
type BoxExternalName = Box<ExternalName>;
type VecArgPair = Vec<ArgPair>;
type VecRetPair = Vec<RetPair>;
type VecMachLabel = Vec<MachLabel>;

pub(crate) struct Ppc64IsleContext<'a, 'b, I, B>
where
    I: VCodeInst,
    B: LowerBackend,
{
    pub lower_ctx: &'a mut Lower<'b, I>,
    pub backend: &'a B,
}

impl<'a, 'b> Ppc64IsleContext<'a, 'b, MInst, Ppc64Backend> {
    fn new(lower_ctx: &'a mut Lower<'b, MInst>, backend: &'a Ppc64Backend) -> Self {
        Self { lower_ctx, backend }
    }

    pub(crate) fn dfg(&self) -> &crate::ir::DataFlowGraph {
        &self.lower_ctx.f.dfg
    }
}

impl generated_code::Context for Ppc64IsleContext<'_, '_, MInst, Ppc64Backend> {
    isle_lower_prelude_methods!();

    #[inline]
    fn emit(&mut self, arg0: &MInst) -> Unit {
        self.lower_ctx.emit(arg0.clone());
    }

    fn gen_call_info(
        &mut self,
        sig: Sig,
        dest: ExternalName,
        uses: CallArgList,
        defs: CallRetList,
        try_call_info: Option<TryCallInfo>,
        patchable: bool,
    ) -> BoxCallInfo {
        let stack_ret_space = self.lower_ctx.sigs()[sig].sized_stack_ret_space();
        let stack_arg_space = self.lower_ctx.sigs()[sig].sized_stack_arg_space();
        self.lower_ctx
            .abi_mut()
            .accumulate_outgoing_args_size(stack_ret_space + stack_arg_space);

        Box::new(
            self.lower_ctx
                .gen_call_info(sig, dest, uses, defs, try_call_info, patchable),
        )
    }

    fn gen_call_ind_info(
        &mut self,
        sig: Sig,
        dest: Reg,
        uses: CallArgList,
        defs: CallRetList,
        try_call_info: Option<TryCallInfo>,
    ) -> BoxCallIndInfo {
        let stack_ret_space = self.lower_ctx.sigs()[sig].sized_stack_ret_space();
        let stack_arg_space = self.lower_ctx.sigs()[sig].sized_stack_arg_space();
        self.lower_ctx
            .abi_mut()
            .accumulate_outgoing_args_size(stack_ret_space + stack_arg_space);

        Box::new(
            self.lower_ctx
                .gen_call_info(sig, dest, uses, defs, try_call_info, false),
        )
    }

    fn gen_return_call_info(
        &mut self,
        sig: Sig,
        dest: ExternalName,
        uses: CallArgList,
    ) -> BoxReturnCallInfo {
        let new_stack_arg_size = self.lower_ctx.sigs()[sig].sized_stack_arg_space();
        self.lower_ctx
            .abi_mut()
            .accumulate_tail_args_size(new_stack_arg_size);
        Box::new(ReturnCallInfo {
            dest,
            uses,
            new_stack_arg_size,
        })
    }

    fn gen_return_call_ind_info(
        &mut self,
        sig: Sig,
        dest: Reg,
        uses: CallArgList,
    ) -> BoxReturnCallIndInfo {
        let new_stack_arg_size = self.lower_ctx.sigs()[sig].sized_stack_arg_space();
        self.lower_ctx
            .abi_mut()
            .accumulate_tail_args_size(new_stack_arg_size);
        Box::new(ReturnCallInfo {
            dest,
            uses,
            new_stack_arg_size,
        })
    }

    fn amode(&mut self, addr: Value, offset: i32) -> AMode {
        AMode::RegOffset(self.put_in_reg(addr), i64::from(offset))
    }

    /// The high doubleword of an `i128` access: eight bytes past the
    /// low one. The offset is widened to `i64` first, so it cannot
    /// overflow.
    fn amode8(&mut self, addr: Value, offset: i32) -> AMode {
        AMode::RegOffset(self.put_in_reg(addr), i64::from(offset) + 8)
    }

    /// The comparison to run on the high halves of an ordered 128-bit
    /// compare: the strict version of the condition, keeping its
    /// signedness. Fails on equal/not-equal, which take another path.
    fn icmp128_hi_cc(&mut self, cc: &IntCC) -> Option<IntCC> {
        Some(match cc {
            IntCC::SignedLessThan | IntCC::SignedLessThanOrEqual => IntCC::SignedLessThan,
            IntCC::SignedGreaterThan | IntCC::SignedGreaterThanOrEqual => {
                IntCC::SignedGreaterThan
            }
            IntCC::UnsignedLessThan | IntCC::UnsignedLessThanOrEqual => IntCC::UnsignedLessThan,
            IntCC::UnsignedGreaterThan | IntCC::UnsignedGreaterThanOrEqual => {
                IntCC::UnsignedGreaterThan
            }
            IntCC::Equal | IntCC::NotEqual => return None,
        })
    }

    /// The comparison for the low halves when the high halves are
    /// equal: always unsigned, keeping the original strictness.
    fn icmp128_lo_cc(&mut self, cc: &IntCC) -> Option<IntCC> {
        Some(match cc {
            IntCC::SignedLessThan | IntCC::UnsignedLessThan => IntCC::UnsignedLessThan,
            IntCC::SignedLessThanOrEqual | IntCC::UnsignedLessThanOrEqual => {
                IntCC::UnsignedLessThanOrEqual
            }
            IntCC::SignedGreaterThan | IntCC::UnsignedGreaterThan => IntCC::UnsignedGreaterThan,
            IntCC::SignedGreaterThanOrEqual | IntCC::UnsignedGreaterThanOrEqual => {
                IntCC::UnsignedGreaterThanOrEqual
            }
            IntCC::Equal | IntCC::NotEqual => return None,
        })
    }

    fn load_op_for_type(&mut self, ty: Type) -> LoadOP {
        LoadOP::from_type(ty)
    }

    fn store_op_for_type(&mut self, ty: Type) -> StoreOP {
        StoreOP::from_type(ty)
    }

    fn int_compare(&mut self, cc: &IntCC, rs1: Reg, rs2: Reg, ty: Type) -> IntegerCompare {
        IntegerCompare {
            kind: *cc,
            rs1,
            rs2,
            is_64: ty == I64,
        }
    }

    fn cc_is_signed(&mut self, cc: &IntCC) -> bool {
        match cc {
            IntCC::SignedLessThan
            | IntCC::SignedLessThanOrEqual
            | IntCC::SignedGreaterThan
            | IntCC::SignedGreaterThanOrEqual => true,
            _ => false,
        }
    }

    fn float_compare(&mut self, cc: &FloatCC, rs1: Reg, rs2: Reg) -> FloatCompare {
        FloatCompare {
            kind: *cc,
            rs1,
            rs2,
        }
    }

    fn f32_bits_as_f64(&mut self, bits: u32) -> u64 {
        f64::from(f32::from_bits(bits)).to_bits()
    }

    fn cond_br_target(&mut self, label: MachLabel) -> CondBrTarget {
        CondBrTarget::Label(label)
    }

    fn simm16_from_imm64(&mut self, imm: Imm64) -> Option<u16> {
        i16::try_from(i64::from(imm)).ok().map(|i| i as u16)
    }

    fn shift_imm(&mut self, n: u64, ty: Type) -> u8 {
        (n & u64::from(ty.bits() - 1)) as u8
    }

    fn rot_imm_neg(&mut self, n: u64, ty: Type) -> u8 {
        let bits = u64::from(ty.bits());
        ((bits - (n & (bits - 1))) & (bits - 1)) as u8
    }

    fn shift_mask_u16(&mut self, ty: Type) -> u16 {
        u16::try_from(ty.bits() - 1).unwrap()
    }

    /// `cls` on an N-bit type is `clz64(sext(x) ^ (sext(x) >> 63)) -
    /// (65 - N)`: the XOR turns leading sign bits into leading zeros
    /// (plus the sign bit itself, hence the extra 1), and sign
    /// extension makes the count width-independent.
    fn cls_adjust(&mut self, ty: Type) -> u16 {
        (ty.bits() as i16 - 65) as u16
    }

    fn clz_narrow_adjust(&mut self, ty: Type) -> u16 {
        // The count is taken over a 64-bit zero-extension, so subtract the
        // bits the extension added.
        (-((64 - ty.bits()) as i16)) as u16
    }

    fn ty_width_bit(&mut self, ty: Type) -> u64 {
        1u64 << ty.bits()
    }

    fn atomic_rmw_src(&mut self, op: &AtomicRmwOp, ty: Type, val: Value) -> Reg {
        // Only the min/max comparisons look at bits above the type's
        // width, so only they need the operand extended.
        let src = self.put_in_reg(val);
        let (signed, needs_ext) = match op {
            AtomicRmwOp::Smin | AtomicRmwOp::Smax => (true, ty != I64),
            AtomicRmwOp::Umin | AtomicRmwOp::Umax => (false, ty != I64),
            _ => return src,
        };
        if !needs_ext {
            return src;
        }
        let dst = self.temp_writable_reg(I64);
        self.lower_ctx.emit(MInst::Extend {
            rd: dst,
            rn: src,
            signed,
            from_bits: ty.bits() as u8,
            to_bits: 64,
        });
        dst.to_reg()
    }

    fn gen_stack_addr(&mut self, slot: StackSlot, offset: Offset32) -> Reg {
        let result = self.temp_writable_reg(I64);
        let i = self
            .lower_ctx
            .abi()
            .sized_stackslot_addr(slot, i64::from(offset) as u32, result);
        self.emit(&i);
        result.to_reg()
    }

    fn lower_br_table(&mut self, index: Reg, targets: &[MachLabel]) -> Unit {
        let tmp1 = self.temp_writable_reg(I64);
        let tmp2 = self.temp_writable_reg(I64);
        self.emit(&MInst::BrTable {
            index,
            tmp1,
            tmp2,
            targets: targets.to_vec(),
        });
    }

    fn read_return_address(&mut self) -> Reg {
        let dst = self.temp_writable_reg(I64);
        self.lower_ctx.emit(MInst::gen_load(
            dst,
            AMode::FPOffset(8),
            I64,
            MemFlagsData::trusted(),
        ));
        dst.to_reg()
    }

    fn load_ext_name(&mut self, name: ExternalName, offset: i64) -> Reg {
        let dst = self.temp_writable_reg(I64);
        self.lower_ctx.emit(MInst::LoadExtName {
            rd: dst,
            name: Box::new(name),
            offset,
        });
        dst.to_reg()
    }
}

/// The main entry point for lowering with ISLE.
pub(crate) fn lower(
    lower_ctx: &mut Lower<MInst>,
    backend: &Ppc64Backend,
    inst: Inst,
) -> Option<InstOutput> {
    let mut isle_ctx = Ppc64IsleContext::new(lower_ctx, backend);
    generated_code::constructor_lower(&mut isle_ctx, inst)
}

/// The main entry point for branch lowering with ISLE.
pub(crate) fn lower_branch(
    lower_ctx: &mut Lower<MInst>,
    backend: &Ppc64Backend,
    branch: Inst,
    targets: &[MachLabel],
) -> Option<()> {
    let mut isle_ctx = Ppc64IsleContext::new(lower_ctx, backend);
    generated_code::constructor_lower_branch(&mut isle_ctx, branch, targets)
}
