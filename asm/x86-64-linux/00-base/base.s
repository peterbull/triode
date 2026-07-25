BITS 64

section .data
    msg db "Hello, world!", 0xa
    len equ $ - msg

section .text
global _start

_start:
    mov rax, 1          ; write
    mov rdi, 1
    mov rsi, msg
    mov rdx, len
    syscall

    mov rax, 60         ; exit
    mov rdi, 0
    syscall
