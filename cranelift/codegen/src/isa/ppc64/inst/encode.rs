//! ppc64 instruction encoding: pure bit-field helpers.
//!
//! All Power ISA instructions are 32 bits wide. Field layout references
//! use big-endian bit numbering as in the ISA manual (bit 0 = MSB), but
//! the helpers below shift from the LSB for clarity.

use crate::machinst::Reg;

pub(crate) fn reg_num(r: Reg) -> u32 {
    u32::from(r.to_real_reg().unwrap().hw_enc() & 31)
}

/// D-form, arithmetic layout: `opcd | RT | RA | D16`.
/// Used by addi (14), addis (15) and the load/store opcodes.
/// N.B.: RA=0 means "literal zero" for addi/addis and load/store bases.
pub(crate) fn enc_d(opcd: u32, rt: u32, ra: u32, imm16: u16) -> u32 {
    debug_assert!(opcd < 64 && rt < 32 && ra < 32);
    (opcd << 26) | (rt << 21) | (ra << 16) | u32::from(imm16)
}

/// D-form, logical layout: `opcd | RS | RA | UI16`, destination in RA.
/// Used by ori (24), oris (25), xori (26), xoris (27), andi. (28).
pub(crate) fn enc_d_logic(opcd: u32, rs: u32, ra: u32, uimm16: u16) -> u32 {
    debug_assert!(opcd < 64 && rs < 32 && ra < 32);
    (opcd << 26) | (rs << 21) | (ra << 16) | u32::from(uimm16)
}

/// DS-form: `opcd | RT | RA | DS14 | XO2`. The displacement must be
/// 4-aligned; its low two bits carry the extended opcode.
/// Used by ld (58/0), lwa (58/2), std (62/0), stdu (62/1).
pub(crate) fn enc_ds(opcd: u32, rt: u32, ra: u32, imm16: i16, xo2: u32) -> u32 {
    debug_assert_eq!(imm16 & 3, 0, "DS-form displacement must be 4-aligned");
    debug_assert!(xo2 < 4);
    (opcd << 26) | (rt << 21) | (ra << 16) | (imm16 as u16 as u32 & 0xFFFC) | xo2
}

/// X-form with an explicit primary opcode: `opcd | RT | RA | RB | XO10 | Rc`.
/// Primary opcode 31 covers the integer instructions and 63 the
/// floating-point ones, which share this field layout.
pub(crate) fn enc_x_opcd(opcd: u32, rt: u32, ra: u32, rb: u32, xo: u32) -> u32 {
    debug_assert!(opcd < 64 && rt < 32 && ra < 32 && rb < 32 && xo < 1024);
    (opcd << 26) | (rt << 21) | (ra << 16) | (rb << 11) | (xo << 1)
}

/// X-form: `31 | RT | RA | RB | XO10 | Rc`.
pub(crate) fn enc_x(rt: u32, ra: u32, rb: u32, xo: u32) -> u32 {
    enc_x_opcd(31, rt, ra, rb, xo)
}

/// A-form: `opcd | FRT | FRA | FRB | FRC | XO5 | Rc`. `opcd` is 63 for the
/// double-precision forms and 59 for the single-precision ones. Note that
/// `fmul` takes its second operand in FRC rather than FRB.
pub(crate) fn enc_a(opcd: u32, frt: u32, fra: u32, frb: u32, frc: u32, xo: u32) -> u32 {
    debug_assert!(opcd < 64 && frt < 32 && fra < 32 && frb < 32 && frc < 32 && xo < 32);
    (opcd << 26) | (frt << 21) | (fra << 16) | (frb << 11) | (frc << 6) | (xo << 1)
}

/// XL-form condition-register logical: `19 | BT | BA | BB | XO10 | 0`.
pub(crate) fn enc_xl(bt: u32, ba: u32, bb: u32, xo: u32) -> u32 {
    debug_assert!(bt < 32 && ba < 32 && bb < 32 && xo < 1024);
    (19 << 26) | (bt << 21) | (ba << 16) | (bb << 11) | (xo << 1)
}

/// `cror BT, BA, BB`: BT = BA | BB, over condition-register bits.
pub(crate) fn enc_cror(bt: u32, ba: u32, bb: u32) -> u32 {
    enc_xl(bt, ba, bb, 449)
}

