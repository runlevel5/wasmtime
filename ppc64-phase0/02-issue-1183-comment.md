# Phase 0 artifact 2 — comment to revive wasmtime#1183

Post as a comment on
[wasmtime#1183](https://github.com/bytecodealliance/wasmtime/issues/1183),
which is still open and already carries the `cranelift:new-target` label.
Reviving it beats filing a duplicate.

Use this *after* the Zulip conversation, so you can say what was agreed.
**You must post this yourself** — per the Bytecode Alliance AI Tool Use
Policy and this repo's `AGENTS.md`, issues and PRs are human-only.

---

Reviving this one. I'd like to take a run at a ppc64le Cranelift backend,
and I've started a thread on Zulip in `#cranelift` to work out whether the
maintainers want it and in what form. <!-- link the thread once posted -->

Short version of where I'm coming from: I've been working on the ppc64le
port of SpiderMonkey's JIT, continuing Cameron Kaiser and Justin Hibbits'
work — baseline, Ion, and wasm including SIMD. That code is MPL-2.0 so none
of it can be reused here, but the ISA knowledge, verified encodings and ABI
notes carry over, and I have POWER9 and POWER10 hardware to develop and test
on.

The plan I'm proposing is `powerpc64le-unknown-linux-gnu` on ELFv2, POWER8
baseline with ISA flags for the POWER9 and POWER10 additions, scalar first
with SIMD gated off and added later, and Tier 3 at merge with me as
maintainer. Structurally it would follow riscv64 — fixed-width encodings,
fully ISLE-lowered — with s390x as the reference for the ABI and frame code.

Worth noting for anyone watching this issue: powerpc64le is currently used
in CI as the deliberate example of an architecture with *no* Cranelift
backend, so part of this work is moving that canary to another target.

Happy to fold in anyone who wants to help, particularly on testing across
POWER generations. I'll update here once the Zulip thread settles on a
direction.
