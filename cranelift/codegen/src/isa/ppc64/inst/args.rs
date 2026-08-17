//! ppc64 instruction arguments: addressing modes and compare kinds.

use crate::ir::condcodes::CondCode;
use crate::isa::ppc64::inst::*;
use crate::machinst::StackAMode;

/// An addressing mode. PPC64's D-form reaches ±32 KiB from a base
/// register (with a 4-alignment restriction on the 64-bit `ld`/`std`
/// DS-forms); anything beyond that is emitted as an X-form indexed
/// access with the offset materialized into r0.
#[derive(Clone, Copy, Debug)]
pub enum AMode {
    /// Arbitrary offset from a register.
    RegOffset(Reg, i64),
    /// Offset from the stack pointer.
    SPOffset(i64),
    /// Offset from the frame pointer.
    #[expect(dead_code, reason = "will be used by future lowerings")]
    FPOffset(i64),
    /// Offset into the slot area of the stack, which lies just above the
    /// outgoing argument area that's setup by the function prologue.
    /// At emission time, this is converted to `SPOffset`.
    SlotOffset(i64),
    /// Offset into the argument area.
    IncomingArg(i64),
}

impl AMode {
    /// Add the registers referenced by this AMode to `collector`.
    pub(crate) fn get_operands(&mut self, collector: &mut impl OperandVisitor) {
        match self {
            AMode::RegOffset(reg, ..) => collector.reg_use(reg),
            // Registers used in these modes aren't allocatable.
            AMode::SPOffset(..)
            | AMode::FPOffset(..)
            | AMode::SlotOffset(..)
            | AMode::IncomingArg(..) => {}
        }
    }

    /// Resolve to a (base register, offset) pair, given the frame layout.
    pub(crate) fn to_base_and_offset(self, frame: &FrameLayout) -> (Reg, i64) {
        match self {
            AMode::RegOffset(reg, off) => (reg, off),
            AMode::SPOffset(off) => (stack_reg(), off),
            AMode::FPOffset(off) => (fp_reg(), off),
            AMode::SlotOffset(off) => {
                (stack_reg(), off + i64::from(frame.outgoing_args_size))
            }
            // Compute the offset into the incoming argument area relative
            // to SP, through the whole frame.
            AMode::IncomingArg(off) => {
                let sp_offset = frame.tail_args_size
                    + frame.setup_area_size
                    + frame.clobber_size
                    + frame.fixed_frame_storage_size
                    + frame.outgoing_args_size;
                (stack_reg(), i64::from(sp_offset) - off)
            }
        }
    }
}

impl From<StackAMode> for AMode {
    fn from(stack: StackAMode) -> AMode {
        match stack {
            StackAMode::IncomingArg(offset, stack_args_size) => {
                AMode::IncomingArg(i64::from(stack_args_size) - offset)
            }
            StackAMode::OutgoingArg(offset) => AMode::SPOffset(offset),
            StackAMode::Slot(offset) => AMode::SlotOffset(offset),
        }
    }
}

impl core::fmt::Display for AMode {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            AMode::RegOffset(r, off) => write!(f, "{off}({})", reg_name(*r)),
            AMode::SPOffset(off) => write!(f, "{off}(sp)"),
            AMode::FPOffset(off) => write!(f, "{off}(fp)"),
            AMode::SlotOffset(off) => write!(f, "{off}(slot)"),
            AMode::IncomingArg(off) => write!(f, "{off}(incoming_arg)"),
        }
    }
}

/// A comparison to be performed with `cmp`/`cmpl` (or their 32-bit
/// forms), consumed by a conditional branch, select, or trap within the
/// same MInst. The condition-register field used is always cr0.
#[derive(Clone, Copy, Debug)]
pub struct IntegerCompare {
    pub(crate) kind: IntCC,
    pub(crate) rs1: Reg,
    pub(crate) rs2: Reg,
    /// Compare on 64 bits (`cmpd`) rather than 32 (`cmpw`).
    pub(crate) is_64: bool,
}

/// cr0 bit indices, as used in the BI field of `bc` and the BC field of
/// `isel`.
pub(crate) const CR0_LT: u32 = 0;
pub(crate) const CR0_GT: u32 = 1;
pub(crate) const CR0_EQ: u32 = 2;

impl IntegerCompare {
    /// Is this a signed comparison (`cmp`) as opposed to logical (`cmpl`)?
    pub(crate) fn is_signed(&self) -> bool {
        match self.kind {
            IntCC::SignedLessThan
            | IntCC::SignedLessThanOrEqual
            | IntCC::SignedGreaterThan
            | IntCC::SignedGreaterThanOrEqual => true,
            _ => false,
        }
    }

