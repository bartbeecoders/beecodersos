/*
 * BeeCodersOs - interrupts.s
 *
 * Entry stubs for interrupt vectors 0-47 (CPU exceptions 0-31, PIC IRQs 32-47).
 * Every stub pushes a uniform frame (error code or 0, vector number) and jumps
 * to isr_common, which saves the caller-saved registers and calls
 * `interrupt_dispatch(frame: *mut InterruptFrame)` in Rust.
 * Generated layout; see interrupts.rs for the matching Rust struct.
 */

.section .text
.code64

isr_0:
    push 0
    push 0
    jmp isr_common

isr_1:
    push 0
    push 1
    jmp isr_common

isr_2:
    push 0
    push 2
    jmp isr_common

isr_3:
    push 0
    push 3
    jmp isr_common

isr_4:
    push 0
    push 4
    jmp isr_common

isr_5:
    push 0
    push 5
    jmp isr_common

isr_6:
    push 0
    push 6
    jmp isr_common

isr_7:
    push 0
    push 7
    jmp isr_common

isr_8:
    push 8
    jmp isr_common

isr_9:
    push 0
    push 9
    jmp isr_common

isr_10:
    push 10
    jmp isr_common

isr_11:
    push 11
    jmp isr_common

isr_12:
    push 12
    jmp isr_common

isr_13:
    push 13
    jmp isr_common

isr_14:
    push 14
    jmp isr_common

isr_15:
    push 0
    push 15
    jmp isr_common

isr_16:
    push 0
    push 16
    jmp isr_common

isr_17:
    push 17
    jmp isr_common

isr_18:
    push 0
    push 18
    jmp isr_common

isr_19:
    push 0
    push 19
    jmp isr_common

isr_20:
    push 0
    push 20
    jmp isr_common

isr_21:
    push 21
    jmp isr_common

isr_22:
    push 0
    push 22
    jmp isr_common

isr_23:
    push 0
    push 23
    jmp isr_common

isr_24:
    push 0
    push 24
    jmp isr_common

isr_25:
    push 0
    push 25
    jmp isr_common

isr_26:
    push 0
    push 26
    jmp isr_common

isr_27:
    push 0
    push 27
    jmp isr_common

isr_28:
    push 0
    push 28
    jmp isr_common

isr_29:
    push 29
    jmp isr_common

isr_30:
    push 30
    jmp isr_common

isr_31:
    push 0
    push 31
    jmp isr_common

isr_32:
    push 0
    push 32
    jmp isr_common

isr_33:
    push 0
    push 33
    jmp isr_common

isr_34:
    push 0
    push 34
    jmp isr_common

isr_35:
    push 0
    push 35
    jmp isr_common

isr_36:
    push 0
    push 36
    jmp isr_common

isr_37:
    push 0
    push 37
    jmp isr_common

isr_38:
    push 0
    push 38
    jmp isr_common

isr_39:
    push 0
    push 39
    jmp isr_common

isr_40:
    push 0
    push 40
    jmp isr_common

isr_41:
    push 0
    push 41
    jmp isr_common

isr_42:
    push 0
    push 42
    jmp isr_common

isr_43:
    push 0
    push 43
    jmp isr_common

isr_44:
    push 0
    push 44
    jmp isr_common

isr_45:
    push 0
    push 45
    jmp isr_common

isr_46:
    push 0
    push 46
    jmp isr_common

isr_47:
    push 0
    push 47
    jmp isr_common

isr_common:
    push rax
    push rcx
    push rdx
    push rsi
    push rdi
    push r8
    push r9
    push r10
    push r11
    cld
    mov rdi, rsp            /* &InterruptFrame; rsp is 16-byte aligned here */
    call interrupt_dispatch
    pop r11
    pop r10
    pop r9
    pop r8
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rax
    add rsp, 16             /* drop vector + error code */
    iretq

.section .rodata
.align 8
.global isr_table
isr_table:
    .quad isr_0
    .quad isr_1
    .quad isr_2
    .quad isr_3
    .quad isr_4
    .quad isr_5
    .quad isr_6
    .quad isr_7
    .quad isr_8
    .quad isr_9
    .quad isr_10
    .quad isr_11
    .quad isr_12
    .quad isr_13
    .quad isr_14
    .quad isr_15
    .quad isr_16
    .quad isr_17
    .quad isr_18
    .quad isr_19
    .quad isr_20
    .quad isr_21
    .quad isr_22
    .quad isr_23
    .quad isr_24
    .quad isr_25
    .quad isr_26
    .quad isr_27
    .quad isr_28
    .quad isr_29
    .quad isr_30
    .quad isr_31
    .quad isr_32
    .quad isr_33
    .quad isr_34
    .quad isr_35
    .quad isr_36
    .quad isr_37
    .quad isr_38
    .quad isr_39
    .quad isr_40
    .quad isr_41
    .quad isr_42
    .quad isr_43
    .quad isr_44
    .quad isr_45
    .quad isr_46
    .quad isr_47
