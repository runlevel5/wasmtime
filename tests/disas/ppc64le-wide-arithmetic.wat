;;! target = "powerpc64le"
;;! test = "compile"
;;! flags = "-Wwide-arithmetic"

(module
  (func $add128 (param i64 i64 i64 i64) (result i64 i64)
    local.get 0
    local.get 1
    local.get 2
    local.get 3
    i64.add128)

  (func $sub128 (param i64 i64 i64 i64) (result i64 i64)
    local.get 0
    local.get 1
    local.get 2
    local.get 3
    i64.sub128)

  (func $signed (param i64 i64) (result i64 i64)
    local.get 0
    local.get 1
    i64.mul_wide_s)

  (func $unsigned (param i64 i64) (result i64 i64)
    local.get 0
    local.get 1
    i64.mul_wide_u)

  (func $signed_only_high (param i64 i64) (result i64)
    local.get 0
    local.get 1
    i64.mul_wide_s
    local.set 0
    drop
    local.get 0)

  (func $unsigned_only_high (param i64 i64) (result i64)
    local.get 0
    local.get 1
    i64.mul_wide_u
    local.set 0
    drop
    local.get 0)
)
;; wasm[0]::function[0]::add128:
;;       mflr    r0
;;       addi    r1, r1, -0x10
;;       std     r0, 8(r1)
;;       std     r31, 0(r1)
;;       ori     r31, r1, 0
;;       addc    r3, r5, r7
;;       adde    r4, r6, r8
;;       ld      r0, 8(r1)
;;       mtlr    r0
;;       ld      r31, 0(r1)
;;       addi    r1, r1, 0x10
;;       blr
;;
;; wasm[0]::function[1]::sub128:
;;       mflr    r0
;;       addi    r1, r1, -0x10
;;       std     r0, 8(r1)
;;       std     r31, 0(r1)
;;       ori     r31, r1, 0
;;       subfc   r3, r7, r5
;;       subfe   r4, r8, r6
;;       ld      r0, 8(r1)
;;       mtlr    r0
;;       ld      r31, 0(r1)
;;       addi    r1, r1, 0x10
;;       blr
;;
;; wasm[0]::function[2]::signed:
;;       mflr    r0
;;       addi    r1, r1, -0x10
;;       std     r0, 8(r1)
;;       std     r31, 0(r1)
;;       ori     r31, r1, 0
;;       li      r12, 0x3f
;;       srad    r4, r5, r12
;;       li      r12, 0x3f
;;       srad    r7, r6, r12
;;       mulld   r3, r5, r6
;;       mulhdu  r9, r5, r6
;;       mulld   r12, r4, r6
;;       mulld   r4, r5, r7
;;       add     r4, r12, r4
;;       add     r4, r9, r4
;;       ld      r0, 8(r1)
;;       mtlr    r0
;;       ld      r31, 0(r1)
;;       addi    r1, r1, 0x10
;;       blr
;;
;; wasm[0]::function[3]::unsigned:
;;       mflr    r0
;;       addi    r1, r1, -0x10
;;       std     r0, 8(r1)
;;       std     r31, 0(r1)
;;       ori     r31, r1, 0
;;       li      r10, 0
;;       li      r12, 0
;;       mulld   r3, r5, r6
;;       mulhdu  r7, r5, r6
;;       mulld   r9, r10, r6
;;       mulld   r12, r5, r12
;;       add     r4, r9, r12
;;       add     r4, r7, r4
;;       ld      r0, 8(r1)
;;       mtlr    r0
;;       ld      r31, 0(r1)
;;       addi    r1, r1, 0x10
;;       blr
;;
;; wasm[0]::function[4]::signed_only_high:
;;       mflr    r0
;;       addi    r1, r1, -0x10
;;       std     r0, 8(r1)
;;       std     r31, 0(r1)
;;       ori     r31, r1, 0
;;       li      r12, 0x3f
;;       srad    r3, r5, r12
;;       li      r12, 0x3f
;;       srad    r4, r6, r12
;;       mulld   r7, r5, r6
;;       mulhdu  r9, r5, r6
;;       mulld   r12, r3, r6
;;       mulld   r3, r5, r4
;;       add     r3, r12, r3
;;       add     r3, r9, r3
;;       ld      r0, 8(r1)
;;       mtlr    r0
;;       ld      r31, 0(r1)
;;       addi    r1, r1, 0x10
;;       blr
;;
;; wasm[0]::function[5]::unsigned_only_high:
;;       mflr    r0
;;       addi    r1, r1, -0x10
;;       std     r0, 8(r1)
;;       std     r31, 0(r1)
;;       ori     r31, r1, 0
;;       li      r10, 0
;;       li      r12, 0
;;       mulld   r7, r5, r6
;;       mulhdu  r7, r5, r6
;;       mulld   r9, r10, r6
;;       mulld   r12, r5, r12
;;       add     r3, r9, r12
;;       add     r3, r7, r3
;;       ld      r0, 8(r1)
;;       mtlr    r0
;;       ld      r31, 0(r1)
;;       addi    r1, r1, 0x10
;;       blr
