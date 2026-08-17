//! Unwind information for System V ABI (ppc64).
//!
//! DWARF register numbering per the ELFv2 ABI: GPRs are 0-31, FPRs are
//! 32-63, LR is 65, VRs are 77-108.

use crate::isa::ppc64::inst::regs;
use crate::isa::unwind::systemv::RegisterMappingError;
use crate::machinst::Reg;
use gimli::{Encoding, Format, Register, write::CommonInformationEntry};
use regalloc2::RegClass;

/// DWARF register number for the link register.
const DWARF_LR: u16 = 65;

/// Creates a new ppc64 common information entry (CIE).
pub fn create_cie() -> CommonInformationEntry {
    use gimli::write::CallFrameInstruction;

    let mut entry = CommonInformationEntry::new(
        Encoding {
            address_size: 8,
            format: Format::Dwarf32,
            version: 1,
        },
        4,  // Code alignment factor
        -8, // Data alignment factor
        Register(DWARF_LR),
    );

    // Every frame will start with the call frame address (CFA) at SP.
    let sp = Register(regs::stack_reg().to_real_reg().unwrap().hw_enc().into());
    entry.add_instruction(CallFrameInstruction::Cfa(sp, 0));

    entry
}

/// Map Cranelift registers to their corresponding Gimli registers.
pub fn map_reg(reg: Reg) -> Result<Register, RegisterMappingError> {
    let reg_offset = match reg.class() {
        RegClass::Int => 0,
        RegClass::Float => 32,
        RegClass::Vector => 77,
    };
    let reg = u16::from(reg.to_real_reg().unwrap().hw_enc());
    Ok(Register(reg_offset + reg))
}

pub(crate) struct RegisterMapper;

impl crate::isa::unwind::systemv::RegisterMapper<Reg> for RegisterMapper {
    fn map(&self, reg: Reg) -> Result<u16, RegisterMappingError> {
        Ok(map_reg(reg)?.0)
    }
    fn fp(&self) -> Option<u16> {
        Some(regs::fp_reg().to_real_reg().unwrap().hw_enc().into())
    }
    fn lr(&self) -> Option<u16> {
        Some(DWARF_LR)
    }
    fn lr_offset(&self) -> Option<u32> {
        // The return address is stored at [FP+8] by the prologue.
        Some(8)
    }
}
