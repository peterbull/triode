BITS 64

section .data
    msg db "hello world", 0xa
    len equ $ - msg
    msg2 db "goodbye all", 0xa
    len2 equ $ - msg2
    msg3 db 100, 0xa 
    len3 equ $ - msg3

section .text
global _start

_start:
    mov rax, 1          ; write
    mov rdi, 1
    mov rsi, msg
    mov rdx, len
    syscall

    mov cx, 5
    again:
      add al, 10
      add ah, 10
      dec cx
      jnz again

    mov rax, 1 ; sys write
    mov rdi, 1 ; stdout 
    mov rsi, msg2
    mov rdx, len2
    syscall

    mov rax, 1
    mov rdi, 1
    mov rsi, msg3
    mov rdx, len3
    syscall

    mov rax, 60         ; exit
    mov rdi, 0
    syscall