/// XX1-form, used here only for the GPR/FPR transfers `mtvsrd` (XO 179,
/// GPR to FPR) and `mfvsrd` (XO 51, FPR to GPR). The `vsr` field selects
/// VSR 0-31, which alias the FPRs, so the extension bit is always zero.
pub(crate) fn enc_xx1(vsr: u32, gpr: u32, xo: u32) -> u32 {
    debug_assert!(vsr < 32 && gpr < 32);
    (31 << 26) | (vsr << 21) | (gpr << 16) | (xo << 1)
}

/// XO-form: `31 | RT | RA | RB | OE=0 | XO9 | Rc=0`.
/// Used by add (266), subf (40), mulld (233).
pub(crate) fn enc_xo(rt: u32, ra: u32, rb: u32, xo: u32) -> u32 {
    debug_assert!(xo < 512);
    (31 << 26) | (rt << 21) | (ra << 16) | (rb << 11) | (xo << 1)
}

/// X-form logical layout: `31 | RS | RA | RB | XO10 | Rc=0`, destination
/// in RA. Used by and (28), or (444), xor (316), extsb (954), extsh
/// (922), extsw (986).
pub(crate) fn enc_x_logic(rs: u32, ra: u32, rb: u32, xo: u32) -> u32 {
    (31 << 26) | (rs << 21) | (ra << 16) | (rb << 11) | (xo << 1)
}

/// MD-form rldicl/rldicr: `30 | RS | RA | sh[0:4] | m[0:4] m[5] | XO3 | sh[5] | Rc=0`.
/// `xo3` is 0 for rldicl (clear left), 1 for rldicr (clear right).
pub(crate) fn enc_md(rs: u32, ra: u32, sh: u32, m: u32, xo3: u32) -> u32 {
    debug_assert!(sh < 64 && m < 64 && xo3 < 8);
    (30 << 26)
        | (rs << 21)
        | (ra << 16)
        | ((sh & 0x1F) << 11)
        | ((m & 0x1F) << 6)
        | ((m >> 5) << 5)
        | (xo3 << 2)
        | ((sh >> 5) << 1)
}

/// M-form `rlwinm`: `21 | RS | RA | SH | MB | ME | Rc=0`. The word
/// rotate-and-mask instruction, which supplies the shift-by-immediate
/// forms for 32-bit shifts.
pub(crate) fn enc_m(rs: u32, ra: u32, sh: u32, mb: u32, me: u32) -> u32 {
    debug_assert!(sh < 32 && mb < 32 && me < 32);
    (21 << 26) | (rs << 21) | (ra << 16) | (sh << 11) | (mb << 6) | (me << 1)
}

/// XS-form `sradi`: `31 | RS | RA | sh[0:4] | XO9 | sh[5] | Rc=0`.
pub(crate) fn enc_xs(rs: u32, ra: u32, sh: u32) -> u32 {
    debug_assert!(sh < 64);
    (31 << 26) | (rs << 21) | (ra << 16) | ((sh & 0x1F) << 11) | (413 << 2) | ((sh >> 5) << 1)
}

/// Compare, X-form: `31 | BF | 0 | L | RA | RB | XO10 | 0`.
/// `xo` is 0 for cmp (signed), 32 for cmpl (logical); `l` selects 64-bit.
pub(crate) fn enc_cmp(bf: u32, l: u32, ra: u32, rb: u32, xo: u32) -> u32 {
    debug_assert!(bf < 8 && l < 2);
    (31 << 26) | (bf << 23) | (l << 21) | (ra << 16) | (rb << 11) | (xo << 1)
}

/// Compare immediate, D-form: `opcd | BF | 0 | L | RA | IMM16`.
/// `opcd` is 11 for cmpi (signed) or 10 for cmpli (logical).
pub(crate) fn enc_cmpi(opcd: u32, bf: u32, l: u32, ra: u32, imm: u16) -> u32 {
    debug_assert!(opcd == 10 || opcd == 11);
    debug_assert!(bf < 8 && l < 2);
    (opcd << 26) | (bf << 23) | (l << 21) | (ra << 16) | u32::from(imm)
}

