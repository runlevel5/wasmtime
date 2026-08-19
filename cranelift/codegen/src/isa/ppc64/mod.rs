//! ppc64 (64-bit PowerPC, little-endian, ELFv2) Instruction Set
//! Architecture.

use crate::dominator_tree::DominatorTree;
use crate::ir::{Function, Type};
use crate::isa::ppc64::settings as ppc64_settings;
use crate::isa::{
    Builder as IsaBuilder, FunctionAlignment, IsaFlagsHashKey, OwnedTargetIsa, TargetIsa,
};
#[cfg(feature = "unwind")]
use crate::isa::unwind::systemv;
#[cfg(feature = "unwind")]
use crate::machinst::CompiledCode;
use crate::machinst::{
    CompiledCodeStencil, MachInst, MachTextSectionBuilder, Reg, SigSet, TextSectionBuilder, VCode,
    compile,
};
use crate::result::CodegenResult;
use crate::settings::{self as shared_settings, Flags};
use crate::ir;
use alloc::string::String;
use alloc::{boxed::Box, vec::Vec};
use core::fmt;
use cranelift_control::ControlPlane;
use target_lexicon::{Architecture, Triple};

mod abi;
pub(crate) mod inst;
mod lower;
mod settings;

use self::inst::EmitInfo;

/// A ppc64 backend.
pub struct Ppc64Backend {
    triple: Triple,
    flags: shared_settings::Flags,
    isa_flags: ppc64_settings::Flags,
}

impl Ppc64Backend {
    /// Create a new ppc64 backend with the given (shared) flags.
    pub fn new_with_flags(
        triple: Triple,
        flags: shared_settings::Flags,
        isa_flags: ppc64_settings::Flags,
    ) -> Ppc64Backend {
        Ppc64Backend {
            triple,
            flags,
            isa_flags,
        }
    }

    /// This performs lowering to VCode, register-allocates the code,
    /// computes block layout and finalizes branches. The result is ready
    /// for binary emission.
    fn compile_vcode(
        &self,
        func: &Function,
        domtree: &DominatorTree,
        regalloc_ctx: &mut regalloc2::Ctx,
        ctrl_plane: &mut ControlPlane,
    ) -> CodegenResult<VCode<inst::Inst>> {
        let emit_info = EmitInfo::new(self.flags.clone(), self.isa_flags.clone());
        let sigs = SigSet::new::<abi::Ppc64MachineDeps>(func, &self.flags)?;
        let abi = abi::Ppc64Callee::new(func, self, &self.isa_flags, &sigs)?;
        compile::compile::<Ppc64Backend>(
            func,
            domtree,
            regalloc_ctx,
            self,
            abi,
            emit_info,
            sigs,
            ctrl_plane,
        )
    }
}

impl TargetIsa for Ppc64Backend {
    fn compile_function(
        &self,
        func: &Function,
        domtree: &DominatorTree,
        regalloc_ctx: &mut regalloc2::Ctx,
        want_disasm: bool,
        ctrl_plane: &mut ControlPlane,
    ) -> CodegenResult<CompiledCodeStencil> {
        let vcode = self.compile_vcode(func, domtree, regalloc_ctx, ctrl_plane)?;

        let want_disasm = want_disasm || log::log_enabled!(log::Level::Debug);
        let emit_result = vcode.emit(&regalloc_ctx.output, want_disasm, &self.flags, ctrl_plane)?;
        let value_labels_ranges = emit_result.value_labels_ranges;
        let buffer = emit_result.buffer;

        if let Some(disasm) = emit_result.disasm.as_ref() {
            log::debug!("disassembly:\n{disasm}");
        }

        Ok(CompiledCodeStencil {
            buffer,
            vcode: emit_result.disasm,
            value_labels_ranges,
            bb_starts: emit_result.bb_offsets,
            bb_edges: emit_result.bb_edges,
        })
    }

