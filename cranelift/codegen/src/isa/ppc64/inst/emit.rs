//! ppc64 ISA: binary code emission.

use crate::ir;
use crate::isa::ppc64::abi::Ppc64MachineDeps;
use crate::isa::ppc64::inst::*;
use cranelift_control::ControlPlane;

pub struct EmitInfo {
    #[expect(dead_code, reason = "will gate ISA-3.0 fast paths later")]
    shared_flags: settings::Flags,
    isa_flags: crate::isa::ppc64::settings::Flags,
}

impl EmitInfo {
    pub(crate) fn new(
        shared_flags: settings::Flags,
        isa_flags: crate::isa::ppc64::settings::Flags,
    ) -> Self {
        Self {
            shared_flags,
            isa_flags,
        }
    }
}

/// State carried between emissions of a sequence of instructions.
#[derive(Default, Clone, Debug)]
pub struct EmitState {
    /// The user stack map for the upcoming instruction, as provided to
    /// `pre_safepoint()`.
    user_stack_map: Option<ir::UserStackMap>,
    ctrl_plane: ControlPlane,
    frame_layout: FrameLayout,
}

impl EmitState {
    fn take_stack_map(&mut self) -> Option<ir::UserStackMap> {
        self.user_stack_map.take()
    }
}

impl MachInstEmitState<Inst> for EmitState {
    fn new(abi: &Callee<Ppc64MachineDeps>, ctrl_plane: ControlPlane) -> Self {
        EmitState {
            user_stack_map: None,
            ctrl_plane,
            frame_layout: abi.frame_layout().clone(),
        }
    }

    fn pre_safepoint(&mut self, user_stack_map: Option<ir::UserStackMap>) {
        self.user_stack_map = user_stack_map;
    }

    fn ctrl_plane_mut(&mut self) -> &mut ControlPlane {
        &mut self.ctrl_plane
    }

    fn take_ctrl_plane(self) -> ControlPlane {
        self.ctrl_plane
    }

    fn frame_layout(&self) -> &FrameLayout {
        &self.frame_layout
    }
}

impl Inst {
    /// Produce the shortest instruction sequence that materializes `imm`
    /// into GPR number `rd`.
    pub(crate) fn load_constant_words(rd: u32, imm: u64) -> SmallVec<[u32; 5]> {
        let mut words = SmallVec::new();
        let val = imm as i64;
        if let Ok(imm16) = i16::try_from(val) {
            // li rd, imm16
            words.push(enc_d(14, rd, 0, imm16 as u16));
        } else if let Ok(imm32) = i32::try_from(val) {
            // lis rd, hi; [ori rd, rd, lo]
            words.push(enc_d(15, rd, 0, (imm32 >> 16) as u16));
            if imm32 as u16 != 0 {
                words.push(enc_d_logic(24, rd, rd, imm32 as u16));
            }
        } else {
            // Full 64-bit build: assemble the high 32 bits, shift them
            // into place (which also discards `lis`'s sign extension),
            // then OR in the low 32 bits.
            words.push(enc_d(15, rd, 0, (imm >> 48) as u16)); // lis
            if (imm >> 32) as u16 != 0 {
                words.push(enc_d_logic(24, rd, rd, (imm >> 32) as u16)); // ori
            }
            words.push(enc_md(rd, rd, 32, 31, 1)); // rldicr rd, rd, 32, 31
            if (imm >> 16) as u16 != 0 {
                words.push(enc_d_logic(25, rd, rd, (imm >> 16) as u16)); // oris
            }
            if imm as u16 != 0 {
                words.push(enc_d_logic(24, rd, rd, imm as u16)); // ori
            }
        }
        words
    }
}

/// Resolve an AMode and emit the D/DS-form access if the offset fits, or
/// materialize the offset into r0 and use the X-form indexed variant.
///
/// `(d_opcd, ds_xo)` describe the displacement form (`ds_xo` is `Some`
/// for the 4-aligned DS-forms), `x_xo` the indexed form. `rt` is the
/// data register.
fn emit_mem_access(
    sink: &mut MachBuffer<Inst>,
    state: &EmitState,
    mem: AMode,
    rt: u32,
    d_opcd: u32,
    ds_xo: Option<u32>,
    x_xo: u32,
) {
    let (base, offset) = mem.to_base_and_offset(state.frame_layout());
    let base_n = reg_num(base);
    debug_assert!(base_n != 0, "r0 is not a valid base register");
    let fits_d = i16::try_from(offset).is_ok();
    let ds_ok = ds_xo.is_none() || (offset & 3) == 0;
    if fits_d && ds_ok {
        let imm = offset as i16;
        match ds_xo {
            Some(xo2) => sink.put4(enc_ds(d_opcd, rt, base_n, imm, xo2)),
            None => sink.put4(enc_d(d_opcd, rt, base_n, imm as u16)),
        }
    } else {
        // Materialize the offset into r0 and use the indexed form; RB has
        // no reads-as-zero quirk.
        for w in Inst::load_constant_words(0, offset as u64) {
            sink.put4(w);
        }
        sink.put4(enc_x(rt, base_n, 0, x_xo));
    }
}

/// The VX-form extended opcode for a lane-width-dispatched vector ALU
/// operation. The doubleword compares sit one above their b/h/w
/// siblings' stride rather than continuing it, hence the explicit
/// table rather than arithmetic.
fn vx_xo(op: VecAluOp, ty: Type) -> u32 {
    let lane = ty.lane_bits();
    let idx = match lane {
        8 => 0,
        16 => 1,
        32 => 2,
        64 => 3,
        _ => unreachable!("vector lane width {ty}"),
    };
    if matches!(op, VecAluOp::RotlDword) {
        assert_eq!(lane, 64, "vrld operates on doubleword lanes");
    }
    if matches!(op, VecAluOp::MergeEvenWord) {
        assert_eq!(lane, 32, "vmrgew is a word-lane merge");
    }
    if matches!(
        op,
        VecAluOp::MulEvenS | VecAluOp::MulOddS | VecAluOp::MulEvenU | VecAluOp::MulOddU
    ) {
        assert_ne!(lane, 64, "a doubleword even/odd product would not fit");
    }
    if matches!(op, VecAluOp::PackMod) {
        assert_ne!(lane, 8, "nothing packs a byte lane into something narrower");
    }
    if matches!(op, VecAluOp::MulWord) {
        assert_eq!(lane, 32, "vmuluwm is a word-lane multiply");
    }
    if matches!(
        op,
        VecAluOp::PackSS | VecAluOp::PackSU | VecAluOp::PackUU
    ) {
        assert_ne!(lane, 8, "nothing packs a byte lane into something narrower");
    }
    if matches!(op, VecAluOp::MergeLow | VecAluOp::MergeHigh) {
        assert_ne!(lane, 64, "nothing widens a doubleword lane");
    }
    if matches!(
        op,
        VecAluOp::AvgRoundS
            | VecAluOp::AvgRoundU
            | VecAluOp::SAddSat
            | VecAluOp::UAddSat
            | VecAluOp::SSubSat
            | VecAluOp::USubSat
    ) {
        assert_ne!(
            lane, 64,
            "the ISA has no doubleword vector average or saturating arithmetic"
        );
    }
    let table: [u32; 4] = match op {
        VecAluOp::Add => [0, 64, 128, 192],
        VecAluOp::Sub => [1024, 1088, 1152, 1216],
        VecAluOp::CmpEq => [6, 70, 134, 199],
        VecAluOp::CmpGtS => [774, 838, 902, 967],
        VecAluOp::CmpGtU => [518, 582, 646, 711],
        VecAluOp::MinS => [770, 834, 898, 962],
        VecAluOp::MinU => [514, 578, 642, 706],
        VecAluOp::MaxS => [258, 322, 386, 450],
        VecAluOp::MaxU => [2, 66, 130, 194],
        VecAluOp::Shl => [260, 324, 388, 1476],
        VecAluOp::ShrU => [516, 580, 644, 1732],
        VecAluOp::ShrS => [772, 836, 900, 964],
        // No doubleword average exists; the lowering rules never ask.
        VecAluOp::AvgRoundS => [1282, 1346, 1410, 0],
        VecAluOp::AvgRoundU => [1026, 1090, 1154, 0],
        // Likewise no doubleword saturating arithmetic exists.
        VecAluOp::SAddSat => [768, 832, 896, 0],
        VecAluOp::UAddSat => [512, 576, 640, 0],
        VecAluOp::SSubSat => [1792, 1856, 1920, 0],
        VecAluOp::USubSat => [1536, 1600, 1664, 0],
        // Merges take the source lane width; there is no doubleword
        // form because nothing widens a doubleword lane.
        VecAluOp::MergeLow => [268, 332, 396, 0],
        VecAluOp::MergeHigh => [12, 76, 140, 0],
        // Packs are indexed by the *source* width, so the byte slot is
        // unused: nothing packs bytes into a narrower lane.
        VecAluOp::PackSS => [0, 398, 462, 1486],
        VecAluOp::PackSU => [0, 270, 334, 1358],
        VecAluOp::PackUU => [0, 142, 206, 1230],
        // `vmuluwm` is word-only.
        VecAluOp::MulWord => [0, 0, 137, 0],
        // Even/odd multiplies, indexed by source lane width; there is
        // no doubleword source because the result would not fit.
        VecAluOp::MulEvenS => [776, 840, 904, 0],
        VecAluOp::MulOddS => [264, 328, 392, 0],
        VecAluOp::MulEvenU => [520, 584, 648, 0],
        VecAluOp::MulOddU => [8, 72, 136, 0],
        // Truncating pack, indexed by source width; nothing packs a
        // byte lane.
        VecAluOp::PackMod => [0, 14, 78, 1102],
        VecAluOp::MergeEvenWord => [0, 0, 1932, 0],
        VecAluOp::RotlDword => [0, 0, 0, 196],
        VecAluOp::And
        | VecAluOp::Or
        | VecAluOp::Xor
        | VecAluOp::Nor
        | VecAluOp::AndC => {
            unreachable!("{op:?} is emitted as a VSX logical, not a VX form")
        }
    };
    table[idx]
}

/// Resolve an `AMode` for the X-form-only vector accesses: returns the
/// `(RA, RB)` fields, materializing the offset into r0 when non-zero.
/// With a zero offset the base goes in RB and RA is the literal zero,
/// so no scratch instruction is needed.
fn vec_mem_ea(sink: &mut MachBuffer<Inst>, state: &EmitState, mem: AMode) -> (u32, u32) {
    let (base, offset) = mem.to_base_and_offset(state.frame_layout());
    let base_n = reg_num(base);
    debug_assert!(base_n != 0, "r0 is not a valid base register");
    if offset == 0 {
        (0, base_n)
    } else {
        for w in Inst::load_constant_words(0, offset as u64) {
            sink.put4(w);
        }
        (base_n, 0)
    }
}