/// isel, A-form: `31 | RT | RA | RB | BC5 | 15 | 0`.
/// RT = CR[bc] ? (RA == 0 ? 0 : GPR[RA]) : GPR[RB].
pub(crate) fn enc_isel(rt: u32, ra: u32, rb: u32, bc: u32) -> u32 {
    debug_assert!(bc < 32);
    (31 << 26) | (rt << 21) | (ra << 16) | (rb << 11) | (bc << 6) | (15 << 1)
}

/// I-form branch: `18 | LI24 | AA | LK`. `off` is a signed byte offset,
/// must be 4-aligned, range ±32 MiB.
pub(crate) fn enc_b(off: i32, lk: bool) -> u32 {
    debug_assert_eq!(off & 3, 0);
    debug_assert!(off >= -(1 << 25) && off < (1 << 25));
    (18 << 26) | ((off as u32) & 0x03FF_FFFC) | u32::from(lk)
}

/// B-form conditional branch: `16 | BO | BI | BD14 | AA | LK`. `off` is a
/// signed byte offset, 4-aligned, range ±32 KiB.
pub(crate) fn enc_bc(bo: u32, bi: u32, off: i32, lk: bool) -> u32 {
    debug_assert!(bo < 32 && bi < 32);
    debug_assert_eq!(off & 3, 0);
    debug_assert!(off >= -(1 << 15) && off < (1 << 15));
    (16 << 26) | (bo << 21) | (bi << 16) | ((off as u32) & 0xFFFC) | u32::from(lk)
}

/// The BO field for "branch if CR bit set" (12) or "clear" (4).
pub(crate) fn bo_for(polarity: bool) -> u32 {
    if polarity { 12 } else { 4 }
}

/// mfspr/mtspr: `31 | RT | spr[5:9] spr[0:4] | XO10 | 0`. The 10-bit SPR
/// field is stored with its halves swapped.
fn enc_spr(rt: u32, spr: u32, xo: u32) -> u32 {
    (31 << 26) | (rt << 21) | ((spr & 0x1F) << 16) | ((spr >> 5) << 11) | (xo << 1)
}

pub(crate) fn enc_mflr(rt: u32) -> u32 {
    enc_spr(rt, 8, 339)
}

pub(crate) fn enc_mtlr(rs: u32) -> u32 {
    enc_spr(rs, 8, 467)
}

pub(crate) fn enc_mtctr(rs: u32) -> u32 {
    enc_spr(rs, 9, 467)
}

/// bclr (blr when BO=20): `19 | BO | BI | 0 | XO=16 | LK`.
pub(crate) fn enc_blr() -> u32 {
    (19 << 26) | (20 << 21) | (16 << 1)
}

/// bcctr/bcctrl: `19 | BO=20 | 0 | 0 | XO=528 | LK`.
pub(crate) fn enc_bctr(lk: bool) -> u32 {
    (19 << 26) | (20 << 21) | (528 << 1) | u32::from(lk)
}

/// `bcl 20,31,$+4`: the classic "read the PC" idiom. Branches to the
/// next instruction with LK=1, so LR = address of the following
/// instruction. BO=20/BI=31 is the form that does not touch the
/// branch-predictor's link stack on modern cores.
pub(crate) const BCL_20_31_PLUS4: u32 = (16 << 26) | (20 << 21) | (31 << 16) | 4 | 1;

/// The unconditional trap: an all-zeros word, which the Power ISA
/// permanently reserves as an invalid instruction. This raises SIGILL,
/// which Wasmtime's signal handler listens for; the architecturally
/// cleaner `tw 31,0,0` (`trap`) raises SIGTRAP, which it does not. The
/// kernel delivers the signal with NIP pointing at the faulting word, so
/// no address correction is needed (unlike s390x).
pub(crate) const TRAP_INSTRUCTION: u32 = 0;

/// A no-op: `ori r0, r0, 0`.
pub(crate) const NOP_INSTRUCTION: u32 = 24 << 26;