    fn name(&self) -> &'static str {
        "ppc64"
    }

    fn dynamic_vector_bytes(&self, _dynamic_ty: ir::Type) -> u32 {
        16
    }

    fn triple(&self) -> &Triple {
        &self.triple
    }

    fn flags(&self) -> &shared_settings::Flags {
        &self.flags
    }

    fn isa_flags(&self) -> Vec<shared_settings::Value> {
        self.isa_flags.iter().collect()
    }

    fn isa_flags_hash_key(&self) -> IsaFlagsHashKey<'_> {
        IsaFlagsHashKey(self.isa_flags.hash_key())
    }

    #[cfg(feature = "unwind")]
    fn emit_unwind_info(
        &self,
        result: &CompiledCode,
        kind: crate::isa::unwind::UnwindInfoKind,
    ) -> CodegenResult<Option<crate::isa::unwind::UnwindInfo>> {
        use crate::isa::unwind::UnwindInfo;
        use crate::isa::unwind::UnwindInfoKind;
        Ok(match kind {
            UnwindInfoKind::SystemV => {
                let mapper = self::inst::unwind::systemv::RegisterMapper;
                Some(UnwindInfo::SystemV(
                    crate::isa::unwind::systemv::create_unwind_info_from_insts(
                        &result.buffer.unwind_info[..],
                        result.buffer.data().len(),
                        &mapper,
                    )?,
                ))
            }
            // There is no Windows on ppc64.
            _ => None,
        })
    }

    #[cfg(feature = "unwind")]
    fn create_systemv_cie(&self) -> Option<gimli::write::CommonInformationEntry> {
        Some(inst::unwind::systemv::create_cie())
    }

    fn text_section_builder(&self, num_funcs: usize) -> Box<dyn TextSectionBuilder> {
        Box::new(MachTextSectionBuilder::<inst::Inst>::new(num_funcs))
    }

    #[cfg(feature = "unwind")]
    fn map_regalloc_reg_to_dwarf(&self, reg: Reg) -> Result<u16, systemv::RegisterMappingError> {
        inst::unwind::systemv::map_reg(reg).map(|reg| reg.0)
    }

    fn function_alignment(&self) -> FunctionAlignment {
        inst::Inst::function_alignment()
    }

    fn page_size_align_log2(&self) -> u8 {
        // Linux on ppc64le uses 64 KiB pages by default.
        debug_assert_eq!(1 << 16, 0x10000);
        16
    }

    #[cfg(feature = "disas")]
    fn to_capstone(&self) -> Result<capstone::Capstone, capstone::Error> {
        use capstone::prelude::*;
        let mut cs = Capstone::new()
            .ppc()
            .mode(arch::ppc::ArchMode::Mode64)
            .endian(capstone::Endian::Little)
            .build()?;
        // Skip over inline constants (e.g. LoadExtName literals) instead
        // of stopping at bytes capstone can't decode.
        cs.set_skipdata(true)?;
        Ok(cs)
    }

    fn pretty_print_reg(&self, reg: Reg, _size: u8) -> String {
        inst::regs::reg_name(reg)
    }

    fn has_native_fma(&self) -> bool {
        // fmadd and friends have been part of the ISA since POWER1, but
        // the lowering rules don't use them yet.
        false
    }

    fn has_round(&self) -> bool {
        true
    }

    fn has_blendv_lowering(&self, _: Type) -> bool {
        false
    }

    fn has_x86_pshufb_lowering(&self) -> bool {
        false
    }

    fn has_x86_pmulhrsw_lowering(&self) -> bool {
        false
    }

    fn has_x86_pmaddubsw_lowering(&self) -> bool {
        false
    }

    fn default_argument_extension(&self) -> ir::ArgumentExtension {
        // ELFv2 extends sub-doubleword arguments per their signedness;
        // Cranelift signatures carry explicit uext/sext annotations when
        // interop requires it, so no implicit extension is applied here.
        ir::ArgumentExtension::None
    }
}

impl fmt::Display for Ppc64Backend {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("MachBackend")
            .field("name", &self.name())
            .field("triple", &self.triple())
            .field("flags", &format!("{}", self.flags()))
            .finish()
    }
}

/// Create a new `isa::Builder`.
pub fn isa_builder(triple: Triple) -> IsaBuilder {
    match triple.architecture {
        Architecture::Powerpc64le => {}
        _ => unreachable!(),
    }
    IsaBuilder {
        triple,
        setup: ppc64_settings::builder(),
        constructor: isa_constructor,
    }
}

fn isa_constructor(
    triple: Triple,
    shared_flags: Flags,
    builder: &shared_settings::Builder,
) -> CodegenResult<OwnedTargetIsa> {
    let isa_flags = ppc64_settings::Flags::new(&shared_flags, builder);
    let backend = Ppc64Backend::new_with_flags(triple, shared_flags, isa_flags);
    Ok(backend.wrapped())
}