impl Inst {
    /// Expand a division or remainder, including the checks CLIF requires
    /// but PPC's divide instructions do not perform.
    ///
    /// PPC leaves `RT` *undefined* (rather than trapping) when the divisor
    /// is zero, and likewise for the signed `INT_MIN / -1` overflow, so
    /// both cases are branched around explicitly. The `-1` divisor is
    /// special-cased rather than checked-then-divided because it is the
    /// only value for which the hardware result is unusable: `x / -1` is
    /// `-x` and `x % -1` is `0` for every `x`.
    fn emit_divrem(
        sink: &mut MachBuffer<Inst>,
        emit_info: &EmitInfo,
        state: &mut EmitState,
        op: DivOp,
        rd: Writable<Reg>,
        ra: Reg,
        rb: Reg,
        ty: Type,
    ) {
        let is_64 = ty == I64;
        let rd_n = reg_num(rd.to_reg());
        let ra_n = reg_num(ra);
        let rb_n = reg_num(rb);
        let signed = matches!(op, DivOp::SDiv | DivOp::SRem);

        // Trap if the divisor is zero.
        let after_divz = sink.get_label();
        emit_cmpi(sink, rb_n, 0, is_64);
        emit_bc_to_label(sink, after_divz, CR0_EQ, false);
        Inst::Udf {
            trap_code: ir::TrapCode::INTEGER_DIVISION_BY_ZERO,
        }
        .emit(sink, emit_info, state);
        sink.bind_label(after_divz, &mut state.ctrl_plane);

        let done = sink.get_label();

        if signed {
            // The `-1` divisor path, which the hardware cannot be trusted
            // with for the minimum-value dividend.
            let normal = sink.get_label();
            emit_cmpi(sink, rb_n, -1, is_64);
            emit_bc_to_label(sink, normal, CR0_EQ, false);

            match op {
                DivOp::SDiv => {
                    // Trap on INT_MIN / -1, otherwise negate.
                    let min = if is_64 {
                        0x8000_0000_0000_0000
                    } else {
                        0xFFFF_FFFF_8000_0000
                    };
                    for w in Inst::load_constant_words(0, min) {
                        sink.put4(w);
                    }
                    let no_ovf = sink.get_label();
                    sink.put4(enc_cmp(0, u32::from(is_64), ra_n, 0, 0));
                    emit_bc_to_label(sink, no_ovf, CR0_EQ, false);
                    Inst::Udf {
                        trap_code: ir::TrapCode::INTEGER_OVERFLOW,
                    }
                    .emit(sink, emit_info, state);
                    sink.bind_label(no_ovf, &mut state.ctrl_plane);
                    sink.put4(enc_xo(rd_n, ra_n, 0, 104)); // neg rd, ra
                }
                DivOp::SRem => {
                    sink.put4(enc_d(14, rd_n, 0, 0)); // li rd, 0
                }
                _ => unreachable!(),
            }
            emit_b_to_label(sink, done);
            sink.bind_label(normal, &mut state.ctrl_plane);
        }

        match op {
            DivOp::SDiv => sink.put4(enc_xo(rd_n, ra_n, rb_n, if is_64 { 489 } else { 491 })),
            DivOp::UDiv => sink.put4(enc_xo(rd_n, ra_n, rb_n, if is_64 { 457 } else { 459 })),
            DivOp::SRem | DivOp::URem => {
                let unsigned = matches!(op, DivOp::URem);
                if emit_info.isa_flags.has_isa_3_0() {
                    // The modulo instructions are X-form, with a 10-bit
                    // extended opcode rather than the divides' 9-bit one.
                    let xo = match (unsigned, is_64) {
                        (false, true) => 777,  // modsd
                        (false, false) => 779, // modsw
                        (true, true) => 265,   // modud
                        (true, false) => 267,  // moduw
                    };
                    sink.put4(enc_x(rd_n, ra_n, rb_n, xo));
                } else {
                    // rd = ra - (ra / rb) * rb, via the r0 scratch.
                    let div_xo = match (unsigned, is_64) {
                        (false, true) => 489,
                        (false, false) => 491,
                        (true, true) => 457,
                        (true, false) => 459,
                    };
                    sink.put4(enc_xo(0, ra_n, rb_n, div_xo));
                    sink.put4(enc_xo(0, 0, rb_n, 233)); // mulld r0, r0, rb
                    sink.put4(enc_xo(rd_n, 0, ra_n, 40)); // subf rd, r0, ra
                }
            }
        }

        sink.bind_label(done, &mut state.ctrl_plane);
    }
}

/// `sync` (heavyweight) and `isync` barriers.
const SYNC: u32 = (31 << 26) | (598 << 1);
const ISYNC: u32 = (19 << 26) | (150 << 1);

/// The larx/stcx. extended opcodes for each access size. POWER8 (ISA
/// 2.07) provides the byte and halfword forms.
fn larx_stcx_xo(ty: Type) -> (u32, u32) {
    match ty {
        I8 => (52, 694),
        I16 => (116, 726),
        I32 => (20, 150),
        I64 => (84, 214),
        _ => unreachable!(),
    }
}

/// The frame teardown shared by both tail-call forms: restore the
/// clobbered callee-saves, the return address (back into LR) and the
/// frame pointer, then pop the frame down to the callee's expected
/// incoming-argument size. The caller then branches without linking.
///
/// The sequence length depends on the clobber set, so it is emitted once
/// into a throwaway buffer to size an island request, mirroring riscv64.
fn emit_return_call_common_sequence<T>(
    sink: &mut MachBuffer<Inst>,
    emit_info: &EmitInfo,
    state: &mut EmitState,
    info: &ReturnCallInfo<T>,
) {
    let mut buffer = MachBuffer::new();
    let mut fake_emit_state = state.clone();
    return_call_emit_impl(&mut buffer, emit_info, &mut fake_emit_state, info);
    let buffer = buffer.finish(&Default::default(), &mut Default::default());
    let length = buffer.data().len() as u32;

    if sink.island_needed(length) {
        let jump_around_label = sink.get_label();
        Inst::gen_jump(jump_around_label).emit(sink, emit_info, state);
        sink.emit_island(length + 4, &mut state.ctrl_plane);
        sink.bind_label(jump_around_label, &mut state.ctrl_plane);
    }

    return_call_emit_impl(sink, emit_info, state, info);
}

fn return_call_emit_impl<T>(
    sink: &mut MachBuffer<Inst>,
    emit_info: &EmitInfo,
    state: &mut EmitState,
    info: &ReturnCallInfo<T>,
) {
    let sp_to_fp_offset = {
        let frame_layout = state.frame_layout();
        i64::from(
            frame_layout.clobber_size
                + frame_layout.fixed_frame_storage_size
                + frame_layout.outgoing_args_size,
        )
    };

    // Restore the clobbered callee-saves, in the layout gen_clobber_save
    // wrote them: descending from just below the frame record.
    let mut clobber_offset = sp_to_fp_offset - 8;
    for reg in state.frame_layout().clobbered_callee_saves.clone() {
        let rreg = reg.to_reg();
        let ty = match rreg.class() {
            RegClass::Int => I64,
            RegClass::Float => F64,
            RegClass::Vector => {
                unimplemented!("ppc64 vector clobber restores are not yet supported")
            }
        };
        Inst::gen_load(
            reg.map(Reg::from),
            AMode::SPOffset(clobber_offset),
            ty,
            MemFlagsData::trusted(),
        )
        .emit(sink, emit_info, state);
        clobber_offset -= 8;
    }

    // Restore the return address into LR, and the frame pointer.
    let setup_area_size = i64::from(state.frame_layout().setup_area_size);
    if setup_area_size > 0 {
        Inst::gen_load(
            Writable::from_reg(zero_scratch_reg()),
            AMode::SPOffset(sp_to_fp_offset + 8),
            I64,
            MemFlagsData::trusted(),
        )
        .emit(sink, emit_info, state);
        Inst::Mtlr {
            rs: zero_scratch_reg(),
        }
        .emit(sink, emit_info, state);
        Inst::gen_load(
            writable_fp_reg(),
            AMode::SPOffset(sp_to_fp_offset),
            I64,
            MemFlagsData::trusted(),
        )
        .emit(sink, emit_info, state);
    }

    // If the prologue over-allocated the incoming-argument area relative
    // to what the callee expects, shrink back down to its size.
    let incoming_args_diff =
        i64::from(state.frame_layout().tail_args_size - info.new_stack_arg_size);

    let sp_increment = sp_to_fp_offset + setup_area_size + incoming_args_diff;
    if sp_increment > 0 {
        for inst in Ppc64MachineDeps::gen_sp_reg_adjust(i32::try_from(sp_increment).unwrap()) {
            inst.emit(sink, emit_info, state);
        }
    }
}

/// Emit `cmpdi`/`cmpwi` of `ra` against a signed 16-bit immediate, into cr0.
fn emit_cmpi(sink: &mut MachBuffer<Inst>, ra: u32, imm: i16, is_64: bool) {
    sink.put4(enc_cmpi(11, 0, u32::from(is_64), ra, imm as u16));
}

/// Emit a `bc` to `label` testing cr0's `bit`; `polarity` selects whether
/// the branch is taken when the bit is set.
fn emit_bc_to_label(
    sink: &mut MachBuffer<Inst>,
    label: MachLabel,
    bit: u32,
    polarity: bool,
) {
    let off = sink.cur_offset();
    sink.use_label_at_offset(off, label, LabelUse::Branch16);
    sink.put4(enc_bc(bo_for(polarity), bit, 0, false));
}

/// Emit an unconditional `b` to `label`.
fn emit_b_to_label(sink: &mut MachBuffer<Inst>, label: MachLabel) {
    let off = sink.cur_offset();
    sink.use_label_at_offset(off, label, LabelUse::Branch26);
    sink.put4(enc_b(0, false));
}

impl FloatCompare {
    /// Emit `fcmpu` into cr0, plus the `cror` that some conditions need to
    /// combine two of its result bits. Returns the cr0 bit to test and the
    /// polarity for which the condition holds.
    fn emit_cmp(&self, sink: &mut MachBuffer<Inst>) -> (Option<()>, u32, bool) {
        sink.put4(enc_x_opcd(63, 0, reg_num(self.rs1), reg_num(self.rs2), 0));
        let (cror, bit, polarity) = self.cr_plan();
        if let Some((bt, ba, bb)) = cror {
            sink.put4(enc_cror(bt, ba, bb));
        }
        (cror.map(|_| ()), bit, polarity)
    }
}

impl IntegerCompare {
    /// Emit the `cmp`/`cmpl`/`cmpw`/`cmplw` into cr0.
    fn emit_cmp(&self, sink: &mut MachBuffer<Inst>) {
        let l = u32::from(self.is_64);
        let xo = if self.is_signed() { 0 } else { 32 };
        sink.put4(enc_cmp(0, l, reg_num(self.rs1), reg_num(self.rs2), xo));
    }

    /// Encode the `bc` for this comparison with the given byte offset.
    fn enc_bc(&self, off: i32) -> u32 {
        let (bit, polarity) = self.bit_and_polarity();
        enc_bc(bo_for(polarity), bit, off, false)
    }
}

