# Plan: ppc64le JIT backend for Cranelift (Wasmtime)

*Drafted 2026-08-17 against wasmtime `main` @ `bc2f967927`. Untracked working
document — not part of the upstream repo.*

## 1. Context and starting position

- Upstream demand exists since 2020
  ([issue #1183](https://github.com/bytecodealliance/wasmtime/issues/1183)),
  but Cranelift today ships only x64, aarch64, s390x, riscv64 (plus Pulley,
  the portable interpreter, which is what ppc64le currently falls back to).
- The repo already treats `powerpc64le-unknown-linux-gnu` as a first-class
  *known* target: it is listed as Tier 3 in `docs/stability-tiers.md:115`
  ("CI testing, full-time maintainer" missing), and it is deliberately used
  in CI as the "no Cranelift backend" canary (`.github/workflows/main.yml:645`
  — the comment there even says to swap it out when someone adds ppc64
  support; `loongarch64` is the natural replacement canary).
- New architectures enter at
  [Tier 3](https://docs.wasmtime.dev/stability-tiers.html), which requires a
  named, committed maintainer and correctness at inclusion — but no
  CI/fuzzing initially. Per
  [`docs/stability-platform-support.md`](https://docs.wasmtime.dev/stability-platform-support.html)
  and the
  [Cranelift README](https://github.com/bytecodealliance/wasmtime/blob/main/cranelift/README.md),
  maintainers ask that you open a discussion with them **before** starting a
  backend. That conversation (likely a Bytecode Alliance RFC or at least a
  tracking issue) is step zero.

## 2. Using the SpiderMonkey ppc64le patch as reference

`0004-Add-PPC64LE-JIT-backend.patch` (Firefox/SpiderMonkey, based on Cameron
Kaiser / Justin Hibbits' gecko-dev port) is valuable, but as a **knowledge
source, not a code source**:

- **Licence**: the patch is MPL-2.0 (SpiderMonkey); Wasmtime is Apache-2.0
  WITH LLVM-exception. Code cannot be copied across. Facts — instruction
  encodings, ABI rules, register conventions — are not copyrightable, and
  the Cranelift backend will be architecturally unrecognisable anyway (ISLE
  rules vs a C++ MacroAssembler).
- The Bytecode Alliance AI Tool Use Policy (see `AGENTS.md`) applies: any
  eventual upstream PR must be opened by a human, with human authors only.

What transfers directly:

| From the patch | To the Cranelift backend |
|---|---|
| `Assembler-ppc64.h` opcode constants (~2,100 lines of verified `PPC_*` encodings) | Cross-check table for `inst/encode.rs` bit-field encoders and `emit_tests.rs` golden values |
| Register conventions (`Architecture-ppc64.h`): r0 not usable as load/store base, r1 SP, r2 TOC, r13 thread pointer — all non-allocatable; r14–r31 / f14–f31 / vr20–vr31 callee-saved; VRs (VSR32–63) physically distinct from FPRs (VSR0–31) | `create_reg_environment()` and clobber sets in `abi.rs`. The FPR/VR split maps cleanly onto Cranelift's separate `Float` and `Vector` register classes |
| ELFv2 frame facts: LR at caller-SP+16, CR at +8, TOC save at +24, 32-byte minimum frame, 16-byte SP alignment, ±32 MB branch range (`JumpImmediateRange`) | `abi.rs` frame layout, `LabelUse` veneer thresholds |
| Feature detection: `getauxval(AT_HWCAP2)`, `PPC_FEATURE2_ARCH_3_00` (POWER9) / `ARCH_3_1` (POWER10), plus force-override env vars for testing | `cranelift/native` flag inference + the two Wasmtime-side duplicates (§ Phase 3) |
| Icache flush: `dcbst` loop → `sync` → `icbi` loop → `sync` → `isync` (the patch notes GCC's `__builtin___clear_cache` alone is insufficient; sequence matches kernel/QEMU) | `crates/jit-icache-coherence` |
| POWER8-vs-POWER9 lowering knowledge: `modsd`/`modud` are ISA 3.0, XER overflow handling differs, ctz emulation pitfalls, VSX lane-extraction bugs the patch fixed | ISLE rule design + `has_isa_3_0` guards + targeted CLIF runtests reproducing the same corner cases (`mod-pow2-negative-dividend`, `select-i32-condition-high-bits`, `extmul-aliased-dest`, min/max corner cases, …) |

Does *not* transfer: the 7.4k-line simulator (Wasmtime CI uses QEMU),
MoveEmitter/Trampoline/IC machinery (regalloc2 and Cranelift-generated
trampolines replace all of it).

## 3. Key design decisions

1. **Target**: `powerpc64le-unknown-linux-gnu`, ELFv2 ABI, little-endian
   first. This dodges the s390x `LaneOrder` big-endian machinery initially;
   `target_lexicon::Architecture::Powerpc64le` already exists in the pinned
   target-lexicon 0.13.5. Big-endian is Phase 7.
2. **Baseline ISA: POWER8** (first LE-capable generation, distro baseline),
   with ISA flags `has_isa_3_0` (POWER9: `modsd/modud`, `setb`, `mffprd`,
   FP16 conversions, `addpcis`) and `has_isa_3_1` (POWER10: prefixed
   instructions, real PC-relative addressing via `paddi`/`pld`). Baseline
   code uses TOC-free absolute/immediate materialisation
   (`lis/ori/rldicr/oris/ori` sequences and `MachBuffer` constant islands)
   since pre-P10 PPC64 has **no PC-relative addressing** — the single
   biggest codegen difference from every existing backend; decide the
   strategy in Phase 1.
3. **Backend structure**: riscv64 skeleton (fixed-width 32-bit encodings →
   dedicated `inst/encode.rs`; fully ISLE-lowered, thin `lower.rs`) combined
   with s390x conventions where better: define instruction enums in
   `inst.isle` rather than large hand-written `args.rs`; copy s390x's
   `abi.rs` documentation style. s390x also sets precedent for
   per-calling-convention `MachineEnv`s if ELFv2 vs `tail` need different
   register environments.
4. **Frame/unwind model**: real frame pointer (SpiderMonkey uses r31)
   holding a two-slot `(old FP, return address)` record, because Wasmtime's
   `crates/unwinder` walks frames via `get_next_older_pc_from_fp` + fixed
   offsets. Keep the ELFv2 back-chain word at 0(r1) valid for native tools.
   DWARF register numbering per ELFv2 (r0–31 → 0–31, f0–31 → 32–63, LR = 65)
   in `inst/unwind/systemv.rs`.
5. **Calling conventions**: `SystemV` (= ELFv2) for host calls, plus
   Cranelift's `Tail` convention (Wasmtime compiles all wasm with it)
   including exception payload registers for `try_call` — the
   `resume_to_exception_handler` asm in the unwinder must mirror exactly
   what the backend emits.
6. **Register classes**: Int = GPRs (r3–r12, r14–r31 allocatable;
   r0/r1/r2/r13 reserved — r0's "reads as zero when used as base" quirk must
   be respected in `AMode` legalisation); Float = FPRs; Vector = VRs. v128
   disabled at first via a `supports_simd()`-style gate (riscv64 `has_v`
   precedent).
7. **Relocations**: new `Reloc::Ppc64Rel24` (calls, ±32 MB, veneer support
   in `LabelUse`), `Abs8`, later TOC/TLS variants — added in
   `binemit/mod.rs` and mapped in `cranelift/object` (`R_PPC64_REL24`,
   `R_PPC64_ADDR64`, …) and `cranelift/jit`.
8. **Big-endian forward-compatibility (from Phase 7)**: even while only LE
   is implemented, thread `MemFlags` endianness through every load/store
   lowering rule and keep vector lane indices behind a helper from day one.
   Nearly free now, a miserable retrofit later.

## 4. Phased work breakdown

### Phase 0 — Socialise (small, do first) — *drafts ready 2026-08-17*

Talk to Cranelift maintainers, revive the tracking issue, name the
maintainer(s). Tier 3 is unreachable without this. State up front whether
big-endian (Phase 7) is in committed maintenance scope or best-effort — it
changes the CI commitment.

Drafts are in `ppc64-phase0/`, to be posted **by a human** (BA AI Tool Use
Policy — no AI-opened issues/PRs, no AI co-authors):

1. `01-zulip-pre-rfc.md` — post first. riscv64 landed *without* an RFC
   (PR #4271 + follow-ups), so the opening question is procedural: RFC, or
   tracking issue plus incremental PRs?
2. `02-issue-1183-comment.md` — revive wasmtime#1183 (still open, already
   labelled `cranelift:new-target`) rather than filing a duplicate. Post
   after the Zulip thread so it can reference the agreed direction.
3. `03-rfc-draft.md` — only if maintainers ask for one. Follows
   `template-draft.md` from bytecodealliance/rfcs; open as a *draft* PR
   adding it to `accepted/`.

Still to fill in before posting: name/affiliation, realistic hours per week
and whether the work is funded, co-maintainers, and the big-endian scope
answer.

#### Verified development hardware (2026-08-17)

| Alias | Actually is | PVR | Notes |
|---|---|---|---|
| `power9` | POWER9, bare metal | `004e1203` (rev 2.3) | Fedora 44, 32 cores, 63 GB |
| `power10` | POWER10 (architected) | `0080 0200` (rev 2.0) | Fedora 44, 8 cores, 15 GB |
| `power8` | **POWER9 LPAR, not POWER8** | `004e1202` (rev 2.2) | Fedora 40, 8 cores, 15 GB |

All three are ppc64le with **64 KiB pages**, confirming the
`page_size_align()` = 64 KiB decision in Phase 3.

`AT_HWCAP2` readings validate the SpiderMonkey patch's detection constants
against real silicon: POWER9 reports `0xbef00000` and POWER10 `0xbef60000`,
i.e. `ARCH_2_07` baseline on both, `ARCH_3_00` on both, and `ARCH_3_1` plus
`MMA` only on POWER10. `ISEL` is set on all three (relevant to the Phase 6
`isel` fast path). This is direct evidence that the Phase 3
`getauxval(AT_HWCAP2)` approach with `PPC_FEATURE2_ARCH_3_00 = 0x00800000`
and `ARCH_3_1 = 0x00040000` works as designed.

**Gap: no genuine POWER8.** The `power8` alias is a POWER9 LPAR in
architected mode advertising `arch_3_00`, so it runs baseline-compiled code
correctly but cannot validate that feature detection properly *declines* the
ISA 3.0 path. Options, best first: boot that LPAR in POWER8 compatibility
mode (`-cpu power8` under KVM, or PowerVM compat mode) for real `arch_2_07`-
only coverage; QEMU `-cpu power8`; or a force-baseline environment override
of the kind the SpiderMonkey patch used (`MOZ_PPC64_FORCE_POWER8`), which
exercises codegen paths but not silicon behaviour. Worth closing before
claiming POWER8 coverage publicly.

### Phase 1 — Scaffolding + minimal codegen (≈2–4 weeks) — *in progress*

Working branch: `ppc64-backend`.

**Done (2026-08-17, Opus):** meta-crate settings and the Cargo feature — the
slice that is genuinely independent of the ABI design.

- `cranelift/codegen/meta/src/isa/ppc64.rs` — POWER8 baseline, `has_isa_3_0`
  and `has_isa_3_1` flags, cumulative `power9`/`power10` presets.
- `cranelift/codegen/meta/src/isa/mod.rs` — `Isa::Ppc64` threaded through
  `from_arch` (`powerpc64le` only; BE deliberately unmapped), `all()`,
  `Display`, `define()`.
- `cranelift/codegen/Cargo.toml` — `ppc64 = []`.

Verified: `cargo check -p cranelift-codegen-meta`, `cargo check -p
cranelift-codegen --features ppc64` (generates `settings-ppc64.rs` with both
flags and both presets), and `--features all-arch` still builds.

**Deliberately deferred, both would break the build or CI today:**

- `ppc64` is *not* in `all-native-arch` — adding it enables a backend that
  does not exist yet. Add when the skeleton compiles.
- `"powerpc64le"` is *not* in `ALL_ARCHITECTURES` (`codegen/src/isa/mod.rs`)
  — the `cranelift-icache` fuzz target enumerates that list and would drive
  a backend that cannot lower yet. Add in Phase 4.

**Done (2026-08-17, Fable 5): the backend skeleton compiles and generates
correct code.** `clif-util compile --target powerpc64le` works — the Phase
1 exit criterion is met. ~2.4k lines under `cranelift/codegen/src/isa/ppc64/`:

- `abi.rs` — full `ABIMachineSpec`: ELFv2 arg passing (r3–r10/f1–f13, rets
  r3–r4/f1–f2, stack args at entry-SP+32 keeping the ELFv2 header),
  aarch64-style frame record (`[FP]=old FP, [FP+8]=RA`, FP=r31; back-chain
  NOT maintained — documented), `MachineEnv` (non-allocatable: r0/r1/r2/
  r11/r13/r31; **r12 allocatable but fixed at indirect calls** — regalloc2
  requires fixed-use regs be allocatable), clobber sets, probestack.
- `inst.isle`/`lower.isle` — MInst enum + Phase-1 lowerings: iconst, ALU
  (add/sub/and/or/xor/mul), extensions, loads/stores (via the
  `little_or_native_endian` seam — the BE hook from §7a), icmp (cmp+li+isel
  materialization, cr0 only), brif fusion (cmp+bc), jump, trap, stack_addr,
  return, direct/indirect calls.
- `inst/encode.rs` — D/DS/X/XO/MD-form encoders with a golden-value unit
  test cross-checked against GNU as.
- `inst/emit.rs` — emission incl. 1–5-inst constant materialization,
  big-offset fallback (materialize into r0, X-form indexed), CondBr with
  MachBuffer inversion protocol (only the 4-byte `bc` registered, compare
  outside), LoadExtName via `bcl 20,31,+4; mflr; ld; b; .quad` + Abs8 (no
  new reloc needed for symbols; only `Reloc::Ppc64Call` for `bl`).
- `inst/mod.rs` — MachInst impl, LabelUse (Branch26 no-veneer, Branch16 →
  `b` veneer), TRAP_OPCODE = `tw 31,0,0`.
- `inst/unwind/systemv.rs` — ELFv2 DWARF numbering (GPR 0–31, FPR 32–63,
  LR 65, VR 77+), CIE with code-align 4.
- Registration: `lookup()` arm, `all-native-arch`, meta ISLE entry,
  `Reloc::Ppc64Call` in binemit, capstone `arch_powerpc` feature (root
  `Cargo.toml`) for `to_capstone`.
- `cranelift/filetests/filetests/isa/ppc64/` — 4 blessed precise-output
  tests (arithmetic, control-flow, memory, call), all passing.

Verified: zero warnings with `--features ppc64`; `all-arch` and default
builds clean; encoder unit test green; 5-function smoke file disassembles
to correct PPC64 (verified add/mulld/cmpd/blt/ld/std/lbz/mtctr/bctrl,
prologue/epilogue, 5-inst constant build by hand against capstone).

**Still open in Phase 1 → 2:**
- `trapz`/`trapnz`, `select`, shifts, div/rem, FP ALU — next lowering batch.
- `ALL_ARCHITECTURES` still deliberately excludes ppc64 (icache fuzz gate).
- Runtests (`test run`) need Phase 3/4 (host execution on ppc64le).
- Emit tests (`emit_tests.rs`) not started — validate against `llvm-mc` and
  the SpiderMonkey opcode table when written.

Registration glue reference (all landed):

- `cranelift/codegen/meta/src/isa/ppc64.rs` (settings) + `Isa::Ppc64` in
  `meta/src/isa/mod.rs` (`from_arch`, `all()`, `Display`, `define()`)
- `meta/src/isle.rs` ISLE compilation entry (`isle_ppc64.rs`)
- `ppc64 = []` feature in `cranelift/codegen/Cargo.toml` + `all-native-arch`,
  and downstream feature forwards: `cranelift/Cargo.toml`,
  `crates/cranelift/Cargo.toml`, `crates/wasmtime/Cargo.toml`,
  `fuzz/Cargo.toml:20`
- `lookup()` arm + `ALL_ARCHITECTURES` in `cranelift/codegen/src/isa/mod.rs`

Skeleton backend under `cranelift/codegen/src/isa/ppc64/`: `mod.rs`,
`settings.rs`, `abi.rs` (frame layout, `MachineEnv`, prologue/epilogue),
`inst/{mod,regs,encode,emit}.rs`, minimal `inst.isle`/`lower.isle` covering
iconst/iadd/isub/logic/load/store/branch/call/return.

Exit criterion: `clif-util compile --target powerpc64le` works and a first
`filetests/isa/ppc64/` compile test passes. Trap-everything-else keeps scope
sane.

### Phase 2 — Full scalar ISA (the long haul, ≈2–4 months) — *batch 1 done*

**Batch 1 (2026-08-17, Opus):** shifts, bit counting, division/remainder,
selects, unary ops, `addi` immediates, `trapz`/`trapnz`. 10 blessed
filetests total.

Notable decisions, all verified in the disassembly:

- **Shift amounts are masked by the lowering rules**, not the hardware: PPC
  consumes 6 bits (word forms) / 7 bits (doubleword) and yields zero past
  the width, whereas CLIF masks to the type width. Constant amounts are
  masked at compile time (`ishl x, 70` → `sld` by 6). Sub-word right shifts
  extend the input first, since bits above the type width may be garbage.
- **Division expands at emit time with explicit checks.** PPC leaves the
  result *undefined* rather than trapping on a zero divisor or on signed
  `INT_MIN / -1`, so both are branched around. The `-1` divisor is
  special-cased rather than checked-then-divided, because it is the only
  divisor whose hardware result is unusable: `x / -1` is `-x` and `x % -1`
  is `0` for every `x`.
- **ISA 3.0 gating is live and tested**: `cnttz{w,d}` and `mods{w,d}` /
  `modu{w,d}` under `power9`, with POWER8 fallbacks — remainder as
  divide-multiply-subtract, and trailing zeros as `popcnt((x-1) & ~x)`
  (which yields the full width for a zero input; the i32 form first forces
  bit 32 set so a zero low word yields 32). Separate `-power9.clif`
  filetests pin both paths.
- The modulo instructions are **X-form** (10-bit XO), not XO-form like the
  divides — caught by the `xo < 512` assert in `enc_xo`.

All encodings cross-checked byte-for-byte against `llvm-mc` on the POWER9
host (`ssh power9 'llvm-mc -triple=powerpc64le-unknown-linux-gnu
-show-encoding'` — note `/tmp` there is a full tmpfs, so pipe via stdin).

**Batch 2 (2026-08-17, Opus): scalar floating point.** 13 blessed filetests
total.

- FP arithmetic (`fadd`/`fsub`/`fmul`/`fdiv`/`fneg`/`fabs`/`sqrt`/`fma`/
  `fcopysign`), `fpromote`/`fdemote`, f32/f64 constants, f64↔i64 bitcast,
  and `fcvt_from_sint`/`fcvt_from_uint`.
- **f32 values live in FPRs already widened to double**, since `lfs`
  converts on load and `stfs` converts back. So `fpromote` is a plain
  `fmr`, `fdemote` is `frsp`, and an f32 constant is materialized as the
  bit pattern of the *widened* value. This is also why f32↔i32 bitcast is
  *not* implemented: it would mean re-narrowing the register value.
- Single-precision results use the `s`-suffixed opcodes (`fadds`,
  `fmuls`, `fsqrts`, `fcfids`) — same encoding but primary opcode 59
  instead of 63, which is what rounds each result to single precision.
- Operand-order traps, both verified in the disassembly: `fmul` takes its
  second operand in **FRC** rather than FRB, and `fmadd` computes
  **FRA × FRC + FRB**, so CLIF's `fma x y z` maps to FRA=x, FRC=y, FRB=z.
  `fcpsgn` takes the sign from FRA and the magnitude from FRB.
- **`fcmp` maps FloatCC onto cr0's four mutually exclusive bits** (LT, GT,
  EQ, UN). Eight of the fourteen conditions test one bit directly (four
  plain, four as its complement); the other six combine two bits with a
  `cror` whose destination deliberately overwrites a now-dead input. The
  three-result conditions are expressed as the complement of the fourth
  bit rather than two `cror`s. Inverting a branch only flips the `bc`
  polarity — correct even for the combined conditions, since the `cror`
  has already reduced them to a single bit.
- No FP immediate form exists, so constants go through a GPR and
  `mtvsrd`/`mfvsrd` (VSR 0-31 alias the FPRs, so the extension bit is
  always zero).
- All 18 new FP encodings cross-checked byte-for-byte against `llvm-mc`,
  and pinned in a second golden test.

**Batch 3 and beyond:** float→int conversions (`fcvt_to_sint`/`_uint` need
NaN and range checks since PPC saturates instead of trapping; the `_sat`
variants need only a NaN→0 fixup), `fmin`/`fmax` (NaN semantics; the
SpiderMonkey patch's `xsminjdp` notes apply), i128, atomics
(`lwarx`/`stwcx.` + fences), rotates, `bitrev`/`bswap`, `iadd_overflow` and
friends, `bmask`/`bitselect`, f32↔i32 bitcast, immediate forms for logicals
and shifts, `emit_tests.rs`, and a `cmpdi`-against-zero peephole (a
redundant `li rX, 0` is currently materialized for compare-with-zero).

- Complete integer including i128 (s390x's fuzzgen exclusion list shows the
  pain points), FP (FPSCR rounding; fcmp unordered via CR fields —
  condition codes live in 4-bit CR fields, so `icmp`/`fcmp` + `brif` fusion
  is a core ISLE design task), conversions.
- Atomics: `lwarx/stwcx.` LL/SC loops plus `lwsync`/`sync` fences — the
  patch's atomicity fixes are a checklist.
- Bitops (POWER8 ctz emulation trap noted in the patch).
- `Reloc` plumbing through cranelift-object/cranelift-jit; unwind info.
- `emit_tests.rs` golden encodings (s390x-level rigour; validate against the
  patch's opcode table and `llvm-mc`).
- Grow `filetests/isa/ppc64/` toward the 100–240 file range of
  s390x/riscv64.

### Phase 3 — Wasmtime runtime enablement — *mechanical half done*

**Done (2026-08-18, Opus):** the parts that are templated by existing
arches. All inert until `build.rs` is flipped (see below), so nothing
changes on other hosts.

- `crates/environ/src/compile/mod.rs` — `Powerpc64le` →
  `object::Architecture::PowerPc64`, and `page_size_align()` = 64 KiB.
- `cranelift/native/src/lib.rs` + `Cargo.toml` — `AT_HWCAP2` probe for
  `ARCH_3_00`/`ARCH_3_1`, enabling `has_isa_3_0`/`has_isa_3_1`. The libc
  dependency is now shared with riscv64's target gate.
- `crates/wasmtime/src/config.rs::detect_host_feature` — the same probe
  again (this logic is duplicated three times in tree by design).
- `crates/wasmtime/src/engine.rs` — `has_isa_3_0`/`has_isa_3_1` added to
  the flag→feature map. Without this every `Module::new` on a ppc64le host
  would fail with "don't know how to test for target-specific flag".
  Deliberately *not* following riscv64's `Some(true)`-for-everything
  shortcut, which would let POWER9 code load on a POWER8 host.
- `crates/jit-icache-coherence/src/libc.rs` — the
  `dcbst`/`sync`/`icbi`/`sync`/`isync` sequence. PowerPC's instruction and
  data caches are not coherent, so this is a correctness requirement, not
  an optimisation.

**Bug found by cross-compiling, which host builds could not see:**
`Lower::increment_lowered_uses` in `machinst/lower.rs` is gated on a list
of backend features that did not include `ppc64`, so a **ppc64-only**
build — exactly the `host-arch` configuration a native ppc64le build uses
— failed to compile. Fixed. Host builds passed only because `all-arch` or
`arm64` kept the method alive. Lesson: `cargo check --target
powerpc64le-unknown-linux-gnu` is the check that matters for anything
`cfg`-gated; `rustup target add powerpc64le-unknown-linux-gnu` is enough
(no C toolchain needed) as long as the `cache` and `debug-builtins`
features are off, since those pull in C code.

**Done (2026-08-18, Fable 5): Phase 3 complete and validated on real
POWER9 hardware.** JIT-compiled WebAssembly executes natively:

```
host callback: 78            (wasm→host array trampoline, r12 discipline)
arith(6,7) = 89              (integer lowerings incl. division expansion)
float(3,4) = 4.58257569...   (√21: FP lowerings, FPR ABI)
trap_div: IntegerDivisionByZero   (SIGILL → handler → trap-code lookup)
oob: MemoryOutOfBounds       (guard page → SIGSEGV → handler)
ALL SMOKE TESTS PASSED on powerpc64
```

plus **8/8 `wasmtime-internal-fiber` tests passing natively** — the
hand-written stack switch works. Trap recovery working means the frame
record, the unwinder offsets and the signal arms all agree.

What landed:

- `crates/unwinder/src/arch/ppc64.rs` + both `cfg_select!` lists. FP chain
  matches the backend's record exactly (old FP at +0, RA at +8, caller SP
  = FP+16). `resume_to_exception_handler` moves into r1/r31 from ordinary
  registers (asm! cannot name them as operands) and jumps via CTR with
  payloads in r3/r4.
- `signals.rs` arms: PC = `gp_regs[32]` (NIP), FP = `gp_regs[31]`, per the
  kernel `pt_regs` layout embedded in glibc's `mcontext_t`. No PC
  correction needed (the kernel points NIP *at* the faulting word).
- `crates/fiber/src/stackswitch/ppc64.rs`: saves LR, CR, r14–r31, f14–f31
  **and v20–v31** (callee-saved per ELFv2 and freely used by LLVM-
  vectorised host code — omitting them would be a silent corruption bug).
  Vector save/restore via `stxvd2x`/`lxvd2x` with the offset in r0 (the
  POWER8 baseline has no D-form VSX memory ops); the LE doubleword swap
  cancels over a round trip. The start trampoline establishes a 32-byte
  ELFv2 minimum frame before calling the entry point — ELFv2 callees
  store LR into the *caller's* frame at r1+16, which from the raw fiber
  top would land out of bounds. Indirect calls set r12 (global-entry TOC
  derivation). CFI walks from the fiber stack back to the original thread
  stack via a double-deref CFA expression through the stdu back-chain.
- `build.rs` flip: `"powerpc64"` in `has_host_compiler_backend` and
  `has_builtin_stackswitch`.

**Found by hardware testing (the reason smoke tests exist):**

1. **The trap opcode had to change.** `tw 31,0,0` raises SIGTRAP, which
   Wasmtime's handler does not register. Now the all-zeros word
   (permanently invalid per the ISA) → SIGILL, following riscv64.
2. **`try_call` lowering rules were load-bearing**, not optional: Wasmtime
   compiles wasm calls as `try_call` for exception-based unwinding. The
   emission side was already in place from Phase 1; only the ISLE branch
   rules were missing.
3. **`get_exception_handler_address` needed a new `LabelAddress` MInst**
   and a `PCRelHiLo` label-use kind: `bcl 20,31,$+4; mflr; addis; addi`
   with the hi/lo pair patched at label resolution (±2 GiB) — the
   pre-POWER10 "no PC-relative addressing" problem again, this time for
   label addresses where the constant-island trick does not apply.
4. `get_stack_pointer` / `get_frame_pointer` / `get_return_address`
   lowerings (MovFromPReg + a load from [FP+8]).

The 128-byte icache-block assumption was confirmed on hardware:
`AT_DCACHEBSIZE=128 AT_ICACHEBSIZE=128`.

Remote workflow notes: tree rsynced to `power9:~/wasmtime-ppc64` (101 MB
without target/), smoke crate at `power9:~/smoke` (standalone, path dep on
the synced tree — avoids the workspace dev-deps that need wasm32 targets).
`TMPDIR=~/tmp` required everywhere (system /tmp is a full tmpfs).

#### Reference: the full site list

- `crates/wasmtime/build.rs` — add `"powerpc64"` to
  `has_host_compiler_backend` and `has_builtin_stackswitch` (flips 32 `cfg`
  sites and disables the Pulley default).
- `crates/unwinder/src/arch/ppc64.rs` (~50 lines: `get_stack_pointer`,
  FP-chain offsets, `resume_to_exception_handler`) + its two `cfg_select!`
  lists in `arch/mod.rs`.
- `crates/wasmtime/src/runtime/vm/sys/unix/signals.rs` — two
  `(linux, powerpc64)` arms extracting PC/FP from `ucontext_t`
  (SpiderMonkey's `WasmSignalHandlers.cpp` diff shows the field names).
- `crates/fiber/src/stackswitch/ppc64.rs` — inline-asm
  `wasmtime_fiber_init`/`wasmtime_fiber_switch` with CFI, modelled on
  `s390x.rs`/`riscv64.rs`.
- `crates/jit-icache-coherence/src/libc.rs` — the
  `dcbst/sync/icbi/sync/isync` sequence.
- Feature detection in **three** places: `cranelift/native/src/lib.rs`
  (AT_HWCAP2 via libc, like riscv64's module),
  `crates/wasmtime/src/config.rs::detect_host_feature`, and the
  flag→feature map in `crates/wasmtime/src/engine.rs` (miss this and every
  `Module::new` errors on ppc64le hosts).
- `crates/environ/src/compile/mod.rs` — `object::Architecture::PowerPc64`
  mapping and `page_size_align()` = 64 KiB (ppc64le Linux default page size;
  aarch64 precedent).
- Trampolines need **no asm** — they are Cranelift-generated; correctness of
  the `Tail` convention is the actual requirement.

### Phase 4 — Test/CI/fuzz enablement (≈2–4 weeks)

- `target ppc64` lines across ~340 of the 392 shared
  `filetests/runtests/*.clif`.
- `is_isa_compatible` in `cranelift/filetests/src/test_run.rs` +
  `function_runner.rs` arch lists.
- `crates/test-util/src/wast.rs::supports_host` for the spec suite;
  `tests/all/defaults.rs`, `tests/all/module.rs`.
- Fuzzgen arch arms: `generate_flags`, `function_generator.rs` exclusion
  list (expect to accumulate known-issue exclusions like s390x did),
  `cranelift_arbitrary.rs`, `target_isa_extras.rs`;
  `crates/fuzzing/Cargo.toml` target cfg.
- `ci/build-test-matrix.js` QEMU entry (`qemu_target: "ppc64le-linux-user"`,
  `gcc_package: "gcc-powerpc64le-linux-gnu"`, `isa: "ppc64"` — the key must
  match the source directory name); swap the `platform_checks` canary to
  loongarch64; `ci/docker/ppc64le-linux/Dockerfile` for release artifacts
  later.
- Docs: `docs/stability-tiers.md`, `docs/stability-platform-support.md`;
  capstone tables in `src/disas.rs`, `crates/explorer/src/lib.rs`,
  `src/commands/hot_blocks.rs`.
- Budget time for QEMU flakiness — s390x needed a dedicated
  `is_buggy_s390x_qemu_emulation()` signals workaround; assume ppc64le under
  qemu-user needs similar care. Real POWER hardware access (e.g. OSUOSL
  POWER dev cloud) matters for anything signals/cache related that QEMU
  hides.

### Phase 4 — status (2026-08-18)

**Done:** 70 shared runtests execute on ppc64le and all 1295 filetests pass
on POWER9 (see the Phase 2 notes for the lowering fixes this drove). CI has
a qemu-ppc64le job; `powerpc64le` is no longer the "no backend" canary
(loongarch64 took over). `cranelift-object`/`cranelift-jit` handle
`Ppc64Call`; disassembly works in `objdump`/`explore`/`hot-blocks`. Docs
updated.

**Spec suite: partially blocked.** Pointing `cargo test --test wast` at a
POWER9 host found and fixed two lowerings that every real module needs —
`br_table` and `select_spectre_guard`. Small and medium filtered subsets
now run to completion and report ordinary pass/fail (for example
`memory_copy` runs 12 tests, 8 passing).

**Open, and the next thing to solve:** the full 2442-test run dies with
SIGSEGV before the harness flushes any per-test output. Ruled out so far:

- Not configuration-specific — both the pooling and default engine subsets
  crash (674 and 1768 tests respectively).
- Not memory pressure — `WASMTIME_TEST_NO_HOG_MEMORY=1`, which is what CI
  sets for the emulated targets, makes no difference.
- Scale-dependent: subsets of roughly a dozen tests are reliably fine.

One earlier single-threaded run failed differently, with
`libcalls::raw::raise` reaching `panic_cannot_unwind` and aborting. That
points at the trap-and-unwind path rather than at codegen: something inside
the libcall panics, and the panic cannot cross the `extern "C"` boundary.
The most likely candidates are the frame walk in `crates/unwinder` (a bad
`[FP+8]` read, or the `assert_fp_is_aligned` check) or the interaction
between a libcall-raised trap and the frame record. Worth reproducing under
`gdb` on the POWER9 box, and worth checking whether `fixed_frame_storage_size`
can leave SP 16-byte misaligned, since `gen_clobber_save` subtracts it
without re-aligning.

This is debugging in the runtime/unwind interaction, so it belongs with
Fable 5 per §6.

**Also still outstanding:** `ALL_ARCHITECTURES` plus fuzzgen exclusions
(deliberately deferred — with i128, SIMD and atomics unimplemented a
partial exclusion list would imply coverage that does not exist), the
per-proposal support table in `docs/stability-tiers.md` (needs a clean spec
suite run to fill in honestly), and `ci/build-build-matrix.js` plus a
`ci/docker/ppc64le-linux/Dockerfile` for release artifacts.

### Phase 4 — SIGSEGV root cause and spec-suite results (2026-08-18, Fable 5)

The core-dump analysis found frame #2 of the crash was two instruction
words (`mflr r0; blr`) where a return address belonged, and the wider
disassembly showed a spilled vmctx at `[SP+0]` clobbered with a return
address **two calls up**. Root cause: **ELFv2 native callees own a
32-byte header at the bottom of the caller's frame** (back-chain, CR, LR,
TOC saves) and write it unconditionally — our frames only reserved it
when stack args existed. Second, related ABI bug: the **parameter save
area is positional** (every param owns a doubleword slot from SP+32, and
ints after floats skip the float's GPR position); we packed densely,
which broke every native call with >8 slots — e.g. the component model's
13-arg `prepare_call`, whose garbage `storage` pointer was the
`panic_cannot_unwind` abort. Both fixed (`compute_arg_locs` positional
scheme for non-Tail; Tail stays dense; probestack gets a scratch header).

Also debugged along the way: the full-suite runs were being SIGKILLed by
the OOM killer (~42 GB anon RSS — likely 64 KiB-page amplification of
page-granular touches, 16× x86) — masked as mystery crashes. Chunked
sequential runs with `--test-threads=4` keep each process bounded; the
fuzzer VM (`debian12-ppc64`, qemu:///system) was shut down with the
user's permission, and `/tmp` (RAM tmpfs) holds ~26 GB of fuzzer
artifacts worth clearing for future runs.

New lowerings from this session: `fcvt_to_{s,u}int{,_sat}` (exclusive
bounds from `wasmtime_core::math`, NaN→trap or →0), `fmin`/`fmax`
(xsmindp/xsmaxdp + fadd NaN path), `uadd_overflow_trap`,
`smin`/`smax`/`umin`/`umax`.

**Spec suite on POWER9: 1808 passed / 146 failed**, every failure
attributed: ~106 SIMD-family, ~8 atomics, ~6 tail calls, ~2 i128 (plus
GC-collector echoes of the same). Component-model: **418/418**. Pulley
(control group, not our backend): 2 failures in `conversions`/
`simd_conversions` — worth reporting upstream separately.

Remaining before the per-proposal docs table can be filled: decide
whether to gate SIMD/threads/tail-call proposals off in
`compiler_panicking_wasm_features` for ppc64 (cleaner UX than compile
errors), then a final clean suite run.

### Gap-closure session (2026-08-19, Fable 5)

**Spec suite on POWER9: 1954 passed, 0 failed** — every configuration
(default, pooling, Null/DRC/Copying collectors, component-model 418/418).

Implemented: tail calls (teardown mirrors riscv64; unlinked `b`/`bctr`;
r12-pinned indirect target), atomics (larx/stcx. loops, POWER8 sub-word
reservation forms, sync + ctrl/isync fencing per LLVM), rotates
(rotlw/rldcl, hardware-masked amounts, rotr = rotl of negated amount),
smin/smax/umin/umax, and gating of SIMD/relaxed-SIMD/wide-arithmetic in
`compiler_panicking_wasm_features` + the wast harness (with a 4-file
skip list for tests that use v128 unconditionally).

**Miscompilation found by the suite:** `fpromote` of a signalling NaN
passed the sNaN through (lfs/fmr preserve the quiet bit; x86's cvtss2sd
quiets). Fixed by multiplying by 1.0 — exact, sign-of-zero-preserving,
NaN-quieting. Worth a runtest when the f32-widened representation is
next revisited.

Remaining feature gaps (all declared, none failing): SIMD (Phase 5),
i128/wide-arithmetic, f16/f128. Docs proposal table can now be filled
from a truthful baseline.

### Phase 4 — complete (2026-08-19)

Everything in Phase 4 is now done:

- Runtests: 70 shared runtests enabled and executing on ppc64le.
- Spec suite: **1954/1954** on POWER9 across default, pooling and all
  three GC collectors; component-model 418/418.
- CI: qemu-ppc64le test job (`isa: "ppc64"`, filter `linux-ppc64le`);
  ppc64 added to the no_std codegen check; loongarch64 took over as the
  "no Cranelift backend" canary; release-artifact build + Dockerfile.
- Fuzzing: `ALL_ARCHITECTURES` now lists `powerpc64le`, so the
  `cranelift-icache` target fuzzes ppc64le from any host. fuzzgen knows
  there are no vector lowerings and has an op exclusion list.
  **Caveat:** i128 still enters through generated *signatures*, which an
  op-level list cannot filter. The icache target discards compile errors
  (`Err(_) => return`), so this lowers fuzz yield rather than causing
  failures, and it disappears when i128 lands. A synthetic harness run
  against `FuzzGen` confirmed this is the only leak path reaching the
  backend.
- Docs: per-proposal table filled from the measured run, not estimates;
  tier entry now lacks only a full-time maintainer.

**Phase 4 done. Remaining declared gaps:** SIMD (Phase 5), i128 /
wide-arithmetic, f16/f128, and `stack-switching` (x86_64-unix only
upstream). Phase 7 (big-endian) unchanged.

### i128 / wide-arithmetic — complete (2026-08-19)

An `i128` lives in a GPR pair, low doubleword first. Design notes and
post-mortems:

- **ELFv2 placement is dense, not aligned.** The first design aligned
  `__int128` to an even doubleword slot the way AAPCS64 does; checking
  GCC and Clang on the POWER9 box showed ELFv2 does no such thing: the
  pair takes the next two slots wherever they fall, and may straddle
  r10 and the parameter save area (7 longs + `__int128` puts the low
  half in r10 and the high half at SP+96). Deleting the alignment
  special case made `compute_arg_locs` handle pairs with no extra code
  at all — each half allocates independently. Verify-against-native
  before blessing, always.
- **Carry chains are safe as separate MInsts.** `addc`/`adde` and
  `subc`/`sube` are emitted as ordinary AluRRR instructions, not a
  fused pseudo: nothing the register allocator or MachBuffer can
  insert between them (mr/spills/`addi`/islands) touches XER.CA, and
  no other instruction the backend emits writes CA. Rules extract all
  operand registers before emitting the first half so the pair stays
  adjacent.
- **Shifts use the 7-bit shift-amount saturation.** `sld`/`srd`/`srad`
  produce zero (or sign fill) for amounts 64–127, which makes the
  classic branchless double-register sequences valid across the whole
  0–127 range; only `sshr` needs one `isel`. Rotates compose the two
  shift helpers; `(-n) & 127` supplies the complementary amount.
- **Ordered icmp decomposes** as `hi <strict> || (hi == && lo <uns>)`
  — three `cmp_set`s and two logicals; eq/ne are an XOR/OR/compare.
- **Bugs only hardware caught** (macOS "runtests" for a cross target
  are silently *skipped*, not compiled — never trust a local PASS on a
  `test run` file): sign-extend-from-64 hit an emit-time
  `unreachable`; identity bitcasts and i8/i16 rotates had never been
  implemented; `select_spectre_guard.i128` was missing.
- Also picked up along the way: `bitselect` (all int widths), i128
  smin/smax/umin/umax, bmask stayed ≤64, identity bitcasts for every
  scalar type, and i128 shift *amounts* on narrower shifts.
- wide-arithmetic is ungated in wasmtime (config, wast harness, docs
  table now ✅); fuzzgen generates i128 except div/rem, matching other
  backends. `i64.add128/sub128/mul_wide_{s,u}` lower to exactly the
  expected instruction pairs (`addc/adde`, `subc/sube`,
  `mulld/mulhd(u)`).
- Follow-up (2026-08-19, same day): everything on the gap list except
  i128 division has now been implemented too:
  - `cls` (all widths): `clz(x ^ (x >> 63)) − 1` on the sign-extended
    value; `iabs` via compare-and-isel with a carry-chained negate for
    the i128 case; `bmask` to/from i128.
  - `bswap`: store to the ELFv2 red zone + `lhbrx`/`lwbrx`/`ldbrx`
    (the byte-reversed loads exist only in indexed form, so r0 carries
    the slot address). The Linux kernel's `USER_REDZONE_SIZE` (512)
    keeps the slot safe from signal delivery. `bitrev` = the bswap
    sequence + three SWAR swap-merge rounds; narrow widths finish
    with one right shift, which also flushes the upper garbage.
  - `ceil`/`floor`/`trunc` via `frip`/`frim`/`friz`; `nearest` via
    `xsrdpic`, which rounds by the *current* FPSCR mode — ties-to-even
    under the default Cranelift always runs with (`frin` is
    ties-away and unusable for this). `has_round()` now returns true,
    so wasm rounding ops compile natively instead of calling host
    builtins.
  - 128-bit atomics via `lqarx`/`stqcx.` pseudo-loops. Plain `lq`/
    `stq` raise alignment interrupts in LE mode before ISA 3.0, so
    even the plain atomic load uses `lqarx`. The reservation pair
    must be even:odd with the even register holding the
    most-significant doubleword; every operand is pinned to a fixed
    register (addr r3, source r4:r5, CAS replacement r6:r7, old value
    out r8:r9, computed pair r10:r11 — r11 being the always-free
    spill temp), mirroring how aarch64 pins its CAS loop. Min/max
    inside the loop: compare highs, one conditional skip to re-compare
    lows unsigned, then two `isel`s on the surviving cr0 bit.
- Still out of scope: i128 div/rem — deliberate. The only backend with
  a lowering is s390x, and only via z17 vector hardware; x64, aarch64
  and riscv64 all exclude it in fuzzgen exactly as we do. A software
  long-division loop would be upstream-divergent effort with no
  consumer (wasm never emits it).

### Phase 5 — SIMD via VSX (optional, +2–3 months)

#### Foundations laid (2026-08-20) — design decisions binding on all rules

1. **Lane order is little-endian in the register.** Wasm lane 0 lives in
   the least-significant bits; memory byte 0 at the LSB end. `bitcast`
   between `i64x2` lane 0 and a scalar is the identity, and lane `i` of
   `iNxM` sits at memory offset `i*N/8`. PPC numbers vector elements
   big-endian (element 0 = MSB), so *lane-indexed instructions* translate
   the index (`lane_count-1 - lane`); nothing else ever sees the
   difference. Memory ops enforce the layout: `lxvx`/`stxvx` on ISA 3.0,
   `lxvd2x`/`stxvd2x` + `xxswapd` on the POWER8 baseline. All forms are
   alignment-free (never `lvx`/`stvx`, which silently mask the address).
2. **v0 is the reserved vector emission scratch.** The P8 store path
   must byte-swap somewhere, and spill stores are generated where no
   temporary can be allocated. It must be a *volatile* register: the
   first draft reserved v31, but the scratch use is invisible to
   regalloc, and ELFv2 makes v31 callee-saved — a native caller keeping
   a value there across a call into JIT code would have been silently
   corrupted. v1-v19 volatile allocatable, v20-v31 callee-saved
   allocatable (now in `DEFAULT_CALLEE_SAVES`; the clobber save/restore
   loops handle 16-byte slots through the same `VecStore`/`VecLoad`).
3. **Register class stays split: `Vector` = VRs (VSR32-63) only.** VSX
   instructions reach both halves of the register file with their
   extension bits (`vsr_num()` maps Float → n, Vector → 32+n), which is
   how the scalar/vector seams cross without memory: `mtvsrd` targets a
   VR directly, `xxpermdi`/`xxspltw` read an FPR and write a VR.
4. **The f32 seam.** Scalar `f32` is held widened to double (Phase 1
   decision, unchanged); an `f32x4` lane is a raw single. Every
   crossing converts explicitly: splat = `xscvdpspn` (already the
   `CvtToSingleBits` op) then `xxspltw` of BE word 0; extract will be
   `xxspltw` + `xscvspdpn`. No rule may move f32 bits across the seam
   any other way.
5. **ELFv2 vector ABI, verified against GCC on hardware:** vector args
   in v2-v13 *by vector-parameter order* (like FPRs, independent of GPR
   exhaustion); each occupies a quadword-aligned pair of doubleword
   slots in the parameter save area and *skips the corresponding GPRs*
   — the opposite of `__int128`, which packs densely and never skips.
   Returns in v2 (v2-v3 internally).
6. **P8/P9 gating lives at emit time** inside the memory pseudos
   (identical operand shapes, `has_isa_3_0` selects the sequence), not
   in the ISLE rules. Everything else emitted so far is POWER8-clean;
   the lane extract/insert fast paths (`xxextractuw`, `mfvsrld`, ...)
   are ISA 3.0 and get the same treatment when they arrive.
7. **`vconst`** goes through the GPRs (two `LoadConst64` + two `mtvsrd`
   + `xxpermdi`), with `xxlxor` for zero. A constant-island path is a
   later optimization; measure before assuming it wins (see Phase 6:
   off-critical-path instruction count has measured zero effect).

#### Batches 1-2 (2026-08-20): compares, min/max, shifts, float lane arith

Landed: integer compares (all ten conditions, four widths), integer
min/max signed and unsigned, per-lane shifts, float lane arithmetic
(add/sub/mul/div/sqrt/neg/abs), float compares (all fourteen
`FloatCC`s), `iabs`, `avg_round`. 39 of upstream's `simd-*.clif`
runtests now execute on ppc64le.

Four bugs, every one caught by a test rather than by reading the code —
worth recording because they cluster into two lessons:

1. **`ishl`/`ushr`/`sshr` on a vector by an `i128` amount crashed the
   compiler.** CLIF lets the shift amount be any integer type, and
   upstream's `simd-ishl.clif` exercises `i128`; my own runtest only
   used `i32`, so it passed. The fix reused `shift_amt_64`, already
   written for scalar i128 shifts.
2. **The scalar `fcmp` rule had a wildcard type guard and so also
   matched *vector* compares**, feeding vector registers into a scalar
   compare. regalloc2's `left: Vector, right: Int` assertion caught it,
   but that assertion is debug-only: in release this was a silent
   miscompilation. An audit found `fcmp` was the only unguarded rule.
3. **`fcmp`'s controlling type is the *result*, not the operands** --
   `i8` for a scalar compare. So the obvious guard (`ty_scalar_float`
   on the controlling type) matched nothing and broke every scalar
   `fcmp`, including the `fcmp ne v, v` NaN checks inside `fmul` and
   `fdemote`. All fourteen rules now bind the operand type explicitly;
   the vector ones had been keying lane width off the result type,
   which was sound only because a compare's result lane width always
   equals its operand's.
4. **`Unordered` was implemented as NOR where it needed NAND.**
   Unordered means "not both self-equal"; NOR and NAND agree except
   when exactly one operand is NaN, which is what the runtest's
   `[NaN, 1.0]` case hit. `Ordered`/`Unordered` now share one helper
   and are each other's complement by construction.

The lessons: **a local `test run` PASS for a cross target means
nothing** -- macOS silently *skips* those files rather than compiling
them, so the trial-enable sweep must run on the POWER9 (an earlier
sweep "passed" 68 files locally of which only 39 actually work). And
**upstream's runtest corpus is worth more than hand-written tests** for
finding the cases one would not think to write: i128 shift amounts,
NaN-versus-normal comparison pairs.

#### Batch 3 (2026-08-20): lane reductions, and the first CR6 use

`vall_true`/`vany_true` need the record form of the equality compare,
whose CR6 summary answers both questions against a zero vector: EQ set
means no lane is zero (all non-zero), LT set means every lane is zero.
This is the first condition-register field other than cr0 the backend
touches, and it keeps the same invariant -- the zero, the record-form
compare and the `isel` are one MInst, so no CR field is ever visible to
regalloc.

The compare **must** use the value's own lane width: a 32-bit lane
holding 1 is non-zero yet contains three zero bytes, so a byte-wise
test answers `vall_true` wrongly. Naive all-ones test inputs pass
either way, so the runtest pins the hazard explicitly at each width
(e.g. `[0x01000000 0x00010000 0x00000100 0x00000001]`).

Batch 3 needed no fixes, against four in batch 2. The difference was
deriving the CR6 bit semantics and the lane-width requirement *before*
writing the emit code, and encoding the hazard into the test rather
than discovering it afterwards. 46 upstream `simd-*.clif` runtests now
execute on hardware.

#### Batch 4 (2026-08-20): lane extract/insert — the flagged trap, defused

The plan marked this the SIMD danger zone (ISA 3.0 extract/insert
instructions vs POWER8), and the resolution is worth recording because
it dissolved rather than materialised:

- **Extract needs no memory and no P8/P9 split at all.** Splat the
  wanted lane across the reserved v0 scratch (`vsplt{b,h,w}`, or one
  `xxpermdi` for doublewords), then `mfvsrd`. The replication leaves
  the lane value in the low bits with copies above -- exactly the
  garbage the narrow-value convention tolerates, and the subsequent
  `sextend`/`uextend` that wasm's `extract_lane_s/u` become read only
  the lane's bits. Two instructions, POWER8-clean. f32 lanes widen
  into the scalar double convention with `xxspltw` + `xscvspdpn`; f64
  lanes are one `xxpermdi`.
- **Insert is a red-zone round-trip** where the *scalar store* does
  all the type dispatch -- `stfs` converts the widened-double f32
  scalar to its 4-byte lane bits as a plain side effect of being a
  single-precision store. One sequence for all six lane types; only
  the vector store/load halves gate on ISA 3.0. The `vinsert*`
  register path remains a P9 optimization for later.
- The lane→BE-element translation (15-lane, 7-lane, 3-lane, 1-lane)
  exists only in emission, per the foundations convention, and the
  runtest makes translation errors unpassable: every input vector is
  asymmetric, so lane i reading element i instead of M-1-i returns a
  visibly wrong value. First hardware contact passed both paths with
  no fixes.
- Vector `bitselect` turned out to be a single `xxsel`, added in
  passing (operand order: result bit = mask ? XB : XA, so CLIF's
  (mask, x, y) maps to XA=y, XB=x).
- Triage note: the `simd-*_32.clif` / `-32` upstream runtests are for
  **32-bit pointer targets** (the verifier rejects them under a
  64-bit ISA) -- inapplicable, not gaps. 53 upstream `simd-*.clif`
  files now execute on hardware.

#### Batch 5 (2026-08-20): saturating arithmetic, popcnt, rounding

Direct instruction mappings, plus one design correction worth keeping:

- Saturating add/sub exist for b/h/w only. The first cut relied on a
  poisoned opcode table, which turned an `i64x2` saturating add into an
  **emit-time panic** rather than a clean `Unsupported`. Panics read as
  compiler bugs and kill the process; `Unsupported` is a signal callers
  can act on. The rules now carry a `vec_lanes_under_64` guard and the
  assertion is demoted to a backstop. Generalisable: an unsupported
  operation must be declined at *lowering*, never asserted at emission.
- Vector `popcnt` at all four widths; needed the single-operand VX
  form, which the unpack instructions will reuse for widening.
- Rounding: the scalar `nearest` lesson transferred directly. Plain
  `xvr{sp,dp}i` rounds ties *away from zero* and cannot implement
  `nearest`, exactly as `frin` could not; the `xvr*ic` current-mode
  forms give ties-to-even. Picked correctly first time because the
  earlier trap was written down.
- 11 more upstream runtests, including `simd-arithmetic` (broad
  coverage, blocked only on `sadd_sat`). 64 now execute on hardware.

Note: POWER9's `/tmp` is quota-exhausted; scratch files must go in
`~/tmp` there.

#### Batch 6 (2026-08-21): widening and narrowing — predicted hazard, no fixes

The one batch where the endianness trap was called in advance and the
paper derivation got it right first time. LE lane i is BE element
15-i, so `vupkhsb` (BE elements 0-7) is CLIF's `swiden_high` and
`vupklsb` is `swiden_low` -- **the names invert**. Likewise a pack's
first operand lands in the LE *high* lanes, so `snarrow(x, y)` passes
its operands swapped. Both derivations are written into the rules, not
just their conclusions, because "the names are backwards" is a claim a
reader should distrust without the argument.

This mattered because an inversion here yields *plausible* wrong
answers -- widening the wrong half still returns a well-formed vector
of the correct type. The asymmetric test values confirm the choice
rather than merely failing to contradict it.

Other notes: there are no unsigned unpack instructions, so unsigned
widening interleaves with a zero vector (pairing a lane with a
same-width zero lane *is* a zero-extension). Everything is selected by
the **source** lane width, so the impossible cases differ per family
(no doubleword widen source, no byte pack source) -- both declined at
lowering per the batch-5 rule.

19/19 upstream tests passed first contact, including the ten
`simd-i{add,sub}-*widen-*` files whose `-mix` variants pair a low
widen with a high widen in one expression. 83 upstream runtests now
execute on hardware.

#### Batches 7-8 (2026-08-21): integer multiply

Per-lane forms first (`vmladduhm` for i16x8, `vmuluwm` for i32x4,
`vmhraddshs` for `sqmul_round_sat` -- which computes
`sat((a*b + 0x4000) >> 15) + c`, exactly the Q15 op at c = 0), then
doubleword multiply built from 32-bit halves.

**The even/odd mnemonics invert too.** `vmuleub`'s result lane k takes
LE byte lanes 2k+1, so the instruction named "even" works on LE-*odd*
lanes; likewise `vmulouw` ("odd") multiplies the even LE word lanes,
which are a doubleword's *low* halves -- which is precisely why it
supplies `al*bl` for the i64x2 decomposition
`a*b == al*bl + (al*bh + ah*bl) * 2^32`. Nine vector instructions,
nothing leaving vector registers. Derivations sit beside the rules:
someone trusting the mnemonics would file bugs against correct code.

Testing note that generalises to any *composed* lowering: the runtest
must exercise each **term**, not just plausible results. A rule
computing only `al*bl` still gets `3*7` right. So the cases are 2^32
squared (all terms vanish -> 0), (2^32+1) squared (both cross products
live), all-ones squared (every term maximal, wraps to 1), and
asymmetric operands where transposing the cross products would show.

Also: `vspltisw`'s immediate only reaches 15, so a splatted 32 must
come through the scalar-to-vector path.

Deferred deliberately: `i8x16` imul (needs a `vperm` interleave of the
even/odd products, and **wasm has no `i8x16.mul`** -- it was removed
from the proposal, so this is CLIF completeness with no wasm traffic)
and the `umulhi`/`smulhi` family, which reuses the same even/odd
derivation.

#### Batch 9 (2026-08-21): high-half multiply, and the safety net firing

`smulhi`/`umulhi` at byte, halfword and word widths. Two findings:

- **i32x4 mulhi is three instructions**, not the seven budgeted. A
  doubleword's high word is its *odd* LE word lane, and `vmrgew`
  ("merge **even** word") interleaves exactly the odd LE word lanes --
  so the two even/odd products feed one merge with no shift or mask.
  Fourth family where the inversion, once derived, *helps*.
- **Shift-amount trick**: `vspltisw`'s immediate stops at 15, but the
  vector shifts read only log2(lane width) bits per element, so
  `vspltisw -16` splatted to words yields a shift of 16 in one
  instruction (vs three for the GPR round-trip). Documented on
  `VecSpltImm`; reusable for other awkward amounts.
- i64x2 mulhi is **absent, not deferred** — needs 64x64->128, which
  does not exist pre-POWER10. Distinct category from things skipped by
  choice.

**The emit assertion caught a regression I introduced.** Unifying batch
8's private `MulOddWordU` into the general `MulOddU` moved the table
index from *result* width to *source* width; batch 8's three call sites
still passed `$I64X2`. Index 3 is a poisoned zero and extended opcode 0
is `vaddubm`, so i64x2 multiply would have silently *added*. Two
lessons: (1) the batch-5 layering (rule guard primary, emit assertion
as defence-in-depth) paid off exactly as intended, three batches later;
(2) unifying op variants is riskier than it looks — the merge changed
what the type parameter *means*, an invariant nothing in the type
system encoded. Caught by running the **full** suite, not batch 9's own
tests, which passed while breaking batch 8.

#### Batch 10 (2026-08-21): pairwise addition

No horizontal-add instruction exists and none is needed: **a pair of
adjacent lanes is one lane of the next width up** (even element in the
low half, odd in the high). Shift the double-width lane down by one
element width, add at double width, and the pack discards the high half
where the carry went. Six instructions.

Semantics checked against the docs *and* the upstream test, not
assumed: the result **concatenates** (operand 1's sums = low lanes),
so it is the pack's second argument. Had it interleaved,
`vmrgew`/`vmrgow` would have looked like the obvious tools and given a
subtly wrong order.

**Limit of the splat-immediate shift trick** found here: the word-pair
case needs a doubleword shift of 32, and no 5-bit immediate is
congruent to 32 mod 64, so that one needs the GPR path.

Unlocked 4 upstream tests, 3 of which only became reachable because
earlier batches *compose*: `simd-sdot` and
`simd-wideningpairwisedotproducts` need widening (b6) + multiply (b7-8)
+ pairwise add; `simd-addv-reduce` also needs lane extract (b4).

**`vmsumshm` fusion now actionable but deliberately not done**: it
collapses the widen/multiply/pairwise-add tree to one instruction (the
`sdot` fusion aarch64 does). Phase 6 measured off-critical-path
instruction removal as worth nothing in wall-clock here, and 4-into-1
is exactly the change that looks obviously good and may measure as
nothing. Benchmark either side before committing to it; the Phase 6
harness is still on the POWER9.

Still unimplemented, blocking further upstream tests: 64-bit vector
types (`i8x8`, `i32x2`, `f32x2`), `bitcast` to `i128`, widening and
narrowing, `shuffle`/`swizzle`, saturating arithmetic, `fmin`/`fmax`
(the wasm NaN semantics need checking against `xvmin`/`xvmax`
behaviour on hardware before implementing).

Ops landed with the foundations: v128 load/store, `vconst`, all-lane
`splat` (int and float), `iadd`/`isub` at every lane width,
`band`/`bor`/`bxor`/`bnot`, vector-vector `bitcast`, plus regalloc
spill/reload, cross-call preservation and both ABI paths. Everything
else in the ~236-op grid remains: comparisons, shifts, float lane
arithmetic, min/max, lane ops, conversions, narrowing/widening,
`shuffle`/`swizzle` — the bulk-fill phase, on the cheaper model, one
family at a time with runtests per family.

~236 SIMD ops; POWER8 VSX covers most of wasm SIMD but the patch's SIMD
regression tests (extract-lane canonicalisation, extmul aliased dest,
high-lane corruption on P8 vs P9) are exactly the corner cases to encode as
runtests. Keep gated off until scalar is solid.

#### Batches 11-12 (2026-08-24): float/integer lane conversions

**NaN behaviour probed on hardware, not read from the manual** — and it
is asymmetric. `xvcvspuxws`/`xvcvdpuxds` already give zero for a NaN
lane, exactly what CLIF's saturating conversions specify, so the
unsigned rules are a bare instruction. The signed forms give the *most
negative* value instead, so those mask the result against the source
compared with itself (all ones for an ordered lane, all zeros for NaN).
Reading the saturation description and assuming symmetry would have
produced a wrong signed lowering that passes every non-NaN test.

**Both precision changes need lane gathers**, and the derivations are
the mirror of each other under the standing identity that LE lane i is
BE element (count - 1 - i):

- `xvcvdpsp` writes BE doubleword 0's result to BE word 0 and
  doubleword 1's to word 2 — in LE terms, lanes 0 and 1 land in word
  lanes 1 and 3, each in the *high* word of its doubleword. Rotate each
  doubleword by 32 to bring them down, then pack the low words against
  zero: results in lanes 0 and 1, upper two zeroed, as `fvdemote`
  requires.
- `xvcvspdp` reads from BE words 0 and 2 — the same high words. So the
  lanes to promote must be moved there first, and merging the source
  against zero does precisely that, leaving source lanes 0 and 1 at
  word lanes 1 and 3.

The batch-12 runtest uses distinct asymmetric values in every lane
specifically so a gather that picked the wrong word shows up; symmetric
test data would hide it.

Validated on the POWER9: 8 runtests pass against the interpreter, the
full filetest suite is green, and all seven encodings match `llvm-mc`
byte-for-byte. Enables six upstream conversion tests.

#### Batch 13 (2026-08-24): byte permutes

One identity carries both lowerings: **over the 32-byte concatenation
`vperm` addresses, CLIF's little-endian index k is big-endian index
31 - k**. Reflecting an index also moves it to the other half of the
concatenation, which is why the sources must be passed in the opposite
order — the operand swap is not a separate fact to remember but a
consequence of the reflection. `shuffle` is then one permute against a
constant-folded control vector.

`swizzle` needs a bounds check, and the interesting part is that **two
unrelated hazards collapse into one fix**. Indices 16..31 would select
the second permute source; indices ≥ 32 would have their high bits
dropped by the five-bit control field and alias back into range. Both
disappear if the result is masked, and masking pays for itself twice
over: the single `vcmpgtub` against 15 is both the bounds check and
(via `vspltisb 15`) the operand of the `15 - idx` subtract, and because
out-of-range lanes are cleared afterwards the permute can take the
source register twice instead of needing a zero vector. Five
instructions, verified in the disassembly as exactly five with no
spills.

Note the control bytes are `15 - idx`, not `31 - idx`: the wanted byte
now lies in the *first* source, and reflecting within a single 16-byte
half is a complement against 15. Getting this wrong is invisible to any
test whose indices are symmetric about the halfway point, so the
runtests use asymmetric data throughout.

`vandc` was added as `AndC`, emitted as the VSX `xxlandc` to match how
the other vector logicals are done here. The `vconst` materialization
was factored into a `vec_const` helper, since the permute control
vector needs the same path.

**Deliberately not used: `vpermr`** (POWER9), which is this permute with
the little-endian index convention and would remove both the reflection
and the swap. It buys nothing — the reflection is constant-folded for
`shuffle` and shares an instruction with the bounds check for `swizzle`
— and it is outside the POWER8 baseline, so it would mean two code
paths for zero gain.

Validated on the POWER9: batch runtests plus `simd-shuffle` and
`simd-swizzle` pass natively, the full filetest suite is green, and both
new encodings match `llvm-mc` byte-for-byte.

#### Batch 14 (2026-08-24): vector min/max — probe first, and it paid

Following the batch-11 lesson, `xvmin`/`xvmax` were **probed on the
POWER9 before any lowering was written** (a small C program with inline
asm over the interesting bit patterns). Result: they are right about
more than expected and wrong about exactly one thing.

Right: -0 ranks below +0, **and independently of operand order** —
stronger than IEEE minNum requires, so no operand-order fixup is
needed. Infinities correct. A *signalling* NaN is quieted and returned,
which is already an acceptable answer.

Wrong: a *quiet* NaN makes them return the **numeric** operand, where
CLIF wants a NaN. Same as the scalar `xsmindp`/`xsmaxdp`, so the fixup
took the same shape.

Two decisions inside the fixup, both load-bearing:

1. **The NaN is the sum of the operands, not a materialized constant.**
   The wasm min/max tests demand `nan:canonical` in 72 of their cases
   per op (checked in `tests/spec_testsuite/simd_f64x2.wast` rather
   than assumed), and only propagation gives a canonical NaN out for a
   canonical NaN in. A synthesized quiet NaN would have been *cheaper*
   — the all-ones unordered mask is itself a valid quiet NaN, which
   would have made the whole thing five instructions via `xxlorc` —
   and would have failed those 72 assertions. Worth recording as a case
   where the cheaper sequence is wrong for a reason no runtest of my
   own devising would have caught.
2. **The select is driven by the operands, not by the sum.** Testing
   whether the sum is a NaN looks equivalent and is not: `inf + -inf`
   is a NaN while the minimum of those operands is perfectly ordered.
   Comparing each operand with itself is false exactly where it is
   unordered; the two masks are ANDed so either NaN poisons the lane.

Six instructions, verified in the disassembly. The runtests place NaNs
in both operand positions (the hardware's wrong answer is asymmetric
that way) and compare raw bits through a bitcast, so the payload is
actually checked rather than merely NaN-ness. Enables four upstream
vector min/max tests.

#### Batch 15 (2026-08-24): 64-bit vector types

Implemented, at Trung's direction, for parity with aarch64. The
analysis that had argued against it stands on the facts and was wrong
on the priority: wasm genuinely never produces these types (the
frontend refers to `I8X16` fifty times and to any 64-bit vector type
zero times) and s390x genuinely does not support them, but parity with
the most capable backend is the goal, so they are in.

**Representation: big-endian doubleword 0**, the bit positions the
*high* lanes of the corresponding 128-bit type occupy, with the other
doubleword undefined. That half was picked because three things then
come out exactly right instead of needing fixups:

- `mfvsrd`/`mtvsrd` move precisely that doubleword, and within it lane
  0 is in the least-significant bits, so a bitcast to or from a 64-bit
  scalar is one instruction.
- A little-endian doubleword load puts memory byte j at big-endian byte
  7 - j, which is exactly where lane j lives — memory order and lane
  order agree with no permute. Worth stressing this was *derived and
  then tested*, not assumed; the mirror choice (doubleword 1) would
  have needed a permute on every load, store and bitcast.
- Lane-wise instructions do not care where the lanes sit, only how wide
  they are, and the MInst's type already says. So all the lane-wise
  rules are shared with the 128-bit ones via a `ty_vec_any` guard.

Only three kinds of operation must know:

1. **Reductions** read the meaningful doubleword into a GPR. Cheaper
   than masking the undefined half, and impossible to get subtly wrong.
   `vany_true` needs no lane compare at all — some lane is non-zero
   exactly when the doubleword is.
2. **Pairwise addition** uses the batch-10 trick (a pair of adjacent
   lanes is one lane of the next width up) but must first gather both
   operands' doublewords into one register with `xxpermdi`, because the
   pack reads whole registers. At 128 bits the two could be packed
   against each other directly.
3. **Widening** meets the big-endian naming inversion from the other
   side: since the data is in BE doubleword 0, the *High* unpack and
   merge forms are the ones that see it — for both signednesses and
   both halves. So `swiden_high` is a bare `vupkhsb` and `swiden_low`
   is that plus a doubleword swap.

**Three gaps at 128 bits surfaced only when these tests ran**, and are
fixed here too: vector `fma` (VSX has only accumulating multiply-adds,
so the addend is also the destination — tied with a regalloc
reuse-def, and the runtest deliberately keeps the addend live
afterwards to force the copy), vector `fcopysign`, and `avg_round` on
`i64x2`, which has no instruction and uses
`(a | b) - ((a ^ b) >> 1)` — chosen because no intermediate is ever
wider than a lane, so the carry that would overflow a doubleword add
never has to be represented.

**Process lesson worth keeping: the local runtest pass was
meaningless here.** On a non-ppc64 host, `test run` skips compilation
entirely when host and target differ, so sixteen test files "passed"
locally while four distinct unimplemented-op failures were waiting on
the POWER9 (`fcmp.f32x2`, `fadd.f32x2`, `vconst.i8x8`, and
`avg_round.i64x2`). The `vconst` one is the subtle one:
`u128_from_constant` requires exactly sixteen bytes and silently
returns `None` for a 64-bit constant pool entry, so the rule never
matched — a wrong-looking extractor, not a wrong instruction. For any
batch touching new *types*, the hardware run is the first real check,
not the last.

Not included: 16- and 32-bit vectors (`i8x2`, `i16x2`), which
`simd-small.clif` also needs, so that file stays disabled. Extending
the same scheme downward would mean either a shift on every scalar
bitcast or a second placement convention; worth doing only if
something actually wants those types.

**Follow-up still open:** `supports_simd` for `Powerpc64le` in
`cranelift/fuzzgen/src/target_isa_extras.rs` is still `false` from when
there were no vector lowerings. fuzzgen's type pool holds only 128-bit
vectors, so flipping it will not reach the 16/32-bit gap above. Worth
doing once the v128 grid is complete.

#### Batch 16 (2026-08-24): aarch64 parity sweep

Rather than guess at what was missing, every runtest that aarch64 runs
and this backend did not (113 files) was enabled at once and the POWER9
asked what broke. That turned a vague "what's left?" into a bounded
list of nine operations — and found a bug that no amount of reading
would have.

**A miscompilation in already-shipped code.** `smulhi`/`umulhi` on a
type narrower than a word extended both operands and took the high half
of a *32-bit* product, but the wanted half is the high half of *that
type's* width. `umulhi.i8(255, 255)` is `0xFE01 >> 8` = 254; taking a
word's high half gives 0. It compiled cleanly and returned wrong
answers. Narrow cases now form the whole product in a doubleword — two
extended 16-bit values cannot overflow one — and shift down by the
type's width. **Enabling other backends' tests is a bug-finding
technique, not just a coverage exercise.**

**A second latent bug, same shape.** The new overflow rules first used
`put_in_ext_reg`, which deliberately does *not* extend an `i32` because
the word-wide instructions it feeds read the low word in full. These
checks compare whole doublewords, so they read the register's undefined
upper half and reported overflow at random — visibly for the signed
cases, and only by luck of the garbage being zero for the unsigned
ones. Helpers that promise less than their name suggests are worth
reading before reuse; that one is now gone, having no other callers.

Operations added: `bitcast` between `i128` and a vector; vector `ineg`,
`select` and `scalar_to_vector`; `imul` on `i8x16`, `i16x4` and
`i32x2`; all six overflow-detecting arithmetic ops at every width, with
128-bit for the adds and subtracts; sub-word float-to-integer
conversions; and `vhigh_bits`.

Three of those are worth remembering:

- **`imul.i8x16`** has no instruction. The even/odd forms give halfword
  products of alternating lanes and each wanted byte is the low byte of
  one, but they must be *interleaved* — a truncating pack concatenates
  the even lanes' bytes and then the odd lanes', a de-interleaved
  order, which was the first version's bug.
- **Overflow flags recompute rather than read XER.** Reading the carry
  and overflow bits back costs about what recomputing the condition
  costs, and recomputation composes with the existing
  compare-into-a-GPR machinery. Only the 128-bit carry chain reads XER,
  via `addze` of zero, where there is no cheap alternative.
- **`vhigh_bits` is one `vbpermq`.** It gathers sixteen arbitrary bits
  named by an index vector; the instruction sends the bit chosen by
  index byte `i` to result weight `2^(15-i)`, so lane `j` is named by
  byte `15-j`, and a lane's sign bit sits at big-endian bit
  `w*(n-1-j)`. Bytes with no lane to name are set above 127, which the
  instruction gathers as zero.

**Result: 346 runtests enabled for ppc64le against aarch64's 335.** The
24 files still aarch64-only are 13 other architectures' own regression
tests (`*-aarch64.clif`, `x64-bmi*`, `riscv64-vstate`, the Apple ABI
one) and 11 needing one of four declared type gaps: **f16**, **f128**,
**dynamic vector types**, and **sub-64-bit vectors** (`i8x2`, `i16x2`).
There are no remaining *operation* gaps against aarch64.

#### Batch 17 (2026-08-25): fuzzing enabled, and what it found

`supports_simd` is now `true` for this target and the exclusion list has
been rewritten; it had been written when there were no vector lowerings
at all and was almost entirely stale.

**The list is a correctness requirement, not a yield knob.**
`cranelift-fuzzgen` calls `compile().unwrap()`, so any operation fuzzgen
can generate but the backend cannot lower is a *panic* — an exclusion
list that is merely approximately right produces a fuzz target that
reports spurious crashes forever.

So rather than reason from the rules, every (opcode, control type,
argument type) combination fuzzgen's own `OPCODE_SIGNATURES` table can
produce was enumerated and compiled — **1076 of them** — and each
failure then compiled for **aarch64** to distinguish a real gap from a
combination fuzzgen filters globally (`valid_for_target`'s opening
section rejects a lot: mismatched conversion shapes, unequal-width
bitcasts, `StackSwitch`). That comparison is what makes the result
trustworthy; without it, `blendv`, `x86_cvtt2dq` and `bitselect.f32`
look like ppc64 gaps when in fact no backend takes them.

**A methodological miss worth recording.** The first pass skipped
opcodes with *free* argument types, which quietly excluded the entire
shift and rotate family — the amount type is free. A dumb loop over
fuzzgen internals then hit `rotl.i16x8` on its very first productive
iteration. Enumerating the cartesian product over free positions, not
just the bound ones, is what a complete sweep requires.

Four operations aarch64 supports were missing, and all four were
**implemented rather than excluded**:

- **Vector `rotl`/`rotr` at every lane width.** `vrl{b,h,w,d}` mask the
  amount to the lane width, which is CLIF's rule exactly, so no masking
  is needed and negation gives rotate-right for free. The
  doubleword-only rotate already present for the i64x2 multiply
  generalized to all four widths — including deleting an
  `assert_eq!(lane, 64)` that had pinned it there.
- **The extending 64-bit vector loads** (`{u,s}load{8x8,16x4,32x2}`).
  The loaded doubleword lands in big-endian doubleword 0, exactly where
  the *High* unpack and merge forms read from, so these are batch 15's
  widening rules with a load in front and no permute at all.
- **`select_spectre_guard` on vectors and scalar floats**, mirroring the
  `select` rules.
- **`select` on a vector with a 128-bit condition**, folding the halves
  together before building the mask.

What stays excluded is what the hardware genuinely lacks — doubleword
saturating arithmetic, doubleword high-half multiply before POWER10, the
word-wide rounding multiply — **each individually verified to compile on
aarch64**, so each is a real ISA difference rather than an oversight,
plus the 128-bit division/remainder/high-multiply/float conversions no
backend lowers.

Phase 5 SIMD is now complete for the v128 grid, with fuzz coverage.

#### Vector type coverage, and an f16 miscompilation (2026-08-25)

Auditing *which vector types the backend accepts* — as opposed to which
operations — turned up a silent miscompilation.

`rc_for_type` accepted a vector by its **total width**, so `f16x8` (128
bits) and `f16x4` (64 bits) passed. Nothing downstream re-checks the
lane type, and `vec_fpu_rrr` picks single- or double-precision by asking
whether lanes are 32 bits wide, defaulting to double otherwise. So
`fadd.f16x8` emitted **`xvadddp`** — a double-precision add over two
64-bit lanes — for a value that is eight half-precision numbers. Wrong
format, wrong lane count, no diagnostic.

Nothing was generating these: fuzzgen's type pool has no f16 vectors,
and wasm has no f16 at all. The bug was reachable only by a Cranelift
frontend that uses them, which is precisely the kind of gap that a
wasm-shaped test suite cannot find.

Fixed in `rc_for_type` — the one place that sees every value regardless
of which rule lowers it. `simd-vconst-f16.clif`, enabled by the parity
sweep, had passed only because `vconst` is pure bit movement that never
consults the lane type; it is disabled with the other f16 tests, since
"constructible and copyable but miscompiles on contact with arithmetic"
is worse than unsupported.

**Where vector type support now stands** (CLIF vector widths are 16 to
512 bits):

| Width | Types | Status |
|---|---|---|
| 16-bit | `i8x2` | not supported |
| 32-bit | `i8x4`, `i16x2` | not supported |
| **64-bit** | `i8x8`, `i16x4`, `i32x2`, `f32x2` | **supported** (batch 15) |
| **128-bit** | `i8x16`, `i16x8`, `i32x4`, `i64x2`, `f32x4`, `f64x2` | **supported** |
| 256/512-bit | `i8x32`, `i32x8`, `f64x4`, … | not supported |
| any width | `f16xN`, `f128xN` | not supported (see above) |
| dynamic | `i8x16xN`, … | not supported |

The 128-bit row is the whole of wasm SIMD, and the 64-bit row is what
aarch64 parity required. Everything else fails with a clean
`Unsupported` error rather than miscompiling — the 256- and 512-bit
types would need multi-register values, and the dynamic types a whole
scaling mechanism, neither of which any current consumer wants.

#### Batches 18-20 (2026-08-25): completing 64/128-bit, and a bug sweep

A deliberate hunt for latent bugs and missing lowerings across both
supported vector widths, rather than following upstream tests. It found
**three real defects** and closed the 64-bit grid.

**The sweep had a blind spot that mattered.** Batch 17's exhaustive
enumeration used *fuzzgen's* type pool, which is 128-bit only — so the
64-bit types were never swept at all, and batch 15 had implemented only
what the upstream tests happened to need. Re-running over every
(opcode, control type, argument type) combination across both widths
found **87 shape-valid pairs that did not compile**. Lesson: an
"exhaustive" sweep is only exhaustive over the set you enumerated;
state that set explicitly.

Most were type guards, not missing work: 35 lane-wise rules (shifts,
rotates, min/max, saturating, `popcnt`, `iabs`, `bitselect`, `icmp`,
float lane arithmetic) now take either width, since the instruction
works on whole registers and cares only about lane width. The rest had
to know the data sits in big-endian doubleword 0:

- `imul.i8x8` reuses the 128-bit interleave unchanged.
- `smulhi`/`umulhi` narrow need **one merge fewer** than at 128 bits:
  only four products exist, all in the high big-endian elements, so a
  single high merge interleaves them and fills all eight lanes.
- The narrowings gather both operands into one register before packing
  (`y` above `x`), because the pack fills doubleword 0 from its first
  operand's eight lanes and each source supplies only four.
- `extractlane`/`insertlane` restate the index as `lane + n`, which
  makes the 128-bit path's own big-endian subtraction land on the right
  element — reusing the arithmetic instead of duplicating it.

**Defect 1 (miscompilation): `f16` vector lanes.** Covered in the entry
above; `rc_for_type` accepted vectors by total width, so `fadd.f16x8`
became `xvadddp`. Also hardened the four float emitters that chose
precision by `lane_bits() == 32` with a silent default to double — that
default *was* the bug. The enumerated dispatches already used
`unreachable!`, which is the pattern to prefer.

**Defect 2 (missing lowering): lane access on 64-bit vectors** — and it
had been **passing locally**. With host and target different, `test run`
skips compilation entirely, so the file was only ever interpreted. It
failed the instant the same file ran on the POWER9. This is the second
time this trap has cost time (see batch 15); for anything type-related
the hardware run is the *first* real check.

**Defect 3 (miscompilation, earlier): narrow `smulhi`/`umulhi`** — see
the parity-sweep entry. Same shape as defect 1: a value reaching an
emitter whose type dispatch could not represent it.

**The common thread across all three**: a lowering rule whose type guard
is *looser* than the emitter it feeds. Worth checking guard and emitter
together whenever either changes.

**Differential testing** now covers the lowerings whose derivation is
non-obvious and which no upstream runtest reaches here: every lane of
every integer vector type both directions (88 cases), `avg_round.i64x2`
at the point where `a + b + 1` overflows a doubleword (the whole reason
that lowering uses `(a|b) - ((a^b)>>1)`), `iadd_pairwise`, the
widenings from `i32x4`, `imul` on `i64x2`/`i8x16`, `smulhi`/`umulhi` at
every width, and the narrowings. Expectations are computed from CLIF's
semantics independently of the lowering, so the interpreter validates
the expectations and the POWER9 validates the code against them — the
oracle is never the thing under test.

**State: the 64-bit and 128-bit grids are complete.** Every shape-valid
combination compiles except what the hardware genuinely lacks
(doubleword saturating arithmetic, doubleword high-half multiply before
POWER10, word-wide rounding multiply — each verified to compile on
aarch64, so each a real ISA difference), and the 128-bit
division/remainder/high-multiply/float conversions no backend lowers.

### Phase 6 — Tuning

POWER9/10 fast paths (`isel`, `setb`, mod instructions, P10 pcrel to kill
constant-materialisation sequences), egraph-visible lowering improvements,
benchmarking vs Pulley (the backend must beat the interpreter convincingly
to justify itself).

#### Measurement first (2026-08-19)

Benchmarked native ppc64le against Pulley on the POWER9 with all VMs shut
down, five workloads, AOT-compiled so compile time is excluded, the 5 ms
process-startup floor subtracted:

| workload | native | pulley | ratio | exercises |
|---|---|---|---|---|
| `intloop` | 33 ms | 620 ms | 19x | mul/xor/rotate/divide |
| `memsum` | 9 ms | 280 ms | 31x | 32 MiB of loads and stores |
| `fib` | 12 ms | 202 ms | 17x | calls, recursion |
| `wide` | 7 ms | 107 ms | 15x | i128 carry chains |
| `floatloop` | 37 ms | 488 ms | 13x | FP arithmetic, native `nearest` |

So the "must beat the interpreter convincingly" bar is cleared by a wide
margin, and these numbers are the concrete justification to put in front
of maintainers in Phase 0. Measurement noise is around 10%, which bounds
what any tuning claim below can honestly assert.

**`setb` is not worth doing.** It would shorten `cmp_set` (currently
`cmp` + `li` + `li` + `isel`) by one instruction, but `cmp_set` only
fires when an `icmp` result is materialized as a value, and the lowering
rules already fuse `icmp` into `brif` and `select`. Disassembling all
five workloads found **zero `isel` instructions**: the path `setb` would
optimize never executes in this code. Recorded here so nobody re-derives
it.

**Instruction count is the wrong lever on this core.** Constant shifts
now use the rotate-and-mask immediate forms, removing three of the 27
instructions in `intloop`'s hot loop -- and wall-clock time did not
change at all. The deleted `li`s were independent of the loop's
dependency chain, which is what a wide out-of-order POWER9 is actually
limited by. The same reasoning applies to the ten instructions of
loop-invariant 64-bit constant materialization sitting in that loop
(`lis`/`ori`/`sldi`/`oris`/`ori` twice over, because the mid-end
rematerializes constants at their use sites): they are equally off the
critical path, so a constant-pool load would likely not help either, and
would add memory traffic. Any future tuning should target dependency
chains or memory behaviour, and must be measured -- given ~10% noise,
an 11% instruction reduction is not even detectable here.

### Phase 7 — Big-endian ppc64: ELFv2-BE, then ELFv1 (≈2–4 months)

**Targets and ABI selection.** Big-endian is
`target_lexicon::Architecture::Powerpc64` (`powerpc64-unknown-linux-gnu` =
glibc = **ELFv1**; `powerpc64-unknown-linux-musl` and
`powerpc64-unknown-freebsd` = **ELFv2**; all are Rust targets, so toolchains
exist). The ELF ABI version is *not* in the triple's architecture, so the
backend needs an ISA setting — e.g. `abi_elfv1` in
`meta/src/isa/ppc64.rs` — defaulted from
`triple.environment()`/`operating_system()` in the `isa_constructor`,
overridable like any flag. It must also flow through the three
feature-detection duplicates (Phase 3) and
`Engine::check_compatible_with_shared_flag`.

**7a — Endianness (shared by both ABIs).**

- `TargetIsa::endianness()` comes free from the triple; test infrastructure
  already understands BE targets (`pulley64be` appears in ~217 runtests, and
  `Engine::_check_compatible_with_native_host` already matches endianness
  for Pulley).
- The real work is memory access: Wasmtime marks wasm loads/stores as
  explicitly little-endian via `MemFlags::endianness`, so on a BE host the
  backend must emit byte-reversed accesses — `lhbrx/lwbrx/ldbrx`,
  `sthbrx/stwbrx/stdbrx`. FP has no byte-reversed load form, so LE FP
  accesses need a GPR bounce (`ldbrx` + `mtfprd`, ISA 2.07+) — same problem
  s390x solved with `lrvg`-then-`ldgr` patterns; crib the structure from
  `s390x/lower/isle.rs:651–662`.
- Vectors resurrect the s390x `LaneOrder` machinery: lane numbering flips
  between the wasm (LE) view and the BE register view, so lane-index
  remapping, shuffle-mask permutation, and lane-swapping moves at ABI
  boundaries all return (`s390x/abi.rs:1101`, `lower/isle.rs:348–400` are
  the template — s390x derives `LaneOrder` from the calling convention:
  `Tail` = LE lanes for wasm code, SystemV = native order). Pre-P9 VSX
  memory ops (`lxvd2x`) have element-order quirks (`xxswapd` fixups) the
  SpiderMonkey patch already fought through — its SIMD regression tests
  double in value here.

**7b — ELFv2-BE first (≈1–1.5 months on top of a finished LE backend).**
With 7a done, ELFv2-BE is mostly the same ABI code as ppc64le: same 32-byte
frame, same LR/CR/TOC save slots, same register conventions. Deltas:
`lookup()` arm for `Architecture::Powerpc64` (same `ppc64` feature gate),
`object::Architecture` + `Endianness::Big` in
`crates/environ/src/compile/mod.rs`, BE `target` variants in runtests, and
fuzzgen's NaN-canonicalisation/interpreter differential already copes with
BE (s390x precedent). CI: cross-compile with `-mabi=elfv2` or the musl
target; qemu-user `qemu-ppc64`.

**7c — ELFv1 last, explicitly optional (≈1.5–2.5 months).** The cost
concentrates in **function descriptors**:

- Every function symbol is a 3-doubleword descriptor (entry, TOC, env) in
  `.opd`. Calling a C function pointer means dereferencing the descriptor
  and loading r2; conversely, host (Rust) code calling into JIT entry points
  expects a descriptor, so Wasmtime must synthesise `.opd`-style descriptors
  for every exported trampoline in `code_memory`/loader. Touches
  `crates/cranelift/src/obj.rs`, `cranelift-object`, `cranelift-jit`, and
  the loader — none of which today have any concept of "a function pointer
  is not the code address".
- Internal wasm→wasm calls stay descriptor-free (we own the `Tail`
  convention); descriptors appear only at SystemV boundaries: host imports,
  libcalls, array-call trampolines.
- Frame layout differs: 48-byte minimum frame (backchain, CR, LR,
  compiler/linker dwords, TOC) vs ELFv2's 32, plus a mandatory 64-byte
  parameter save area in more cases — parameterise `abi.rs` frame constants
  on `abi_elfv1` rather than forking the file.
- Unwinder/fiber/signals shared with 7b except descriptor-aware
  indirect-call sites in the fiber switch and
  `resume_to_exception_handler`.

Gate 7c on demonstrated demand: glibc-BE-Linux is the main ELFv1 consumer,
while active BE communities (FreeBSD, Adélie, musl distros) are already
ELFv2. If no concrete user shows up, 7c can sit unbuilt behind `abi_elfv1`
returning "unsupported" — a legitimate Tier-3 posture.

**Sequencing option**: pull 7a's *design constraints* (MemFlags plumbing,
lane-index helper) into Phase 1 as hard requirements — recommended
regardless — but keep all BE *implementation* after Phase 5, since
SIMD-on-BE is the interaction that generates most of the bugs and there is
no point debugging it twice.

## 5. Risks

- **No PC-relative addressing pre-POWER10** — affects constant/address
  materialisation, island design, code size; decide in Phase 1, not after
  10k lines.
- **CR-field condition model** is unlike all four existing backends
  (closest: s390x condition codes); icmp/brif ISLE patterns need careful
  design to avoid materialising booleans everywhere.
- **Maintainer bandwidth is the actual gate**: riscv64 is still Tier 3 years
  after merging precisely because of the "full-time maintainer + CI" bar.
  Scalar backend ≈4–7 months of focused work; the social commitment is
  open-ended. Big-endian roughly doubles the test matrix (LE/BE ×
  POWER8/9 × SIMD).
- **Review load upstream**: a 15–25k-line backend cannot land as one PR.
  Sequence: scaffolding → ABI/frame → scalar ISA in instruction-family
  chunks → runtime enablement → CI, mirroring riscv64
  ([PR #4271](https://github.com/bytecodealliance/wasmtime/pull/4271) and
  ~100 follow-ups) — each PR opened manually per the BA AI Tool Use Policy.

## 6. AI model fit per phase

Working principle: **Fable 5 for design, debugging, and review; Opus for the
bulk of the coding; Sonnet/Haiku for mechanical sweeps.** Fable 5's edge is
on novel-architecture reasoning and cross-cutting correctness — exactly the
parts of this project that are expensive to get wrong — but most of a
backend's line count is pattern-following work where Opus is just as good at
a fraction of the cost.

| Work | Model | Why |
|---|---|---|
| Phase 0 — RFC/tracking-issue drafting | Opus | Prose from an existing plan; no deep reasoning needed |
| Phase 1 — registration glue (meta, features, `lookup()`) | Opus | Pure pattern-matching against riscv64/s390x precedents |
| Phase 1 — `abi.rs` frame/`MachineEnv` design, constant-materialisation strategy (no-pcrel problem) | **Fable 5** | Foundational decisions; errors here cascade through everything |
| Phase 2 — `inst/encode.rs`, `emit.rs`, `emit_tests.rs` golden encodings | Opus | Transcription from the ISA manual / patch opcode table; verifiable against `llvm-mc` |
| Phase 2 — ISLE lowering rule *design* (CR-field icmp/brif fusion, i128, atomics fence placement) | **Fable 5** | Novel design space, subtle correctness (memory ordering, overflow semantics) |
| Phase 2 — ISLE rule *bulk fill-in* once patterns are established | Opus | Each new op follows the established rule shape |
| Phase 3 — unwinder/fiber/signals asm | **Fable 5** | Small but unforgiving: CFI, signal contexts, exception resume must match codegen exactly |
| Phase 3 — feature-detection plumbing (3 duplicate sites), `environ` mapping | Opus | Mechanical, well-templated by existing arches |
| Phase 4 — adding `target ppc64` to ~340 runtests, Cargo forwards, CI YAML | Sonnet (or Haiku) | Pure mechanical sweeps; even Opus is overkill |
| Phase 4 — triaging runtest/fuzzgen failures | **Fable 5** | Miscompilation debugging is the highest-leverage use of the expensive model |
| Phase 5 — SIMD lowerings | Opus, escalate to Fable 5 | Bulk is patterned; lane-semantics bugs (the patch's P8/P9 corner cases) go to Fable 5 |
| Phase 7a/7c — LaneOrder BE design, ELFv1 function descriptors | **Fable 5** | The two genuinely novel subsystems in the whole plan |
| Phase 7b — ELFv2-BE deltas | Opus | Mostly parameterising existing code |
| Ongoing — pre-PR review passes | **Fable 5** | Cheap insurance relative to upstream review cycles |

**Operating mode: manual model switching, no subagent delegation** (subagent
fan-out burns through Claude subscription rate limits too quickly). Switch
the session model by hand with `/model` at these checkpoints:

- **Start of a work block, per the table above**: `/model opus` before
  implementation loops; `/model` back to Fable 5 before design work,
  asm-level runtime code, or debugging.
- **Escalate to Fable 5** when (a) touching any §3 design decision, (b) a
  bug survives two Opus attempts, (c) starting the pre-PR review pass.
- **Drop to Sonnet/Haiku** for the mechanical sweeps (runtest `target`
  lines, Cargo forwards, CI YAML) — check the current model before starting
  one of these; it is the easiest place to waste Fable 5 quota.

Whichever model is active should call out a mismatch: if asked to do bulk
coding while running as Fable 5 (or design work while running as Opus), say
so and suggest the switch before proceeding.

Policy reminder (applies to all of the above): per the
[BA AI Tool Use Policy](https://github.com/bytecodealliance/governance/blob/main/AI_TOOL_POLICY.md),
every upstream PR is opened by a human, all output is human-reviewed and
human-accountable, and no AI tool is listed as a commit co-author.

## Sources

- [PowerPC (64) Support — wasmtime #1183](https://github.com/bytecodealliance/wasmtime/issues/1183)
- [Tiers of support — Wasmtime docs](https://docs.wasmtime.dev/stability-tiers.html)
- [Platform support — Wasmtime docs](https://docs.wasmtime.dev/stability-platform-support.html)
- [Cranelift README](https://github.com/bytecodealliance/wasmtime/blob/main/cranelift/README.md)
- [riscv64 backend PR #4271](https://github.com/bytecodealliance/wasmtime/pull/4271)
- SpiderMonkey ppc64le JIT patch: `~/Downloads/0004-Add-PPC64LE-JIT-backend.patch`
  (MPL-2.0 — reference only, no code reuse)
