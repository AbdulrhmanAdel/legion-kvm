.text
.hidden LEGION_WINE_ORIGINAL
.hidden legion_wine_route
.global legion_wine_entry
.hidden legion_wine_entry
.type legion_wine_entry,@function
.macro restore_frame
    movdqu xmm0, [rsp+112]
    movdqu xmm1, [rsp+128]
    movdqu xmm2, [rsp+144]
    movdqu xmm3, [rsp+160]
    movdqu xmm4, [rsp+176]
    movdqu xmm5, [rsp+192]
    movdqu xmm6, [rsp+208]
    movdqu xmm7, [rsp+224]
    movdqu xmm8, [rsp+240]
    movdqu xmm9, [rsp+256]
    movdqu xmm10, [rsp+272]
    movdqu xmm11, [rsp+288]
    movdqu xmm12, [rsp+304]
    movdqu xmm13, [rsp+320]
    movdqu xmm14, [rsp+336]
    movdqu xmm15, [rsp+352]
    push qword ptr [rsp+368]
    popfq
    mov rax,[rsp]
    mov rbx,[rsp+8]
    mov rcx,[rsp+16]
    mov rdx,[rsp+24]
    mov rsi,[rsp+32]
    mov rdi,[rsp+40]
    mov r8,[rsp+48]
    mov r9,[rsp+56]
    mov r10,[rsp+64]
    mov r11,[rsp+72]
    mov r12,[rsp+80]
    mov r13,[rsp+88]
    mov r14,[rsp+96]
    mov r15,[rsp+104]
    mov rsp,rbp
    pop rbp
    lea rsp,[rsp+8]
.endm
legion_wine_entry:
    endbr64
    pushfq
    push rbp
    mov rbp,rsp
    sub rsp,416
    and rsp,-16
    mov [rsp],rax
    mov [rsp+8],rbx
    mov [rsp+16],rcx
    mov [rsp+24],rdx
    mov [rsp+32],rsi
    mov [rsp+40],rdi
    mov [rsp+48],r8
    mov [rsp+56],r9
    mov [rsp+64],r10
    mov [rsp+72],r11
    mov [rsp+80],r12
    mov [rsp+88],r13
    mov [rsp+96],r14
    mov [rsp+104],r15
    movdqu [rsp+112],xmm0
    movdqu [rsp+128],xmm1
    movdqu [rsp+144],xmm2
    movdqu [rsp+160],xmm3
    movdqu [rsp+176],xmm4
    movdqu [rsp+192],xmm5
    movdqu [rsp+208],xmm6
    movdqu [rsp+224],xmm7
    movdqu [rsp+240],xmm8
    movdqu [rsp+256],xmm9
    movdqu [rsp+272],xmm10
    movdqu [rsp+288],xmm11
    movdqu [rsp+304],xmm12
    movdqu [rsp+320],xmm13
    movdqu [rsp+336],xmm14
    movdqu [rsp+352],xmm15
    mov rax,[rbp+8]
    mov [rsp+368],rax
    mov rax,[rbp+16]
    mov [rsp+376],rax
    lea rax,[rbp+16]
    mov [rsp+384],rax
    mov qword ptr [rsp+392],0
    mov qword ptr [rsp+400],0
    mov rdi,rsp
    call legion_wine_route
    cmp qword ptr [rsp+400],0
    jne .Lwine_redirect
    restore_frame
    jmp qword ptr [rip+LEGION_WINE_ORIGINAL]
.Lwine_redirect:
    restore_frame
    lea rsp,[rsp+8]
    jmp rcx
.size legion_wine_entry,.-legion_wine_entry
.section .note.GNU-stack,"",@progbits
