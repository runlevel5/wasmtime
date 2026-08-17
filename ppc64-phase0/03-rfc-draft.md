# Phase 0 artifact 3 — draft RFC

Only needed if the Zulip thread says they want one. It follows
`template-draft.md` from bytecodealliance/rfcs (Summary / Motivation /
Proposal sketch / Open questions), which is the right template for seeking
early input. Open it as a **draft pull request** adding this file to
`accepted/`, named something like `cranelift-ppc64-backend.md`, and label it
for Cranelift.

**You must open the PR yourself** — AI tools may not open or comment on PRs
in Bytecode Alliance repos, and no AI tool may appear as a co-author.

Fill in your name and time commitment, and settle the big-endian question,
before opening it.

---

# Summary

Add a ppc64le (64-bit PowerPC, little-endian) backend to Cranelift, entering
at Tier 3, targeting `powerpc64le-unknown-linux-gnu` with the ELFv2 ABI.

# Motivation

PowerPC is the last mainstream 64-bit server architecture with no Cranelift
backend. Wasmtime runs there today only through Pulley, so anything that
cares about compiled-code performance — Wasmtime itself, and non-Wasmtime
Cranelift users like `cranelift-jit` consumers and the Rust `rustc_codegen_cranelift`
backend — falls back to interpretation. The request has been open since 2020
(wasmtime#1183) and the platform has an active community around OpenPOWER
hardware, IBM Power servers, and distributions that ship ppc64le as a
first-class target.

There's a secondary reason: powerpc64le currently serves in Wasmtime's CI as
the deliberate example of an architecture with no Cranelift backend. That is
a reasonable role for it to have outgrown.

I'm proposing this as someone who has done the equivalent work once already.
I've been working on the ppc64le port of SpiderMonkey's JIT, continuing
Cameron Kaiser's and Justin Hibbits' effort — baseline and optimising JIT,
plus wasm including SIMD. That code is MPL-2.0 and none of it will be reused
here; what carries over is knowing where this ISA hides its problems, a set
of verified instruction encodings, and ABI notes. I have persistent POWER9
and POWER10 hardware for development and testing.

# Proposal sketch

Scope for the initial merge is deliberately narrow: little-endian, ELFv2,
Linux, scalar only.

The baseline would be POWER8 — the first little-endian-capable generation and
what distributions assume — with `has_isa_3_0` and `has_isa_3_1` ISA flags
opting into POWER9 and POWER10 instructions where they help (`modsd`/`modud`,
`setb`, `isel`, and eventually POWER10's prefixed instructions). SIMD would
be gated off initially behind a flag, following the precedent riscv64 set
with `has_v`, and added once the scalar backend is solid.

Structurally the backend follows riscv64: fixed-width 32-bit encodings with a
dedicated encoding module, fully ISLE-lowered with a thin `LowerBackend`. For
the ABI and frame layout, s390x is the better reference — it has the same
shape of problem, with a caller-allocated save area and a stack pointer that
doesn't move after the prologue. I'd also follow s390x's convention of
declaring instruction enums in ISLE rather than hand-written Rust.

Beyond the backend directory, the work touches shared code in a few places:
new `Reloc` variants and their mappings in `cranelift-object` and
`cranelift-jit`; on the Wasmtime side an `unwinder` architecture module,
fiber stack switching, signal-context extraction, icache coherence (PowerPC
has incoherent instruction and data caches and needs an explicit
`dcbst`/`sync`/`icbi`/`sync`/`isync` sequence), and host feature detection.
None of it is structurally novel — every existing backend needed the same
set — but it's worth naming up front.

Two things genuinely differ from everything currently in tree, and they're
the parts I most want reviewed early.

The first is that **PowerPC has no PC-relative addressing before POWER10**.
Materialising a constant or an address means either an immediate sequence
(`lis`/`ori`/`rldicr`/…) combined with `MachBuffer` constant islands, or
going through a TOC. Every other Cranelift backend can assume PC-relative
addressing exists. My inclination is to go TOC-free and lean on constant
islands, treating POWER10's `paddi`/`pld` as a later optimisation, but this
decision shapes a lot of the backend and I'd rather settle it in review than
discover it was wrong at 10,000 lines.

The second is that comparison results land in **4-bit condition register
fields** rather than general-purpose registers. s390x's condition codes are
the closest analogue, but PowerPC has eight independent CR fields, which
makes the `icmp`/`fcmp` plus `brif` fusion rules a real design exercise
rather than a transcription.

Delivery would be incremental, following how riscv64 landed: scaffolding and
registration first, then ABI and frame layout, then the scalar instruction
set in chunks by instruction family, then Wasmtime runtime enablement, then
CI and fuzzing. Each is a separate reviewable PR rather than one enormous
drop.

On support commitment: Tier 3 at merge, with me named as maintainer, and
QEMU user-mode CI as the immediate follow-up since that's the concrete gap
between Tier 3 and Tier 2.
<!-- TODO: state hours/week, whether the work is funded, and any co-maintainers -->

# Open questions

**Is big-endian wanted?** ELFv2 big-endian is a moderate increment once
endianness is handled properly in the load/store and vector lane paths —
essentially the machinery s390x already has. ELFv1 is a much larger
commitment, because its function descriptors mean a function pointer is not
a code address, and that assumption leaks into object emission and
Wasmtime's loader. I'd rather know now whether to design for either, since
it affects how the memory and lane-order abstractions are built from day
one. My default, absent interest, is to build little-endian only and leave
the seams in the right places.

**Is SIMD-deferred acceptable for an initial merge?** riscv64 set the
precedent with `has_v`, but I want to confirm that a scalar-only backend is
mergeable rather than something to be held until it's complete.

**How much shared-code growth is acceptable?** Particularly the new `Reloc`
variants and the arms they require across `cranelift-object` and
`cranelift-jit`.

**What does CI look like?** A QEMU user-mode job matching the s390x and
riscv64 pattern is the obvious starting point, and I can also offer real
POWER9 and POWER10 hardware if there's an appetite for wiring self-hosted
runners in later.

**Does the constant-materialisation approach above look right to people who
have built these backends before?** This is the question I'd most like
answered before writing code.