    /// Return the cr0 bit to test and whether the branch/select should
    /// trigger when the bit is set (true) or clear (false).
    pub(crate) fn bit_and_polarity(&self) -> (u32, bool) {
        match self.kind {
            IntCC::Equal => (CR0_EQ, true),
            IntCC::NotEqual => (CR0_EQ, false),
            IntCC::SignedLessThan | IntCC::UnsignedLessThan => (CR0_LT, true),
            IntCC::SignedGreaterThanOrEqual | IntCC::UnsignedGreaterThanOrEqual => {
                (CR0_LT, false)
            }
            IntCC::SignedGreaterThan | IntCC::UnsignedGreaterThan => (CR0_GT, true),
            IntCC::SignedLessThanOrEqual | IntCC::UnsignedLessThanOrEqual => (CR0_GT, false),
        }
    }

    pub(crate) fn inverse(self) -> Self {
        Self {
            kind: self.kind.complement(),
            ..self
        }
    }
}

/// cr0 bit index for the "unordered" result of a floating-point compare.
pub(crate) const CR0_UN: u32 = 3;

/// A floating-point comparison, consumed by a branch or select within the
/// same MInst. `fcmpu` records four mutually exclusive results in cr0 —
/// less-than, greater-than, equal and unordered — and each `FloatCC` is a
/// set of those results, so some conditions need two bits combined with a
/// `cror` before they can be tested.
#[derive(Clone, Copy, Debug)]
pub struct FloatCompare {
    pub(crate) kind: FloatCC,
    pub(crate) rs1: Reg,
    pub(crate) rs2: Reg,
}

impl FloatCompare {
    /// The cr0 bit to test, whether the condition holds when that bit is
    /// set, and an optional `cror (dest, a, b)` to evaluate beforehand.
    ///
    /// Conditions covering three of the four results are expressed as the
    /// complement of the fourth rather than two `cror`s. The `cror`
    /// destination deliberately overwrites one of the inputs, which is
    /// dead once the combined bit has been formed.
    pub(crate) fn cr_plan(&self) -> (Option<(u32, u32, u32)>, u32, bool) {
        use FloatCC::*;
        match self.kind {
            // Single result.
            Equal => (None, CR0_EQ, true),
            LessThan => (None, CR0_LT, true),
            GreaterThan => (None, CR0_GT, true),
            Unordered => (None, CR0_UN, true),
            // The complement of a single result.
            NotEqual => (None, CR0_EQ, false),
            Ordered => (None, CR0_UN, false),
            UnorderedOrLessThanOrEqual => (None, CR0_GT, false),
            UnorderedOrGreaterThanOrEqual => (None, CR0_LT, false),
            // Two results combined.
            LessThanOrEqual => (Some((CR0_EQ, CR0_LT, CR0_EQ)), CR0_EQ, true),
            GreaterThanOrEqual => (Some((CR0_EQ, CR0_GT, CR0_EQ)), CR0_EQ, true),
            OrderedNotEqual => (Some((CR0_LT, CR0_LT, CR0_GT)), CR0_LT, true),
            UnorderedOrEqual => (Some((CR0_EQ, CR0_UN, CR0_EQ)), CR0_EQ, true),
            UnorderedOrLessThan => (Some((CR0_LT, CR0_UN, CR0_LT)), CR0_LT, true),
            UnorderedOrGreaterThan => (Some((CR0_GT, CR0_UN, CR0_GT)), CR0_GT, true),
        }
    }

}

/// A conditional branch target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CondBrTarget {
    /// An unresolved reference to a label.
    Label(MachLabel),
    /// No jump; fall through to the next instruction.
    Fallthrough,
}

impl core::fmt::Display for CondBrTarget {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            CondBrTarget::Label(l) => write!(f, "{l:?}"),
            CondBrTarget::Fallthrough => write!(f, "0"),
        }
    }
}

impl LoadOP {
    pub(crate) fn from_type(ty: Type) -> Self {
        match ty {
            I8 => Self::Lbz,
            I16 => Self::Lhz,
            I32 => Self::Lwz,
            I64 => Self::Ld,
            F32 => Self::Lfs,
            F64 => Self::Lfd,
            _ => unimplemented!("ppc64 load for type {ty}"),
        }
    }

    #[expect(dead_code, reason = "will be used by FP lowerings")]
    pub(crate) fn is_fpr(&self) -> bool {
        matches!(self, Self::Lfs | Self::Lfd)
    }
}

impl StoreOP {
    pub(crate) fn from_type(ty: Type) -> Self {
        match ty {
            I8 => Self::Stb,
            I16 => Self::Sth,
            I32 => Self::Stw,
            I64 => Self::Std,
            F32 => Self::Stfs,
            F64 => Self::Stfd,
            _ => unimplemented!("ppc64 store for type {ty}"),
        }
    }

    #[expect(dead_code, reason = "will be used by FP lowerings")]
    pub(crate) fn is_fpr(&self) -> bool {
        matches!(self, Self::Stfs | Self::Stfd)
    }
}
