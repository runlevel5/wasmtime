# Phase 0 artifact 1 — Zulip pre-RFC post

Post to the Bytecode Alliance Zulip, `#cranelift` stream, new topic
`ppc64le backend`. Do this before anything else — riscv64 landed without an
RFC (PR #4271 plus follow-ups), so the first thing to find out is whether
they want an RFC at all or just a tracking issue and incremental PRs.

Before posting: fill in your name/affiliation and your realistic time
commitment, and decide what you want to say about big-endian.

Hardware verified 2026-08-17: `power9` is bare-metal POWER9 (pvr 004e1203),
`power10` is POWER10 (pvr 0080 0200), and the host aliased `power8` is
actually a POWER9 LPAR in architected mode (pvr 004e1202, advertises
`arch_3_00`) — not POWER8 silicon. The paragraph below is worded to match.
If you boot that LPAR in POWER8 compatibility mode, or get real ISA 2.07
hardware, change it to say so outright.

---

Hi all — before I write any code I wanted to ask whether a **ppc64le backend
for Cranelift** is something the project would actually want. The platform
support doc says to check in first, so, checking in.

Quick background on me: I've spent a while porting SpiderMonkey's JIT to
ppc64le, building on Cameron Kaiser and Justin Hibbits' work in gecko-dev —
baseline, Ion and wasm including SIMD. It's MPL-2.0, so none of that code
can come across to Wasmtime, but it does mean I've already run into most of
this ISA's sharp edges and I have a lot of verified encodings and ABI notes
to work from. There's also an open request for ppc64 from 2020 (#1183) that
never went anywhere.

Hardware-wise I have long-term access to POWER9 and POWER10 machines, and I
can cover the POWER8 baseline through compatibility mode and QEMU, so all
three ISA levels the backend would care about are testable.

What I'd aim at for a first merge: `powerpc64le-unknown-linux-gnu`, ELFv2,
little-endian, POWER8 baseline with ISA flags for the POWER9 and POWER10
additions. Scalar only to start, with SIMD gated off the way riscv64 does
with `has_v` and added later. Structurally I'd follow riscv64 — fixed-width
encodings, fully ISLE-lowered — and borrow s390x's conventions for the ABI
code. Tier 3 at merge, with me as maintainer, and QEMU-based CI as the
immediate next step.

Three things I'd rather argue about now than after 10k lines:

The big one is that **PPC has no PC-relative addressing before POWER10**, so
constants and addresses have to come from immediate sequences plus constant
islands, or from a TOC. Nothing else in tree looks like this. I lean
TOC-free with islands and treat P10's `pcrel` as a later optimisation, but
I'd like a second opinion.

Comparisons also land in 4-bit condition register fields rather than GPRs,
which makes the `icmp`/`brif` fusion rules more interesting than usual —
s390x is the closest precedent.

And there's a fair amount of shared code involved: new `Reloc` variants,
arms in cranelift-object and cranelift-jit, plus on the Wasmtime side an
unwinder module, fiber stack switching, signal context handling and icache
coherence (PPC needs an explicit dcbst/sync/icbi/sync/isync dance). Happy to
hear if that footprint is a problem.

Two smaller things. Is big-endian interesting to anyone? ELFv2-BE is a
reasonable increment once endianness is handled properly, but ELFv1 is a
different story — function descriptors mean a function pointer isn't a code
address, which leaks into object emission and the loader. I'd rather know
now whether to design for it. And `platform_checks` currently uses
powerpc64le as its deliberate "architecture with no backend" canary, so that
would need to move to loongarch64 or similar.

I've got a phased plan and a map of the integration points ready to turn
into either an RFC or a tracking issue — whichever you prefer. Is this
welcome? And would anyone be up for looking at the ISLE design early on?
