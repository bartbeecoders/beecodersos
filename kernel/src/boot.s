/*
 * BeeCodersOs - boot.s
 *
 * Early boot code. A Multiboot (v1) loader such as QEMU's `-kernel` or GRUB
 * drops us here in 32-bit protected mode with paging off. We:
 *
 *   1. verify we were started by a Multiboot loader and the CPU supports long mode
 *   2. build identity-mapped page tables for the first 4 GiB (2 MiB pages)
 *   3. enable PAE + long mode + paging
 *   4. load a 64-bit GDT and far-jump into 64-bit code
 *   5. call the Rust entry point `kernel_main(magic, multiboot_info)`
 *
 * Intel syntax (LLVM integrated assembler, pulled in via `global_asm!`).
 */

.set MB_MAGIC,    0x1BADB002
.set MB_FLAGS,    0x00000007          /* page-align modules + provide memory map + set a video mode */
.set MB_CHECKSUM, -(MB_MAGIC + MB_FLAGS)

/* ------------------------------------------------------------------------- */
.section .multiboot_header, "a"
.align 4
    .long MB_MAGIC
    .long MB_FLAGS
    .long MB_CHECKSUM
    .long 0, 0, 0, 0, 0                  /* load addresses: unused, the kernel is an ELF file */
    .long 0                              /* video mode: 0 = graphics (a linear framebuffer)   */
    .long 1920                           /* preferred width                                    */
    .long 1080                           /* preferred height                                   */
    .long 32                             /* preferred bits per pixel                           */

/* ------------------------------------------------------------------------- */
.section .bss
.align 4096
boot_pml4:  .skip 4096
boot_pdpt:  .skip 4096
boot_pd:    .skip 4 * 4096               /* 4 page directories = 4 GiB */
boot_stack_bottom:
    .skip 64 * 1024
boot_stack_top:

/* ------------------------------------------------------------------------- */
.section .rodata
.align 16
gdt64:
    .quad 0                                  /* null descriptor              */
    .quad 0x00AF9A000000FFFF                 /* 0x08: 64-bit code, ring 0    */
    .quad 0x00CF92000000FFFF                 /* 0x10: data, ring 0           */
gdt64_end:

gdt64_ptr:
    .word gdt64_end - gdt64 - 1
    .quad gdt64

/* ------------------------------------------------------------------------- */
.section .text.boot, "ax"
.code32
.global _start
_start:
    cli
    mov esp, offset boot_stack_top
    mov edi, eax                         /* multiboot magic (kept for Rust) */
    mov esi, ebx                         /* multiboot info pointer          */

    /* --- 1a. started by a Multiboot loader? -------------------------- */
    cmp eax, 0x2BADB002
    jne .Lerr_multiboot

    /* --- 1b. CPUID available? (try flipping EFLAGS.ID) --------------- */
    pushfd
    pop eax
    mov ecx, eax
    xor eax, 1 << 21
    push eax
    popfd
    pushfd
    pop eax
    push ecx
    popfd
    cmp eax, ecx
    je .Lerr_cpuid

    /* --- 1c. long mode supported? ------------------------------------- */
    mov eax, 0x80000000
    cpuid
    cmp eax, 0x80000001
    jb .Lerr_longmode
    mov eax, 0x80000001
    cpuid
    test edx, 1 << 29
    jz .Lerr_longmode

    /* --- 2. page tables: PML4[0] -> PDPT[0..3] -> 4 PDs (2048 x 2 MiB) - */
    mov eax, offset boot_pdpt
    or eax, 0x3                          /* present | writable */
    mov dword ptr [boot_pml4], eax

    xor ecx, ecx
.Lmap_pdpt:
    mov eax, ecx
    shl eax, 12                          /* eax = ecx * 4096 */
    add eax, offset boot_pd
    or eax, 0x3
    mov dword ptr [boot_pdpt + ecx * 8], eax
    inc ecx
    cmp ecx, 4
    jne .Lmap_pdpt

    xor ecx, ecx
.Lmap_pd:
    mov eax, ecx
    shl eax, 21                          /* eax = ecx * 2 MiB (fits: < 4 GiB) */
    or eax, 0x83                         /* present | writable | huge */
    mov dword ptr [boot_pd + ecx * 8], eax
    inc ecx
    cmp ecx, 2048
    jne .Lmap_pd

    /* --- 3. enable PAE, long mode, paging ----------------------------- */
    mov eax, offset boot_pml4
    mov cr3, eax

    mov eax, cr4
    or eax, 1 << 5                       /* CR4.PAE */
    mov cr4, eax

    mov ecx, 0xC0000080                  /* EFER MSR */
    rdmsr
    or eax, 1 << 8                       /* EFER.LME */
    wrmsr

    mov eax, cr0
    or eax, (1 << 31) | (1 << 0)         /* CR0.PG | CR0.PE */
    mov cr0, eax

    /* --- 4. load 64-bit GDT and far jump into long mode --------------- */
    lgdt [gdt64_ptr]
    mov eax, offset long_mode_start
    push 0x08                            /* code selector */
    push eax                             /* target RIP    */
    retf

/* Error path: print "ERR: X" in red to the VGA text buffer and halt. */
.Lerr_multiboot:
    mov al, 'M'
    jmp .Lerror
.Lerr_cpuid:
    mov al, 'C'
    jmp .Lerror
.Lerr_longmode:
    mov al, 'L'
.Lerror:
    mov dword ptr [0xB8000], 0x4F524F45   /* "ER" */
    mov dword ptr [0xB8004], 0x4F3A4F52   /* "R:" */
    mov dword ptr [0xB8008], 0x4F204F20   /* "  " */
    mov byte  ptr [0xB800A], al
.Lhang32:
    hlt
    jmp .Lhang32

/* ------------------------------------------------------------------------- */
.code64
long_mode_start:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    xor ax, ax
    mov fs, ax
    mov gs, ax

    /* rdi = multiboot magic, rsi = multiboot info (zero-extended above) */
    mov edi, edi
    mov esi, esi
    mov rsp, offset boot_stack_top
    and rsp, -16
    call kernel_main

.Lhang64:
    cli
    hlt
    jmp .Lhang64
