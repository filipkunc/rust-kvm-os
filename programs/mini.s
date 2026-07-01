.global _start
.text
_start:
    mov $1, %rax          # write
    mov $1, %rdi
    lea msg(%rip), %rsi
    mov $5, %rdx
    syscall
    mov $231, %rax        # exit_group
    mov $7, %rdi
    syscall
.section .rodata
msg: .ascii "tiny\n"
