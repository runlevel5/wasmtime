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
- Still out of scope: i128 div/rem (only s390x lowers these), `cls`,
  `bswap`/`bitrev`/`iabs` (all widths), `nearest` (PPC's `frin`
  rounds ties away from zero, not to even — needs a fixup sequence),
  128-bit atomics.

### Phase 5 — SIMD via VSX (optional, +2–3 months)

~236 SIMD ops; POWER8 VSX covers most of wasm SIMD but the patch's SIMD
regression tests (extract-lane canonicalisation, extmul aliased dest,
high-lane corruption on P8 vs P9) are exactly the corner cases to encode as
runtests. Keep gated off until scalar is solid.

### Phase 6 — Tuning

POWER9/10 fast paths (`isel`, `setb`, mod instructions, P10 pcrel to kill
constant-materialisation sequences), egraph-visible lowering improvements,
benchmarking vs Pulley (the backend must beat the interpreter convincingly
to justify itself).

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