impl MachInstEmit for Inst {
    type State = EmitState;
    type Info = EmitInfo;

    fn emit(&self, sink: &mut MachBuffer<Inst>, emit_info: &Self::Info, state: &mut EmitState) {
        let start_off = sink.cur_offset();

        match self {
            &Inst::Nop0 | &Inst::Args { .. } | &Inst::Rets { .. } | &Inst::DummyUse { .. } => {}
            &Inst::Nop4 => sink.put4(NOP_INSTRUCTION),

            &Inst::LoadConst64 { rd, imm } => {
                for w in Inst::load_constant_words(reg_num(rd.to_reg()), imm) {
                    sink.put4(w);
                }
            }

            &Inst::AluRRR { op, rd, ra, rb } => {
                let rd = reg_num(rd.to_reg());
                let ra = reg_num(ra);
                let rb = reg_num(rb);
                let word = match op {
                    AluOp::Add => enc_xo(rd, ra, rb, 266),
                    // subf RT,RA,RB computes RB - RA; swap to get ra - rb.
                    AluOp::Sub => enc_xo(rd, rb, ra, 40),
                    AluOp::Addc => enc_xo(rd, ra, rb, 10),
                    AluOp::Adde => enc_xo(rd, ra, rb, 138),
                    // The subtract-from forms swap like `subf`.
                    AluOp::Subfc => enc_xo(rd, rb, ra, 8),
                    AluOp::Subfe => enc_xo(rd, rb, ra, 136),
                    AluOp::Mulld => enc_xo(rd, ra, rb, 233),
                    AluOp::Mulhd => enc_xo(rd, ra, rb, 73),
                    AluOp::Mulhdu => enc_xo(rd, ra, rb, 9),
                    AluOp::Mulhw => enc_xo(rd, ra, rb, 75),
                    AluOp::Mulhwu => enc_xo(rd, ra, rb, 11),
                    AluOp::And => enc_x_logic(ra, rd, rb, 28),
                    AluOp::Or => enc_x_logic(ra, rd, rb, 444),
                    AluOp::Xor => enc_x_logic(ra, rd, rb, 316),
                };
                sink.put4(word);
            }

            &Inst::AluRRImm16 { op, rd, ra, imm } => {
                let rd = reg_num(rd.to_reg());
                let ra_n = reg_num(ra);
                let word = match op {
                    // addi with RA=0 would mean "literal zero"; the
                    // lowering rules never produce that because r0 is not
                    // allocatable.
                    AluImmOp::Addi => {
                        debug_assert!(ra_n != 0);
                        enc_d(14, rd, ra_n, imm)
                    }
                    AluImmOp::Andi => enc_d_logic(28, ra_n, rd, imm),
                };
                sink.put4(word);
            }

            &Inst::UnaryRR { op, rd, rn } => {
                let rd = reg_num(rd.to_reg());
                let rn = reg_num(rn);
                let word = match op {
                    UnaryOp::Neg => enc_xo(rd, rn, 0, 104),
                    // `nor rd, rn, rn` is the canonical `not`.
                    UnaryOp::Not => enc_x_logic(rn, rd, rn, 124),
                };
                sink.put4(word);
            }

            &Inst::ShiftRRImm { op, rd, ra, imm } => {
                let rd = reg_num(rd.to_reg());
                let ra_n = reg_num(ra);
                let n = u32::from(imm);
                let word = match op {
                    // sldi rd, ra, n == rldicr rd, ra, n, 63-n
                    ShiftOp::Sld => enc_md(ra_n, rd, n, 63 - n, 1),
                    // srdi rd, ra, n == rldicl rd, ra, 64-n, n. For n = 0
                    // the rotate wraps to 0, which leaves the value alone.
                    ShiftOp::Srd => enc_md(ra_n, rd, (64 - n) & 63, n, 0),
                    ShiftOp::Srad => enc_xs(ra_n, rd, n),
                    // rotldi rd, ra, n == rldicl rd, ra, n, 0
                    ShiftOp::Rotld => enc_md(ra_n, rd, n, 0, 0),
                    // slwi rd, ra, n == rlwinm rd, ra, n, 0, 31-n
                    ShiftOp::Slw => enc_m(ra_n, rd, n, 0, 31 - n),
                    // srwi rd, ra, n == rlwinm rd, ra, 32-n, n, 31
                    ShiftOp::Srw => enc_m(ra_n, rd, (32 - n) & 31, n, 31),
                    ShiftOp::Sraw => enc_x_logic(ra_n, rd, n, 824),
                    // rotlwi rd, ra, n == rlwinm rd, ra, n, 0, 31
                    ShiftOp::Rotlw => enc_m(ra_n, rd, n, 0, 31),
                };
                sink.put4(word);
            }

            &Inst::ShiftRRR { op, rd, ra, rb } => {
                let rd = reg_num(rd.to_reg());
                let ra = reg_num(ra);
                let rb = reg_num(rb);
                match op {
                    // rotlw rd, ra, rb == rlwnm rd, ra, rb, 0, 31 (M-form).
                    ShiftOp::Rotlw => {
                        sink.put4((23 << 26) | (ra << 21) | (rd << 16) | (rb << 11) | (31 << 1));
                        return;
                    }
                    // rotld rd, ra, rb == rldcl rd, ra, rb, 0 (MDS-form).
                    ShiftOp::Rotld => {
                        sink.put4((30 << 26) | (ra << 21) | (rd << 16) | (rb << 11) | (8 << 1));
                        return;
                    }
                    _ => {}
                }
                let xo = match op {
                    ShiftOp::Slw => 24,
                    ShiftOp::Srw => 536,
                    ShiftOp::Sraw => 792,
                    ShiftOp::Sld => 27,
                    ShiftOp::Srd => 539,
                    ShiftOp::Srad => 794,
                    ShiftOp::Rotlw | ShiftOp::Rotld => unreachable!(),
                };
                sink.put4(enc_x_logic(ra, rd, rb, xo));
            }

            &Inst::BitCount { op, rd, rn, ty } => {
                let rd_n = reg_num(rd.to_reg());
                let rn_n = reg_num(rn);
                let is_64 = ty == I64;
                match op {
                    BitOp::Clz => {
                        let xo = if is_64 { 58 } else { 26 };
                        sink.put4(enc_x_logic(rn_n, rd_n, 0, xo));
                    }
                    BitOp::Popcnt => {
                        let xo = if is_64 { 506 } else { 378 };
                        sink.put4(enc_x_logic(rn_n, rd_n, 0, xo));
                    }
                    BitOp::Ctz if emit_info.isa_flags.has_isa_3_0() => {
                        let xo = if is_64 { 570 } else { 538 };
                        sink.put4(enc_x_logic(rn_n, rd_n, 0, xo));
                    }
                    BitOp::Ctz => {
                        // Without ISA 3.0, count trailing zeros as
                        // `popcnt((x - 1) & ~x)`, which yields the full
                        // width for x == 0.
                        let src = if is_64 {
                            rn_n
                        } else {
                            // Force bit 32 set so a zero low word yields
                            // 32, and so that garbage above bit 31 (which
                            // callers may leave in an i32) cannot affect
                            // the count.
                            let tmp = reg_num(spilltmp_reg());
                            for w in Inst::load_constant_words(0, 0xFFFF_FFFF_0000_0000) {
                                sink.put4(w);
                            }
                            sink.put4(enc_x_logic(rn_n, tmp, 0, 444)); // or tmp, rn, r0
                            tmp
                        };
                        sink.put4(enc_d(14, 0, src, (-1i16) as u16)); // addi r0, src, -1
                        sink.put4(enc_x_logic(0, 0, src, 60)); // andc r0, r0, src
                        sink.put4(enc_x_logic(0, rd_n, 0, 506)); // popcntd rd, r0
                    }
                }
            }

            &Inst::DivRem { op, rd, ra, rb, ty } => {
                Inst::emit_divrem(sink, emit_info, state, op, rd, ra, rb, ty);
            }

            &Inst::Select { rd, kind, rt, rf } => {
                kind.emit_cmp(sink);
                let (bit, polarity) = kind.bit_and_polarity();
                let rd = reg_num(rd.to_reg());
                // `isel rd, RA, RB, bit` picks RA when the bit is set.
                // Neither operand can be r0 (which isel reads as a literal
                // zero) because r0 is not allocatable.
                let (a, b) = if polarity {
                    (reg_num(rt), reg_num(rf))
                } else {
                    (reg_num(rf), reg_num(rt))
                };
                debug_assert!(a != 0 && b != 0);
                sink.put4(enc_isel(rd, a, b, bit));
            }

            &Inst::FpuRRR { op, rd, ra, rb, ty } => {
                // The single- and double-precision forms differ only in
                // primary opcode; using the single forms for `f32` is what
                // rounds each result to single precision.
                let opcd = if ty == F32 { 59 } else { 63 };
                let rd = reg_num(rd.to_reg());
                let ra = reg_num(ra);
                let rb = reg_num(rb);
                let word = match op {
                    FpuOp2::Add => enc_a(opcd, rd, ra, rb, 0, 21),
                    FpuOp2::Sub => enc_a(opcd, rd, ra, rb, 0, 20),
                    // `fmul` takes its second operand in FRC.
                    FpuOp2::Mul => enc_a(opcd, rd, ra, 0, rb, 25),
                    FpuOp2::Div => enc_a(opcd, rd, ra, rb, 0, 18),
                    // `fcpsgn` takes the sign from FRA and the magnitude
                    // from FRB, and has no single-precision form.
                    FpuOp2::CopySign => enc_x_opcd(63, rd, ra, rb, 8),
                };
                sink.put4(word);
            }

            &Inst::FpuRR { op, rd, rn, ty } => {
                let rd = reg_num(rd.to_reg());
                let rn = reg_num(rn);
                let word = match op {
                    FpuOp1::Neg => enc_x_opcd(63, rd, 0, rn, 40),
                    FpuOp1::Abs => enc_x_opcd(63, rd, 0, rn, 264),
                    FpuOp1::Sqrt => {
                        let opcd = if ty == F32 { 59 } else { 63 };
                        enc_a(opcd, rd, 0, rn, 0, 22)
                    }
                    FpuOp1::Mov => enc_fmr(rd, rn),
                    FpuOp1::Demote => enc_x_opcd(63, rd, 0, rn, 12), // frsp
                    // XX2-form; VSR 0-31 alias the FPRs so the extension
                    // bits stay zero.
                    FpuOp1::CvtToSingleBits => (60 << 26) | (rd << 21) | (rn << 11) | (267 << 2),
                    FpuOp1::CvtFromSingleBits => (60 << 26) | (rd << 21) | (rn << 11) | (331 << 2),
                };
                sink.put4(word);
            }

            &Inst::FpuFma {
                rd,
                ra,
                rc,
                rb,
                ty,
            } => {
                let opcd = if ty == F32 { 59 } else { 63 };
                sink.put4(enc_a(
                    opcd,
                    reg_num(rd.to_reg()),
                    reg_num(ra),
                    reg_num(rb),
                    reg_num(rc),
                    29,
                ));
            }

            &Inst::FpuCmpSet { rd, kind } => {
                let (cror, bit, polarity) = kind.emit_cmp(sink);
                let _ = cror;
                let rd = reg_num(rd.to_reg());
                sink.put4(enc_d(14, rd, 0, 1)); // li rd, 1
                if polarity {
                    for w in Inst::load_constant_words(0, 0) {
                        sink.put4(w);
                    }
                    sink.put4(enc_isel(rd, rd, 0, bit));
                } else {
                    sink.put4(enc_isel(rd, 0, rd, bit));
                }
            }

            &Inst::FpuCondBr {
                taken,
                not_taken,
                kind,
            } => {
                let (_, bit, polarity) = kind.emit_cmp(sink);
                match taken {
                    CondBrTarget::Label(label) => {
                        let bc_off = sink.cur_offset();
                        let code = enc_bc(bo_for(polarity), bit, 0, false);
                        let inverted = enc_bc(bo_for(!polarity), bit, 0, false).to_le_bytes();
                        sink.use_label_at_offset(bc_off, label, LabelUse::Branch16);
                        sink.add_cond_branch(bc_off, bc_off + 4, label, &inverted);
                        sink.put4(code);
                    }
                    CondBrTarget::Fallthrough => panic!("Cannot fallthrough in taken target"),
                }
                match not_taken {
                    CondBrTarget::Label(label) => {
                        Inst::gen_jump(label).emit(sink, emit_info, state)
                    }
                    CondBrTarget::Fallthrough => {}
                }
            }

            &Inst::IntToFpu { rd, rn, signed, ty } => {
                let opcd = if ty == F32 { 59 } else { 63 };
                let xo = if signed { 846 } else { 974 };
                sink.put4(enc_x_opcd(opcd, reg_num(rd.to_reg()), 0, reg_num(rn), xo));
            }

            &Inst::FpuSelect { rd, kind, rt, rf } => {
                // `isel` yields its RA operand when the tested cr0 bit is
                // set, and reads r0 in that position as a literal zero. So
                // r11 always holds whichever value the *set* bit selects,
                // leaving r0 for the other one.
                let tmp = reg_num(spilltmp_reg());
                let (bit, polarity) = kind.bit_and_polarity();
                let (if_set, if_clear) = if polarity { (rt, rf) } else { (rf, rt) };
                sink.put4(enc_xx1(reg_num(if_set), tmp, 51)); // mfvsrd r11, _
                sink.put4(enc_xx1(reg_num(if_clear), 0, 51)); // mfvsrd r0, _
                kind.emit_cmp(sink);
                sink.put4(enc_isel(tmp, tmp, 0, bit));
                sink.put4(enc_xx1(reg_num(rd.to_reg()), tmp, 179)); // mtvsrd rd, r11
            }

            &Inst::BrTable {
                index,
                tmp1,
                tmp2,
                ref targets,
            } => {
                // The default target is element zero; the rest form the
                // table proper.
                let default_target = targets[0];
                let jt = &targets[1..];

                // The setup is eight instructions and each table entry is
                // one, so make sure an island cannot land in the middle.
                let distance = ((12 + jt.len()) * 4) as u32;
                if sink.island_needed(distance) {
                    let around = sink.get_label();
                    Inst::gen_jump(around).emit(sink, emit_info, state);
                    sink.emit_island(distance + 4, &mut state.ctrl_plane);
                    sink.bind_label(around, &mut state.ctrl_plane);
                }

                let addr = reg_num(tmp1.to_reg());
                let ext = reg_num(tmp2.to_reg());
                debug_assert!(addr != 0 && ext != 0);

                // The upper half of the index is undefined, so narrow it
                // before comparing: clrldi ext, index, 32.
                sink.put4(enc_md(reg_num(index), ext, 0, 32, 0));

                let n = jt.len() as u64;
                match u16::try_from(n) {
                    Ok(n16) => sink.put4(enc_cmpi(10, 0, 1, ext, n16)), // cmpldi
                    Err(_) => {
                        for w in Inst::load_constant_words(addr, n) {
                            sink.put4(w);
                        }
                        sink.put4(enc_cmp(0, 1, ext, addr, 32)); // cmpld
                    }
                }

                let compute = sink.get_label();
                emit_bc_to_label(sink, compute, CR0_LT, true);
                // Out of range: fall through to a branch to the default.
                emit_b_to_label(sink, default_target);
                sink.bind_label(compute, &mut state.ctrl_plane);

                // Read the PC, then index into the table that follows.
                // `bcl` leaves LR pointing at the `mflr`, and the five
                // instructions after it plus the `mflr` itself occupy the
                // 24 bytes before the table.
                sink.put4(BCL_20_31_PLUS4);
                sink.put4(enc_mflr(addr));
                sink.put4(enc_md(ext, ext, 2, 61, 1)); // sldi ext, ext, 2
                sink.put4(enc_xo(addr, addr, ext, 266)); // add addr, addr, ext
                sink.put4(enc_d(14, addr, addr, 24)); // addi addr, addr, 24
                sink.put4(enc_mtctr(addr));
                sink.put4(enc_bctr(false));

                for &target in jt {
                    emit_b_to_label(sink, target);
                }
            }

            &Inst::EmitIsland { needed_space } => {
                if sink.island_needed(needed_space) {
                    let skip = sink.get_label();
                    Inst::gen_jump(skip).emit(sink, emit_info, state);
                    sink.emit_island(needed_space + 4, &mut state.ctrl_plane);
                    sink.bind_label(skip, &mut state.ctrl_plane);
                }
            }

            &Inst::FpuToInt {
                rd,
                tmp,
                rn,
                signed,
                sat,
                in_ty,
                out_ty,
            } => {
                let rd = reg_num(rd.to_reg());
                let tmp = reg_num(tmp.to_reg());
                let rn = reg_num(rn);
                // fctidz / fctiduz / fctiwz / fctiwuz: truncating, and
                // saturating on out-of-range inputs.
                let cvt_xo = match (signed, out_ty == I64) {
                    (true, true) => 815,
                    (false, true) => 943,
                    (true, false) => 15,
                    (false, false) => 143,
                };

                // NaN check: `fcmpu x, x` is unordered only for NaN.
                sink.put4(enc_x_opcd(63, 0, rn, rn, 0));
                if sat {
                    // Ordered: skip the zero-result path.
                    sink.put4(enc_bc(bo_for(false), CR0_UN, 12, false));
                    sink.put4(enc_d(14, rd, 0, 0)); // li rd, 0
                    sink.put4(enc_b(12, false)); // past the conversion
                    sink.put4(enc_x_opcd(63, tmp, 0, rn, cvt_xo));
                    sink.put4(enc_xx1(tmp, rd, 51)); // mfvsrd
                } else {
                    // Trap on NaN.
                    sink.put4(enc_bc(bo_for(false), CR0_UN, 8, false));
                    Inst::Udf {
                        trap_code: ir::TrapCode::BAD_CONVERSION_TO_INTEGER,
                    }
                    .emit(sink, emit_info, state);

                    // The input must lie strictly between the exclusive
                    // bounds. An f32 input is held widened to double, so
                    // its bounds are compared in the double domain too,
                    // which is exact.
                    let (lo, hi) = if in_ty == F32 {
                        let (lo, hi) = wasmtime_core::math::f32_cvt_to_int_bounds(
                            signed,
                            out_ty.bits(),
                        );
                        (f64::from(lo).to_bits(), f64::from(hi).to_bits())
                    } else {
                        let (lo, hi) = wasmtime_core::math::f64_cvt_to_int_bounds(
                            signed,
                            out_ty.bits(),
                        );
                        (lo.to_bits(), hi.to_bits())
                    };

                    // Trap unless x > lo.
                    for w in Inst::load_constant_words(rd, lo) {
                        sink.put4(w);
                    }
                    sink.put4(enc_xx1(tmp, rd, 179)); // mtvsrd tmp, rd
                    sink.put4(enc_x_opcd(63, 0, rn, tmp, 0)); // fcmpu
                    sink.put4(enc_bc(bo_for(true), CR0_GT, 8, false));
                    Inst::Udf {
                        trap_code: ir::TrapCode::INTEGER_OVERFLOW,
                    }
                    .emit(sink, emit_info, state);

                    // Trap unless x < hi.
                    for w in Inst::load_constant_words(rd, hi) {
                        sink.put4(w);
                    }
                    sink.put4(enc_xx1(tmp, rd, 179));
                    sink.put4(enc_x_opcd(63, 0, rn, tmp, 0));
                    sink.put4(enc_bc(bo_for(true), CR0_LT, 8, false));
                    Inst::Udf {
                        trap_code: ir::TrapCode::INTEGER_OVERFLOW,
                    }
                    .emit(sink, emit_info, state);

                    sink.put4(enc_x_opcd(63, tmp, 0, rn, cvt_xo));
                    sink.put4(enc_xx1(tmp, rd, 51)); // mfvsrd
                }
            }

            &Inst::FpuMinMax { rd, ra, rb, is_max } => {
                // `xsmindp`/`xsmaxdp` handle signed zeros correctly but
                // return the numeric operand when the other is NaN, where
                // CLIF requires NaN; `fadd` on the ordered-check failure
                // path propagates a quiet NaN instead. `rd` is an early
                // def, so it cannot alias the inputs the fadd re-reads.
                let rd = reg_num(rd.to_reg());
                let ra = reg_num(ra);
                let rb = reg_num(rb);
                let xo = if is_max { 160 } else { 168 };
                sink.put4(enc_x_opcd(63, 0, ra, rb, 0)); // fcmpu cr0, ra, rb
                sink.put4((60 << 26) | (rd << 21) | (ra << 16) | (rb << 11) | (xo << 3));
                sink.put4(enc_bc(bo_for(false), CR0_UN, 8, false)); // ordered: skip
                sink.put4(enc_a(63, rd, ra, rb, 0, 21)); // fadd rd, ra, rb
            }

            &Inst::FpuRound { rd, rn, mode } => {
                let rd = reg_num(rd.to_reg());
                let rn = reg_num(rn);
                let word = match mode {
                    // fri{m,p,z} frt, frb: X-form, opcode 63.
                    FpuRoundMode::Floor => (63 << 26) | (rd << 21) | (rn << 11) | (488 << 1),
                    FpuRoundMode::Ceil => (63 << 26) | (rd << 21) | (rn << 11) | (456 << 1),
                    FpuRoundMode::Trunc => (63 << 26) | (rd << 21) | (rn << 11) | (424 << 1),
                    // xsrdpic: round by the current mode, which is the
                    // ties-to-even default. XX2-form, opcode 60, xo 107;
                    // FPRs are VSRs 0-31, so the TX/BX bits stay zero.
                    FpuRoundMode::Nearest => (60 << 26) | (rd << 21) | (rn << 11) | (107 << 2),
                };
                sink.put4(word);
            }

            &Inst::Bswap { rd, rn, ty } => {
                // addi r0, r1, -16 ; st? rn, -16(r1) ; l?brx rd, 0, r0
                //
                // The slot is in the ELFv2 red zone, and r0 (the emission
                // scratch) carries its address because the byte-reversed
                // loads exist only in indexed form.
                let rd = reg_num(rd.to_reg());
                let rn = reg_num(rn);
                sink.put4(enc_d(14, 0, 1, (-16i16) as u16)); // addi r0, r1, -16
                let (store, brx_xo) = match ty {
                    I16 => (enc_d(44, rn, 1, (-16i16) as u16), 790), // sth / lhbrx
                    I32 => (enc_d(36, rn, 1, (-16i16) as u16), 534), // stw / lwbrx
                    I64 => (enc_ds(62, rn, 1, -16, 0), 532),         // std / ldbrx
                    _ => unreachable!("bswap of {ty}"),
                };
                sink.put4(store);
                sink.put4(enc_x(rd, 0, 0, brx_xo));
            }

            &Inst::MovToFpr { rd, rn } => {
                sink.put4(enc_xx1(reg_num(rd.to_reg()), reg_num(rn), 179));
            }

            &Inst::MovFromFpr { rd, rn } => {
                sink.put4(enc_xx1(reg_num(rn), reg_num(rd.to_reg()), 51));
            }

            &Inst::Extend {
                rd,
                rn,
                signed,
                from_bits,
                ..
            } => {
                let rd = reg_num(rd.to_reg());
                let rn = reg_num(rn);
                let word = if signed {
                    match from_bits {
                        8 => enc_x_logic(rn, rd, 0, 954),  // extsb
                        16 => enc_x_logic(rn, rd, 0, 922), // extsh
                        32 => enc_x_logic(rn, rd, 0, 986), // extsw
                        _ => unreachable!("extend from {from_bits}"),
                    }
                } else {
                    // rldicl rd, rn, 0, 64-from: clear the high bits.
                    enc_md(rn, rd, 0, 64 - u32::from(from_bits), 0)
                };
                sink.put4(word);
            }

            &Inst::Load {
                rd, op, flags, from, ..
            } => {
                if let Some(trap_code) = flags.trap_code() {
                    sink.add_trap(trap_code);
                }
                let rt = reg_num(rd.to_reg());
                let (d_opcd, ds_xo, x_xo) = match op {
                    LoadOP::Lbz => (34, None, 87),
                    LoadOP::Lhz => (40, None, 279),
                    LoadOP::Lwz => (32, None, 23),
                    LoadOP::Ld => (58, Some(0), 21),
                    LoadOP::Lfs => (48, None, 535),
                    LoadOP::Lfd => (50, None, 599),
                };
                emit_mem_access(sink, state, from, rt, d_opcd, ds_xo, x_xo);
            }

            &Inst::Store {
                to, op, flags, src, ..
            } => {
                if let Some(trap_code) = flags.trap_code() {
                    sink.add_trap(trap_code);
                }
                let rs = reg_num(src);
                let (d_opcd, ds_xo, x_xo) = match op {
                    StoreOP::Stb => (38, None, 215),
                    StoreOP::Sth => (44, None, 407),
                    StoreOP::Stw => (36, None, 151),
                    StoreOP::Std => (62, Some(0), 149),
                    StoreOP::Stfs => (52, None, 663),
                    StoreOP::Stfd => (54, None, 727),
                };
                emit_mem_access(sink, state, to, rs, d_opcd, ds_xo, x_xo);
            }

            &Inst::LoadAddr { rd, mem } => {
                let (base, offset) = mem.to_base_and_offset(state.frame_layout());
                let rd_n = reg_num(rd.to_reg());
                let base_n = reg_num(base);
                debug_assert!(base_n != 0, "r0 is not a valid addi base");
                if let Ok(imm16) = i16::try_from(offset) {
                    sink.put4(enc_d(14, rd_n, base_n, imm16 as u16)); // addi
                } else {
                    for w in Inst::load_constant_words(0, offset as u64) {
                        sink.put4(w);
                    }
                    sink.put4(enc_xo(rd_n, base_n, 0, 266)); // add rd, base, r0
                }
            }

            &Inst::Ret => sink.put4(enc_blr()),

            &Inst::Mov { rd, rm, .. } => {
                debug_assert_eq!(rd.to_reg().class(), rm.class());
                if rd.to_reg() == rm {
                    return;
                }
                match rm.class() {
                    RegClass::Int => {
                        // ori rd, rm, 0
                        sink.put4(enc_d_logic(24, reg_num(rm), reg_num(rd.to_reg()), 0));
                    }
                    RegClass::Float => {
                        sink.put4(enc_fmr(reg_num(rd.to_reg()), reg_num(rm)));
                    }
                    RegClass::Vector => {
                        // xxlor rd, rm, rm
                        let d = vsr_num(rd.to_reg());
                        let m = vsr_num(rm);
                        sink.put4(enc_xx3(d, m, m, 146));
                    }
                }
            }

            &Inst::CmpSet { rd, kind } => {
                // cmp reads its inputs before rd is written, so a plain
                // (non-early) def is fine even if rd aliases an input.
                kind.emit_cmp(sink);
                let rd = reg_num(rd.to_reg());
                let (bit, polarity) = kind.bit_and_polarity();
                sink.put4(enc_d(14, rd, 0, 1)); // li rd, 1
                if polarity {
                    // isel rd, rd(=1), r0(=const 0 via the RA quirk? no:
                    // RB has no quirk, so load a real zero into r0 first).
                    for w in Inst::load_constant_words(0, 0) {
                        sink.put4(w);
                    }
                    sink.put4(enc_isel(rd, rd, 0, bit));
                } else {
                    // Bit clear means "condition holds": select 1 (in rd,
                    // RB position) when clear, constant 0 (RA=r0 quirk)
                    // when set.
                    sink.put4(enc_isel(rd, 0, rd, bit));
                }
            }

            &Inst::Jump { label } => {
                sink.use_label_at_offset(start_off, label, LabelUse::Branch26);
                sink.add_uncond_branch(start_off, start_off + 4, label);
                sink.put4(enc_b(0, false));
            }

            &Inst::CondBr {
                taken,
                not_taken,
                kind,
            } => {
                // The compare is emitted outside the region registered
                // with the buffer's branch-folding machinery; only the
                // 4-byte `bc` participates, so inversion is a same-size
                // patch of the BO field.
                kind.emit_cmp(sink);
                match taken {
                    CondBrTarget::Label(label) => {
                        let bc_off = sink.cur_offset();
                        let code = kind.enc_bc(0);
                        let inverted = kind.inverse().enc_bc(0).to_le_bytes();
                        sink.use_label_at_offset(bc_off, label, LabelUse::Branch16);
                        sink.add_cond_branch(bc_off, bc_off + 4, label, &inverted);
                        sink.put4(code);
                    }
                    CondBrTarget::Fallthrough => panic!("Cannot fallthrough in taken target"),
                }
                match not_taken {
                    CondBrTarget::Label(label) => {
                        Inst::gen_jump(label).emit(sink, emit_info, state)
                    }
                    CondBrTarget::Fallthrough => {}
                }
            }

            &Inst::Udf { trap_code } => {
                sink.add_trap(trap_code);
                sink.put4(TRAP_INSTRUCTION);
            }

            &Inst::TrapIf { kind, trap_code } => {
                let label_end = sink.get_label();
                Inst::CondBr {
                    taken: CondBrTarget::Label(label_end),
                    not_taken: CondBrTarget::Fallthrough,
                    kind: kind.inverse(),
                }
                .emit(sink, emit_info, state);
                Inst::Udf { trap_code }.emit(sink, emit_info, state);
                sink.bind_label(label_end, &mut state.ctrl_plane);
            }

            Inst::Call { info } => {
                sink.add_reloc(Reloc::Ppc64Call, &info.dest, 0);
                sink.put4(enc_b(0, true)); // bl

                if let Some(s) = state.take_stack_map() {
                    let offset = sink.cur_offset();
                    sink.push_user_stack_map(state, offset, s);
                }
                if let Some(try_call) = info.try_call_info.as_ref() {
                    sink.add_try_call_site(
                        Some(state.frame_layout.sp_to_fp()),
                        try_call.exception_handlers(&state.frame_layout),
                    );
                } else {
                    sink.add_call_site();
                }

                let callee_pop_size = i32::try_from(info.callee_pop_size).unwrap();
                if callee_pop_size > 0 {
                    for inst in Ppc64MachineDeps::gen_sp_reg_adjust(-callee_pop_size) {
                        inst.emit(sink, emit_info, state);
                    }
                }

                if info.patchable {
                    unimplemented!("patchable calls are not yet supported on ppc64");
                }
                info.emit_retval_loads::<Ppc64MachineDeps, _, _>(
                    state.frame_layout().stackslots_size,
                    |inst| inst.emit(sink, emit_info, state),
                    |needed_space| Some(Inst::EmitIsland { needed_space }),
                );

                if let Some(try_call) = info.try_call_info.as_ref() {
                    Inst::gen_jump(try_call.continuation).emit(sink, emit_info, state);
                }
            }

            Inst::CallInd { info } => {
                // The operand collector pins `dest` to r12 (the ELFv2
                // global-entry convention).
                sink.put4(enc_mtctr(reg_num(info.dest)));
                sink.put4(enc_bctr(true)); // bctrl

                if let Some(s) = state.take_stack_map() {
                    let offset = sink.cur_offset();
                    sink.push_user_stack_map(state, offset, s);
                }
                if let Some(try_call) = info.try_call_info.as_ref() {
                    sink.add_try_call_site(
                        Some(state.frame_layout.sp_to_fp()),
                        try_call.exception_handlers(&state.frame_layout),
                    );
                } else {
                    sink.add_call_site();
                }

                let callee_pop_size = i32::try_from(info.callee_pop_size).unwrap();
                if callee_pop_size > 0 {
                    for inst in Ppc64MachineDeps::gen_sp_reg_adjust(-callee_pop_size) {
                        inst.emit(sink, emit_info, state);
                    }
                }

                info.emit_retval_loads::<Ppc64MachineDeps, _, _>(
                    state.frame_layout().stackslots_size,
                    |inst| inst.emit(sink, emit_info, state),
                    |needed_space| Some(Inst::EmitIsland { needed_space }),
                );

                if let Some(try_call) = info.try_call_info.as_ref() {
                    Inst::gen_jump(try_call.continuation).emit(sink, emit_info, state);
                }
            }

            &Inst::AtomicLoad { rd, addr, ty } => {
                let rd = reg_num(rd.to_reg());
                let addr = reg_num(addr);
                let load_xo = match ty {
                    I8 => 87,   // lbzx
                    I16 => 279, // lhzx
                    I32 => 23,  // lwzx
                    I64 => 21,  // ldx
                    _ => unreachable!(),
                };
                sink.put4(SYNC);
                sink.put4(enc_x(rd, 0, addr, load_xo));
                // Control dependency + isync gives acquire ordering.
                sink.put4(enc_cmp(0, 1, rd, rd, 0)); // cmpd rd, rd
                sink.put4(enc_bc(bo_for(false), CR0_EQ, 4, false)); // bne .+4
                sink.put4(ISYNC);
            }

            &Inst::AtomicStore { src, addr, ty } => {
                let src = reg_num(src);
                let addr = reg_num(addr);
                let store_xo = match ty {
                    I8 => 215,  // stbx
                    I16 => 407, // sthx
                    I32 => 151, // stwx
                    I64 => 149, // stdx
                    _ => unreachable!(),
                };
                sink.put4(SYNC);
                sink.put4(enc_x(src, 0, addr, store_xo));
            }

            &Inst::AtomicRmw {
                op,
                rd,
                addr,
                src,
                ty,
            } => {
                use crate::ir::AtomicRmwOp;
                let rd_n = reg_num(rd.to_reg());
                let addr = reg_num(addr);
                let src_n = reg_num(src);
                let (larx_xo, stcx_xo) = larx_stcx_xo(ty);

                sink.put4(SYNC);
                let loop_top = sink.get_label();
                sink.bind_label(loop_top, &mut state.ctrl_plane);
                sink.put4(enc_x(rd_n, 0, addr, larx_xo));

                // Compute the replacement value into r0 (or use src
                // directly for exchange).
                let store_reg = match op {
                    AtomicRmwOp::Xchg => src_n,
                    AtomicRmwOp::Add => {
                        sink.put4(enc_xo(0, rd_n, src_n, 266));
                        0
                    }
                    AtomicRmwOp::Sub => {
                        sink.put4(enc_xo(0, src_n, rd_n, 40)); // rd - src
                        0
                    }
                    AtomicRmwOp::And => {
                        sink.put4(enc_x_logic(rd_n, 0, src_n, 28));
                        0
                    }
                    AtomicRmwOp::Or => {
                        sink.put4(enc_x_logic(rd_n, 0, src_n, 444));
                        0
                    }
                    AtomicRmwOp::Xor => {
                        sink.put4(enc_x_logic(rd_n, 0, src_n, 316));
                        0
                    }
                    AtomicRmwOp::Nand => {
                        sink.put4(enc_x_logic(rd_n, 0, src_n, 476));
                        0
                    }
                    AtomicRmwOp::Umin
                    | AtomicRmwOp::Umax
                    | AtomicRmwOp::Smin
                    | AtomicRmwOp::Smax => {
                        // The reservation loads zero-extend, and `src` was
                        // pre-extended by the lowering rules, so unsigned
                        // compares work at full width directly; signed
                        // sub-doubleword compares sign-extend the loaded
                        // value into r0 first.
                        let signed =
                            matches!(op, AtomicRmwOp::Smin | AtomicRmwOp::Smax);
                        let cmp_lhs = if signed && ty != I64 {
                            let ext_xo = match ty {
                                I8 => 954,
                                I16 => 922,
                                I32 => 986,
                                _ => unreachable!(),
                            };
                            sink.put4(enc_x_logic(rd_n, 0, 0, ext_xo));
                            0
                        } else {
                            rd_n
                        };
                        let cmp_xo = if signed { 0 } else { 32 };
                        sink.put4(enc_cmp(0, 1, cmp_lhs, src_n, cmp_xo));
                        // Keep the loaded value when it is already the
                        // min/max, otherwise take src.
                        let keep_old = matches!(
                            op,
                            AtomicRmwOp::Umin | AtomicRmwOp::Smin
                        );
                        let bit = CR0_LT;
                        let (a, b) = if keep_old {
                            (rd_n, src_n)
                        } else {
                            (src_n, rd_n)
                        };
                        debug_assert!(a != 0 && b != 0);
                        sink.put4(enc_isel(0, a, b, bit));
                        0
                    }
                };

                sink.put4(enc_x(store_reg, 0, addr, stcx_xo) | 1); // stcx., Rc=1
                let bc_off = sink.cur_offset();
                sink.use_label_at_offset(bc_off, loop_top, LabelUse::Branch16);
                sink.put4(enc_bc(bo_for(false), CR0_EQ, 0, false)); // bne- loop
                sink.put4(ISYNC);
            }

            &Inst::AtomicCas {
                rd,
                addr,
                expected,
                new,
                ty,
            } => {
                let rd_n = reg_num(rd.to_reg());
                let addr = reg_num(addr);
                let expected = reg_num(expected);
                let new = reg_num(new);
                let (larx_xo, stcx_xo) = larx_stcx_xo(ty);

                sink.put4(SYNC);
                let loop_top = sink.get_label();
                let done = sink.get_label();
                sink.bind_label(loop_top, &mut state.ctrl_plane);
                sink.put4(enc_x(rd_n, 0, addr, larx_xo));
                // The reservation load zero-extends and `expected` was
                // pre-zero-extended, so a full-width logical compare works
                // for every type.
                sink.put4(enc_cmp(0, 1, rd_n, expected, 32)); // cmpld
                emit_bc_to_label(sink, done, CR0_EQ, false); // bne done
                sink.put4(enc_x(new, 0, addr, stcx_xo) | 1); // stcx.
                let bc_off = sink.cur_offset();
                sink.use_label_at_offset(bc_off, loop_top, LabelUse::Branch16);
                sink.put4(enc_bc(bo_for(false), CR0_EQ, 0, false)); // bne- loop
                sink.bind_label(done, &mut state.ctrl_plane);
                sink.put4(ISYNC);
            }

            &Inst::AtomicLoad128 { rd_lo, rd_hi, addr } => {
                // sync ; lqarx r8, 0, addr ; cmpd r8, r8 ; bne- $+4 ; isync
                //
                // Plain `lq` raises an alignment interrupt in LE mode
                // before ISA 3.0, so the reservation load stands in for
                // it (the reservation itself is simply left behind). The
                // compare/branch is the usual acquire control dependency.
                let hi = reg_num(rd_hi.to_reg());
                debug_assert_eq!(hi, 8);
                debug_assert_eq!(reg_num(rd_lo.to_reg()), 9);
                let addr = reg_num(addr);
                debug_assert_eq!(addr, 3);
                sink.put4(SYNC);
                sink.put4(enc_x(hi, 0, addr, 276)); // lqarx r8, 0, addr
                sink.put4(enc_cmp(0, 1, hi, hi, 0)); // cmpd r8, r8
                sink.put4(enc_bc(bo_for(false), CR0_EQ, 4, false)); // bne- $+4
                sink.put4(ISYNC);
            }

            &Inst::AtomicStore128 {
                rs_lo,
                rs_hi,
                ref tmp_lo,
                ref tmp_hi,
                addr,
            } => {
                // sync ; loop: lqarx r8, 0, r3 ; stqcx. r4, 0, r3 ; bne- loop
                debug_assert_eq!(reg_num(addr), 3);
                debug_assert_eq!(reg_num(rs_hi), 4);
                debug_assert_eq!(reg_num(rs_lo), 5);
                debug_assert_eq!(reg_num(tmp_hi.to_reg()), 8);
                debug_assert_eq!(reg_num(tmp_lo.to_reg()), 9);
                sink.put4(SYNC);
                let loop_top = sink.get_label();
                sink.bind_label(loop_top, &mut state.ctrl_plane);
                sink.put4(enc_x(8, 0, 3, 276)); // lqarx r8, 0, r3
                sink.put4(enc_x(4, 0, 3, 182) | 1); // stqcx. r4, 0, r3
                let bc_off = sink.cur_offset();
                sink.use_label_at_offset(bc_off, loop_top, LabelUse::Branch16);
                sink.put4(enc_bc(bo_for(false), CR0_EQ, 0, false)); // bne- loop
            }

            &Inst::AtomicRmw128 {
                op,
                ref rd_lo,
                ref rd_hi,
                ref tmp_hi,
                addr,
                src_lo,
                src_hi,
            } => {
                // sync
                // loop: lqarx r8, 0, r3          ; old: hi r8, lo r9
                //       <new value into r10:r11 from r8:r9 op r4:r5>
                //       stqcx. r10, 0, r3
                //       bne- loop
                //       isync
                debug_assert_eq!(reg_num(addr), 3);
                debug_assert_eq!(reg_num(src_hi), 4);
                debug_assert_eq!(reg_num(src_lo), 5);
                debug_assert_eq!(reg_num(rd_hi.to_reg()), 8);
                debug_assert_eq!(reg_num(rd_lo.to_reg()), 9);
                debug_assert_eq!(reg_num(tmp_hi.to_reg()), 10);
                use crate::ir::AtomicRmwOp;
                sink.put4(SYNC);
                let loop_top = sink.get_label();
                sink.bind_label(loop_top, &mut state.ctrl_plane);
                sink.put4(enc_x(8, 0, 3, 276)); // lqarx r8, 0, r3
                match op {
                    AtomicRmwOp::Xchg => {
                        sink.put4(enc_x_logic(4, 10, 4, 444)); // mr r10, r4
                        sink.put4(enc_x_logic(5, 11, 5, 444)); // mr r11, r5
                    }
                    AtomicRmwOp::Add => {
                        sink.put4(enc_xo(11, 9, 5, 10)); // addc r11, r9, r5
                        sink.put4(enc_xo(10, 8, 4, 138)); // adde r10, r8, r4
                    }
                    AtomicRmwOp::Sub => {
                        sink.put4(enc_xo(11, 5, 9, 8)); // subfc r11, r5, r9
                        sink.put4(enc_xo(10, 4, 8, 136)); // subfe r10, r4, r8
                    }
                    AtomicRmwOp::And => {
                        sink.put4(enc_x_logic(9, 11, 5, 28));
                        sink.put4(enc_x_logic(8, 10, 4, 28));
                    }
                    AtomicRmwOp::Or => {
                        sink.put4(enc_x_logic(9, 11, 5, 444));
                        sink.put4(enc_x_logic(8, 10, 4, 444));
                    }
                    AtomicRmwOp::Xor => {
                        sink.put4(enc_x_logic(9, 11, 5, 316));
                        sink.put4(enc_x_logic(8, 10, 4, 316));
                    }
                    AtomicRmwOp::Nand => {
                        sink.put4(enc_x_logic(9, 11, 5, 476));
                        sink.put4(enc_x_logic(8, 10, 4, 476));
                    }
                    AtomicRmwOp::Smin
                    | AtomicRmwOp::Smax
                    | AtomicRmwOp::Umin
                    | AtomicRmwOp::Umax => {
                        // Decide by the high halves; only when they are
                        // equal, re-compare cr0 on the low halves
                        // (unsigned, as low halves carry no sign). Then
                        // pick each half of the winner with isel on the
                        // one surviving cr0 bit.
                        let (signed_hi, want) = match op {
                            AtomicRmwOp::Smin => (true, CR0_LT),
                            AtomicRmwOp::Smax => (true, CR0_GT),
                            AtomicRmwOp::Umin => (false, CR0_LT),
                            AtomicRmwOp::Umax => (false, CR0_GT),
                            _ => unreachable!(),
                        };
                        let cmp_hi_xo = if signed_hi { 0 } else { 32 };
                        sink.put4(enc_cmp(0, 1, 8, 4, cmp_hi_xo)); // cmp(l)d r8, r4
                        sink.put4(enc_bc(bo_for(false), CR0_EQ, 8, false)); // bne $+8
                        sink.put4(enc_cmp(0, 1, 9, 5, 32)); // cmpld r9, r5
                        sink.put4(enc_isel(10, 8, 4, want)); // isel r10, r8, r4
                        sink.put4(enc_isel(11, 9, 5, want)); // isel r11, r9, r5
                    }
                }
                sink.put4(enc_x(10, 0, 3, 182) | 1); // stqcx. r10, 0, r3
                let bc_off = sink.cur_offset();
                sink.use_label_at_offset(bc_off, loop_top, LabelUse::Branch16);
                sink.put4(enc_bc(bo_for(false), CR0_EQ, 0, false)); // bne- loop
                sink.put4(ISYNC);
            }

            &Inst::AtomicCas128 {
                ref rd_lo,
                ref rd_hi,
                addr,
                exp_lo,
                exp_hi,
                new_lo,
                new_hi,
            } => {
                // sync
                // loop: lqarx r8, 0, r3
                //       cmpd r8, r4 ; bne done
                //       cmpd r9, r5 ; bne done
                //       stqcx. r6, 0, r3
                //       bne- loop
                // done: isync
                debug_assert_eq!(reg_num(addr), 3);
                debug_assert_eq!(reg_num(exp_hi), 4);
                debug_assert_eq!(reg_num(exp_lo), 5);
                debug_assert_eq!(reg_num(new_hi), 6);
                debug_assert_eq!(reg_num(new_lo), 7);
                debug_assert_eq!(reg_num(rd_hi.to_reg()), 8);
                debug_assert_eq!(reg_num(rd_lo.to_reg()), 9);
                sink.put4(SYNC);
                let loop_top = sink.get_label();
                let done = sink.get_label();
                sink.bind_label(loop_top, &mut state.ctrl_plane);
                sink.put4(enc_x(8, 0, 3, 276)); // lqarx r8, 0, r3
                sink.put4(enc_cmp(0, 1, 8, 4, 32)); // cmpld r8, r4
                emit_bc_to_label(sink, done, CR0_EQ, false); // bne done
                sink.put4(enc_cmp(0, 1, 9, 5, 32)); // cmpld r9, r5
                emit_bc_to_label(sink, done, CR0_EQ, false); // bne done
                sink.put4(enc_x(6, 0, 3, 182) | 1); // stqcx. r6, 0, r3
                let bc_off = sink.cur_offset();
                sink.use_label_at_offset(bc_off, loop_top, LabelUse::Branch16);
                sink.put4(enc_bc(bo_for(false), CR0_EQ, 0, false)); // bne- loop
                sink.bind_label(done, &mut state.ctrl_plane);
                sink.put4(ISYNC);
            }

            &Inst::VecLoad { rd, from, flags } => {
                if let Some(trap_code) = flags.trap_code() {
                    sink.add_trap(trap_code);
                }
                let t = vsr_num(rd.to_reg());
                let (ra, rb) = vec_mem_ea(sink, state, from);
                if emit_info.isa_flags.has_isa_3_0() {
                    sink.put4(enc_vsx_x(t, ra, rb, 268)); // lxvx
                } else {
                    sink.put4(enc_vsx_x(t, ra, rb, 844)); // lxvd2x
                    sink.put4(enc_xxpermdi(t, t, t, 2)); // xxswapd
                }
            }

            &Inst::VecStore { to, rs, flags } => {
                if let Some(trap_code) = flags.trap_code() {
                    sink.add_trap(trap_code);
                }
                let src = vsr_num(rs);
                let (ra, rb) = vec_mem_ea(sink, state, to);
                if emit_info.isa_flags.has_isa_3_0() {
                    sink.put4(enc_vsx_x(src, ra, rb, 396)); // stxvx
                } else {
                    // Swap through the reserved scratch v0: spill
                    // stores are emitted where no temporary can be
                    // allocated.
                    sink.put4(enc_xxpermdi(VEC_SCRATCH, src, src, 2));
                    sink.put4(enc_vsx_x(VEC_SCRATCH, ra, rb, 972)); // stxvd2x
                }
            }

            &Inst::VecAluRRR { op, rd, ra, rb, ty } => {
                let d = vsr_num(rd.to_reg());
                let a = vsr_num(ra);
                let b = vsr_num(rb);
                let word = match op {
                    VecAluOp::And => enc_xx3(d, a, b, 130),
                    VecAluOp::Or => enc_xx3(d, a, b, 146),
                    VecAluOp::Xor => enc_xx3(d, a, b, 154),
                    VecAluOp::Nor => enc_xx3(d, a, b, 162),
                    VecAluOp::AndC => enc_xx3(d, a, b, 138),
                    _ => {
                        // VMX forms, VR numbers only.
                        let (d, a, b) = (d - 32, a - 32, b - 32);
                        enc_vx(d, a, b, vx_xo(op, ty))
                    }
                };
                sink.put4(word);
            }

            &Inst::VecExtractLaneInt { rd, rn, ty, lane } => {
                let rd_n = reg_num(rd.to_reg());
                let src = vsr_num(rn);
                let lane = u32::from(lane);
                // Splat the wanted lane across the v0 scratch, then read
                // BE doubleword 0. Lane index -> BE element number.
                let read_from = match ty.lane_bits() {
                    8 => {
                        sink.put4(enc_vx(VEC_SCRATCH - 32, 15 - lane, src - 32, 524));
                        VEC_SCRATCH
                    }
                    16 => {
                        sink.put4(enc_vx(VEC_SCRATCH - 32, 7 - lane, src - 32, 588));
                        VEC_SCRATCH
                    }
                    32 => {
                        sink.put4(enc_vx(VEC_SCRATCH - 32, 3 - lane, src - 32, 652));
                        VEC_SCRATCH
                    }
                    64 => {
                        // Wasm lane 1 is already BE doubleword 0.
                        if lane == 1 {
                            src
                        } else {
                            sink.put4(enc_xxpermdi(VEC_SCRATCH, src, src, 2));
                            VEC_SCRATCH
                        }
                    }
                    _ => unreachable!("vector lane width {ty}"),
                };
                sink.put4(enc_mxvsrd(read_from, rd_n, 51)); // mfvsrd
            }

            &Inst::VecExtractLaneFpu { rd, rn, ty, lane } => {
                let dst = vsr_num(rd.to_reg());
                let src = vsr_num(rn);
                let lane = u32::from(lane);
                match ty.lane_bits() {
                    32 => {
                        // Splat the lane's word to all positions of the
                        // scratch, then widen the single in BE word 0 to
                        // the double the scalar convention holds.
                        sink.put4(enc_xxspltw(VEC_SCRATCH, src, 3 - lane));
                        sink.put4(enc_xx2(dst, VEC_SCRATCH, 331)); // xscvspdpn
                    }
                    64 => {
                        let dm = if lane == 0 { 2 } else { 0 };
                        sink.put4(enc_xxpermdi(dst, src, src, dm));
                    }
                    _ => unreachable!("float lane width {ty}"),
                }
            }

            &Inst::VecInsertLane {
                rd,
                rv,
                rs,
                ty,
                lane,
            } => {
                // Round-trip through the red zone at -16(r1): store the
                // vector, overwrite the lane's bytes with the scalar
                // store that matches the lane type, reload.
                let dst = vsr_num(rd.to_reg());
                let vec = vsr_num(rv);
                let lane_bytes = u32::from(ty.lane_bits()) / 8;
                let off = -16i16 + (u32::from(lane) * lane_bytes) as i16;
                sink.put4(enc_d(14, 0, 1, (-16i16) as u16)); // addi r0, r1, -16
                if emit_info.isa_flags.has_isa_3_0() {
                    sink.put4(enc_vsx_x(vec, 0, 0, 396)); // stxvx vec, 0, r0
                } else {
                    sink.put4(enc_xxpermdi(VEC_SCRATCH, vec, vec, 2));
                    sink.put4(enc_vsx_x(VEC_SCRATCH, 0, 0, 972)); // stxvd2x
                }
                let s = reg_num(rs);
                let word = match (ty.lane_type().is_float(), ty.lane_bits()) {
                    (false, 8) => enc_d(38, s, 1, off as u16),  // stb
                    (false, 16) => enc_d(44, s, 1, off as u16), // sth
                    (false, 32) => enc_d(36, s, 1, off as u16), // stw
                    (false, 64) => enc_ds(62, s, 1, off, 0),    // std
                    (true, 32) => enc_d(52, s, 1, off as u16),  // stfs
                    (true, 64) => enc_d(54, s, 1, off as u16),  // stfd
                    _ => unreachable!("vector lane width {ty}"),
                };
                sink.put4(word);
                if emit_info.isa_flags.has_isa_3_0() {
                    sink.put4(enc_vsx_x(dst, 0, 0, 268)); // lxvx dst, 0, r0
                } else {
                    sink.put4(enc_vsx_x(dst, 0, 0, 844)); // lxvd2x
                    sink.put4(enc_xxpermdi(dst, dst, dst, 2)); // xxswapd
                }
            }

            &Inst::VecAluRRRR {
                op,
                rd,
                ra,
                rb,
                rc,
            } => {
                let xo = match op {
                    VecAluOp4::MulAddUH => 34,
                    VecAluOp4::MulHiRoundAddSHS => 33,
                    VecAluOp4::Perm => 43,
                };
                sink.put4(enc_va(
                    vsr_num(rd.to_reg()) - 32,
                    vsr_num(ra) - 32,
                    vsr_num(rb) - 32,
                    vsr_num(rc) - 32,
                    xo,
                ));
            }

            &Inst::VecCvt { op, rd, rn } => {
                let xo = match op {
                    VecCvtOp::F32ToI32S => 152, // xvcvspsxws
                    VecCvtOp::F32ToI32U => 136, // xvcvspuxws
                    VecCvtOp::I32ToF32S => 184, // xvcvsxwsp
                    VecCvtOp::I32ToF32U => 168, // xvcvuxwsp
                    VecCvtOp::F64ToI64S => 472, // xvcvdpsxds
                    VecCvtOp::F64ToI64U => 456, // xvcvdpuxds
                    VecCvtOp::I64ToF64S => 504, // xvcvsxddp
                    VecCvtOp::I64ToF64U => 488, // xvcvuxddp
                    VecCvtOp::F64ToF32 => 393,  // xvcvdpsp
                    VecCvtOp::F32ToF64 => 457,  // xvcvspdp
                };
                sink.put4(enc_xx2(vsr_num(rd.to_reg()), vsr_num(rn), xo));
            }

            &Inst::VecSpltImm { rd, imm, ty } => {
                let xo = match ty.lane_bits() {
                    8 => 780,  // vspltisb
                    16 => 844, // vspltish
                    32 => 908, // vspltisw
                    _ => unreachable!("no doubleword splat-immediate exists"),
                };
                sink.put4(enc_vx(
                    vsr_num(rd.to_reg()) - 32,
                    (imm as u32) & 0x1F,
                    0,
                    xo,
                ));
            }

            &Inst::VecUnary { op, rd, rn, ty } => {
                let xo = match op {
                    VecUnaryOp::Popcnt => match ty.lane_bits() {
                        8 => 1795,
                        16 => 1859,
                        32 => 1923,
                        64 => 1987,
                        _ => unreachable!("vector lane width {ty}"),
                    },
                    // Selected by the source lane width; there is no
                    // doubleword source, nothing widens to 128 bits.
                    VecUnaryOp::WidenSLow => match ty.lane_bits() {
                        8 => 654,   // vupklsb
                        16 => 718,  // vupklsh
                        32 => 1742, // vupklsw
                        _ => unreachable!("widen source lane width {ty}"),
                    },
                    VecUnaryOp::WidenSHigh => match ty.lane_bits() {
                        8 => 526,   // vupkhsb
                        16 => 590,  // vupkhsh
                        32 => 1614, // vupkhsw
                        _ => unreachable!("widen source lane width {ty}"),
                    },
                };
                sink.put4(enc_vx(
                    vsr_num(rd.to_reg()) - 32,
                    0,
                    vsr_num(rn) - 32,
                    xo,
                ));
            }

            &Inst::VecRound { rd, rn, mode, ty } => {
                let single = ty.lane_bits() == 32;
                let xo = match (mode, single) {
                    (FpuRoundMode::Ceil, true) => 169,
                    (FpuRoundMode::Ceil, false) => 233,
                    (FpuRoundMode::Floor, true) => 185,
                    (FpuRoundMode::Floor, false) => 249,
                    (FpuRoundMode::Trunc, true) => 153,
                    (FpuRoundMode::Trunc, false) => 217,
                    (FpuRoundMode::Nearest, true) => 171,
                    (FpuRoundMode::Nearest, false) => 235,
                };
                sink.put4(enc_xx2(vsr_num(rd.to_reg()), vsr_num(rn), xo));
            }

            &Inst::VecSel {
                rd,
                if_set,
                if_clear,
                mask,
            } => {
                sink.put4(enc_xxsel(
                    vsr_num(rd.to_reg()),
                    vsr_num(if_clear),
                    vsr_num(if_set),
                    vsr_num(mask),
                ));
            }

            &Inst::VecTestLanes { rd, rn, ty, all } => {
                // xxlxor v0, v0, v0        ; the scratch holds zero
                // vcmpequX. v0, rn, v0     ; sets CR6, result discarded
                // li rd, 1
                // all: li r0, 0 ; isel rd, rd, r0, CR6_EQ
                // any:            isel rd, 0,  rd, CR6_LT
                let rd_n = reg_num(rd.to_reg());
                let rn_v = vsr_num(rn) - 32;
                let scratch = VEC_SCRATCH; // v0, as a 6-bit VSR number
                let scratch_v = scratch - 32;
                sink.put4(enc_xx3(scratch, scratch, scratch, 154)); // xxlxor
                // The record form is the plain opcode plus 1024.
                let xo = vx_xo(VecAluOp::CmpEq, ty) + 1024;
                sink.put4(enc_vx(scratch_v, rn_v, scratch_v, xo));
                sink.put4(enc_d(14, rd_n, 0, 1)); // li rd, 1
                if all {
                    sink.put4(enc_d(14, 0, 0, 0)); // li r0, 0
                    sink.put4(enc_isel(rd_n, rd_n, 0, CR6_EQ));
                } else {
                    sink.put4(enc_isel(rd_n, 0, rd_n, CR6_LT));
                }
            }

            &Inst::VecFpuRRR { op, rd, ra, rb, ty } => {
                let single = ty.lane_bits() == 32;
                let xo = match (op, single) {
                    (VecFpuOp2::Add, true) => 64,
                    (VecFpuOp2::Add, false) => 96,
                    (VecFpuOp2::Sub, true) => 72,
                    (VecFpuOp2::Sub, false) => 104,
                    (VecFpuOp2::Mul, true) => 80,
                    (VecFpuOp2::Mul, false) => 112,
                    (VecFpuOp2::Div, true) => 88,
                    (VecFpuOp2::Div, false) => 120,
                    (VecFpuOp2::CmpEq, true) => 67,
                    (VecFpuOp2::CmpEq, false) => 99,
                    (VecFpuOp2::CmpGt, true) => 75,
                    (VecFpuOp2::CmpGt, false) => 107,
                    (VecFpuOp2::CmpGe, true) => 83,
                    (VecFpuOp2::CmpGe, false) => 115,
                    (VecFpuOp2::Min, true) => 200,
                    (VecFpuOp2::Min, false) => 232,
                    (VecFpuOp2::Max, true) => 192,
                    (VecFpuOp2::Max, false) => 224,
                };
                sink.put4(enc_xx3(
                    vsr_num(rd.to_reg()),
                    vsr_num(ra),
                    vsr_num(rb),
                    xo,
                ));
            }

            &Inst::VecFpuRR { op, rd, rn, ty } => {
                let single = ty.lane_bits() == 32;
                let xo = match (op, single) {
                    (VecFpuOp1::Sqrt, true) => 139,
                    (VecFpuOp1::Sqrt, false) => 203,
                    (VecFpuOp1::Neg, true) => 441,
                    (VecFpuOp1::Neg, false) => 505,
                    (VecFpuOp1::Abs, true) => 409,
                    (VecFpuOp1::Abs, false) => 473,
                };
                sink.put4(enc_xx2(vsr_num(rd.to_reg()), vsr_num(rn), xo));
            }

            &Inst::VecZero { rd } => {
                let d = vsr_num(rd.to_reg());
                sink.put4(enc_xx3(d, d, d, 154)); // xxlxor d, d, d
            }

            &Inst::MovToVec { rd, rn } => {
                sink.put4(enc_mxvsrd(vsr_num(rd.to_reg()), reg_num(rn), 179));
            }

            &Inst::VecSplatLane { rd, rn, ty } => {
                // The scalar sits at the low end of BE doubleword 0.
                let d = vsr_num(rd.to_reg());
                let n = vsr_num(rn);
                let word = match ty.lane_bits() {
                    8 => enc_vx(d - 32, 7, n - 32, 524),  // vspltb 7
                    16 => enc_vx(d - 32, 3, n - 32, 588), // vsplth 3
                    32 => enc_vx(d - 32, 1, n - 32, 652), // vspltw 1
                    64 => enc_xxpermdi(d, n, n, 0),
                    _ => unreachable!("vector lane width {ty}"),
                };
                sink.put4(word);
            }

            &Inst::VecSplatFpr { rd, rn, is_f32 } => {
                let d = vsr_num(rd.to_reg());
                let n = vsr_num(rn);
                let word = if is_f32 {
                    enc_xxspltw(d, n, 0)
                } else {
                    enc_xxpermdi(d, n, n, 0)
                };
                sink.put4(word);
            }

            &Inst::VecPermDi { rd, ra, rb, dm } => {
                sink.put4(enc_xxpermdi(
                    vsr_num(rd.to_reg()),
                    vsr_num(ra),
                    vsr_num(rb),
                    u32::from(dm),
                ));
            }

            &Inst::Fence => sink.put4(SYNC),

            Inst::ReturnCall { info } => {
                emit_return_call_common_sequence(sink, emit_info, state, info);
                sink.add_call_site();
                sink.add_reloc(Reloc::Ppc64Call, &info.dest, 0);
                sink.put4(enc_b(0, false)); // b, not bl: LR is the original RA
            }

            Inst::ReturnCallInd { info } => {
                emit_return_call_common_sequence(sink, emit_info, state, info);
                sink.put4(enc_mtctr(reg_num(info.dest)));
                sink.put4(enc_bctr(false)); // bctr, not bctrl
            }

            Inst::LoadExtName { rd, name, offset } => {
                // bcl 20,31,$+4 ; mflr rd ; ld rd, 12(rd) ; b $+12 ;
                // .quad <sym+offset>
                //
                // LR is dead here (saved by the prologue, clobbered by
                // calls), so the bcl trick is safe. The 8-byte island
                // carries an Abs8 relocation.
                let rd = reg_num(rd.to_reg());
                sink.put4(BCL_20_31_PLUS4);
                sink.put4(enc_mflr(rd));
                sink.put4(enc_ds(58, rd, rd, 12, 0)); // ld rd, 12(rd)
                sink.put4(enc_b(12, false)); // skip the literal
                sink.add_reloc(Reloc::Abs8, &**name, *offset);
                sink.put8(0);
            }

            &Inst::MovFromPReg { rd, rm } => {
                debug_assert!(rm == 1 || rm == 31);
                sink.put4(enc_d_logic(24, u32::from(rm), reg_num(rd.to_reg()), 0));
            }

            &Inst::LabelAddress { rd, label } => {
                // bcl 20,31,$+4 ; mflr rd ; addis rd, rd, 0 ; addi rd, rd, 0
                //
                // LR holds the address of the `mflr`; the addis/addi pair
                // is patched with the label's offset from that point once
                // it is known. LR is dead here, as at LoadExtName.
                let rd = reg_num(rd.to_reg());
                sink.put4(BCL_20_31_PLUS4);
                sink.put4(enc_mflr(rd));
                let hi_lo_off = sink.cur_offset();
                sink.use_label_at_offset(hi_lo_off, label, LabelUse::PCRelHiLo);
                sink.put4(enc_d(15, rd, rd, 0)); // addis rd, rd, 0
                sink.put4(enc_d(14, rd, rd, 0)); // addi rd, rd, 0
            }

            &Inst::Mflr { rd } => sink.put4(enc_mflr(reg_num(rd.to_reg()))),
            &Inst::Mtlr { rs } => sink.put4(enc_mtlr(reg_num(rs))),

            &Inst::Unwind { ref inst } => sink.add_unwind(inst.clone()),
        }

        // Calls and explicit islands emit their own islands and so are
        // allowed to exceed the worst-case size; the return-value loads
        // following a call are unbounded in particular.
        let emits_own_island = matches!(
            self,
            Inst::Call { .. }
                | Inst::CallInd { .. }
                | Inst::EmitIsland { .. }
                | Inst::BrTable { .. }
        );
        if !emits_own_island {
            let end_off = sink.cur_offset();
            debug_assert!(
                (end_off - start_off) <= Inst::worst_case_size(),
                "inst {self:?} longer ({}) than worst-case size",
                end_off - start_off,
            );
        }
    }

    fn pretty_print_inst(&self, state: &mut Self::State) -> String {
        self.print_with_state(state)
    }
}