/// fmr, X-form on FPRs: `63 | FRT | 0 | FRB | 72 | 0`.
pub(crate) fn enc_fmr(frt: u32, frb: u32) -> u32 {
    (63 << 26) | (frt << 21) | (frb << 11) | (72 << 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_encodings() {
        // Values cross-checked against GNU as output.
        assert_eq!(enc_d(14, 3, 0, 1), 0x3860_0001); // li r3, 1
        assert_eq!(enc_d(15, 4, 0, 0x1234), 0x3C80_1234); // lis r4, 0x1234
        assert_eq!(enc_d_logic(24, 0, 0, 0), NOP_INSTRUCTION); // nop
        // Shift-by-immediate forms, all verified against llvm-mc.
        assert_eq!(enc_md(4, 3, 7, 56, 1), 0x7883_3E24); // sldi r3, r4, 7
        assert_eq!(enc_md(4, 3, 57, 7, 0), 0x7883_C9C2); // srdi r3, r4, 7
        assert_eq!(enc_md(4, 3, 7, 0, 0), 0x7883_3800); // rotldi r3, r4, 7
        assert_eq!(enc_xs(4, 3, 7), 0x7C83_3E74); // sradi r3, r4, 7
        assert_eq!(enc_m(4, 3, 7, 0, 24), 0x5483_3830); // slwi r3, r4, 7
        assert_eq!(enc_m(4, 3, 25, 7, 31), 0x5483_C9FE); // srwi r3, r4, 7
        assert_eq!(enc_m(4, 3, 7, 0, 31), 0x5483_383E); // rotlwi r3, r4, 7
        assert_eq!(enc_x_logic(4, 3, 7, 824), 0x7C83_3E70); // srawi r3, r4, 7
        assert_eq!(enc_xo(3, 4, 5, 266), 0x7C64_2A14); // add r3, r4, r5
        assert_eq!(enc_xo(3, 4, 5, 40), 0x7C64_2850); // subf r3, r4, r5
        assert_eq!(enc_xo(3, 4, 5, 10), 0x7C64_2814); // addc r3, r4, r5
        assert_eq!(enc_xo(3, 4, 5, 138), 0x7C64_2914); // adde r3, r4, r5
        assert_eq!(enc_xo(3, 5, 4, 8), 0x7C65_2010); // subfc r3, r5, r4
        assert_eq!(enc_xo(3, 5, 4, 136), 0x7C65_2110); // subfe r3, r5, r4
        assert_eq!(enc_x_logic(4, 3, 5, 444), 0x7C83_2B78); // or r3, r4, r5
        assert_eq!(enc_ds(58, 3, 1, 16, 0), 0xE861_0010); // ld r3, 16(r1)
        assert_eq!(enc_ds(62, 3, 1, 16, 0), 0xF861_0010); // std r3, 16(r1)
        assert_eq!(enc_mflr(0), 0x7C08_02A6); // mflr r0
        assert_eq!(enc_mtlr(0), 0x7C08_03A6); // mtlr r0
        assert_eq!(enc_mtctr(12), 0x7D89_03A6); // mtctr r12
        assert_eq!(enc_blr(), 0x4E80_0020); // blr
        assert_eq!(enc_bctr(true), 0x4E80_0421); // bctrl
        assert_eq!(BCL_20_31_PLUS4, 0x429F_0005); // bcl 20,31,$+4
        assert_eq!(enc_b(8, false), 0x4800_0008); // b $+8
        assert_eq!(enc_b(-4, true), 0x4BFF_FFFD); // bl $-4
        assert_eq!(enc_fmr(1, 2), 0xFC20_1090); // fmr f1, f2
        // rldicl r3, r4, 0, 32 == clrldi r3, r4, 32 (zero-extend i32)
        assert_eq!(enc_md(4, 3, 0, 32, 0), 0x7883_0020);
        assert_eq!(enc_md(4, 3, 32, 31, 1), 0x7883_07C6); // sldi r3, r4, 32
        assert_eq!(enc_isel(3, 4, 5, 0), 0x7C64_281E); // isellt r3, r4, r5
        assert_eq!(enc_cmpi(11, 0, 1, 3, 0), 0x2C23_0000); // cmpdi r3, 0
        assert_eq!(enc_cmpi(11, 0, 1, 3, -1i16 as u16), 0x2C23_FFFF); // cmpdi r3, -1
        assert_eq!(enc_cmpi(11, 0, 0, 3, 0), 0x2C03_0000); // cmpwi r3, 0
        assert_eq!(enc_cmp(0, 0, 3, 4, 32), 0x7C03_2040); // cmplw r3, r4
        assert_eq!(enc_d_logic(28, 4, 7, 63), 0x7087_003F); // andi. r7, r4, 63
        assert_eq!(enc_xo(3, 4, 0, 104), 0x7C64_00D0); // neg r3, r4
        assert_eq!(enc_x_logic(4, 3, 4, 124), 0x7C83_20F8); // nor r3, r4, r4
        assert_eq!(enc_x_logic(0, 0, 3, 60), 0x7C00_1878); // andc r0, r0, r3
        assert_eq!(enc_x_logic(3, 3, 4, 794), 0x7C63_2634); // srad r3, r3, r4
        assert_eq!(enc_x_logic(4, 3, 5, 24), 0x7C83_2830); // slw r3, r4, r5
        assert_eq!(enc_x_logic(4, 3, 0, 58), 0x7C83_0074); // cntlzd r3, r4
        assert_eq!(enc_x_logic(4, 3, 0, 506), 0x7C83_03F4); // popcntd r3, r4
        assert_eq!(enc_x_logic(4, 3, 0, 570), 0x7C83_0474); // cnttzd r3, r4
        assert_eq!(enc_xo(3, 4, 5, 489), 0x7C64_2BD2); // divd r3, r4, r5
        assert_eq!(enc_xo(3, 4, 5, 457), 0x7C64_2B92); // divdu r3, r4, r5
        // The modulo instructions are X-form: a 10-bit extended opcode.
        assert_eq!(enc_x(3, 5, 4, 777), 0x7C65_2612); // modsd r3, r5, r4
        assert_eq!(enc_x(3, 5, 4, 265), 0x7C65_2212); // modud r3, r5, r4
    }

    #[test]
    fn well_known_fp_encodings() {
        // Also cross-checked against GNU as / llvm-mc.
        assert_eq!(enc_a(63, 9, 1, 2, 0, 21), 0xFD21_102A); // fadd f9, f1, f2
        assert_eq!(enc_a(59, 5, 1, 2, 0, 21), 0xECA1_102A); // fadds f5, f1, f2
        // `fmul` takes its second operand in FRC, not FRB.
        assert_eq!(enc_a(63, 9, 9, 0, 1, 25), 0xFD29_0072); // fmul f9, f9, f1
        assert_eq!(enc_a(63, 9, 9, 2, 0, 18), 0xFD29_1024); // fdiv f9, f9, f2
        assert_eq!(enc_a(63, 1, 0, 9, 0, 22), 0xFC20_482C); // fsqrt f1, f9
        assert_eq!(enc_a(59, 1, 0, 5, 0, 22), 0xEC20_282C); // fsqrts f1, f5
        // fmadd computes FRA * FRC + FRB.
        assert_eq!(enc_a(63, 7, 1, 3, 2, 29), 0xFCE1_18BA); // fmadd f7, f1, f2, f3
        assert_eq!(enc_x_opcd(63, 9, 0, 9, 40), 0xFD20_4850); // fneg f9, f9
        assert_eq!(enc_x_opcd(63, 9, 0, 9, 264), 0xFD20_4A10); // fabs f9, f9
        assert_eq!(enc_x_opcd(63, 7, 0, 7, 12), 0xFCE0_3818); // frsp f7, f7
        assert_eq!(enc_x_opcd(63, 7, 2, 7, 8), 0xFCE2_3810); // fcpsgn f7, f2, f7
        assert_eq!(enc_x_opcd(63, 0, 1, 2, 0), 0xFC01_1000); // fcmpu cr0, f1, f2
        assert_eq!(enc_x_opcd(63, 0, 0, 0, 846), 0xFC00_069C); // fcfid f0, f0
        assert_eq!(enc_x_opcd(63, 1, 0, 1, 974), 0xFC20_0F9C); // fcfidu f1, f1
        assert_eq!(enc_x_opcd(59, 1, 0, 4, 846), 0xEC20_269C); // fcfids f1, f4
        assert_eq!(enc_cror(2, 0, 2), 0x4C40_1382); // cror cr0eq, cr0lt, cr0eq
        assert_eq!(enc_xx1(6, 4, 179), 0x7CC4_0166); // mtvsrd f6, r4
        assert_eq!(enc_xx1(1, 4, 51), 0x7C24_0066); // mfvsrd r4, f1
    }
}
