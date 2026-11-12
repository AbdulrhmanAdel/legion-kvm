.text
.globl legion_nt_raw_case
.def legion_nt_raw_case; .scl 2; .type 32; .endef
.seh_proc legion_nt_raw_case
legion_nt_raw_case:
    push rbx
    .seh_pushreg rbx
    .seh_endprologue
    mov rbx, rcx
    movdqu xmm4, [rbx + 32]
    movdqu xmm5, [rbx + 16]
    lea rax, [rip + .Lnt_return]
    mov [rbx + 48], rax
    mov [rbx + 56], rsp
    mov r10, rbx
    mov rax, [rbx + 8]
    pushfq
    pop qword ptr [rbx + 176]
    syscall
.Lnt_return:
    mov [rbx + 64], rax
    movdqu [rbx + 72], xmm4
    movdqu [rbx + 88], xmm5
    pop rbx
    ret
.globl legion_nt_raw_end
legion_nt_raw_end:
.seh_endproc

.globl legion_nt_stub_case
.def legion_nt_stub_case; .scl 2; .type 32; .endef
.seh_proc legion_nt_stub_case
legion_nt_stub_case:
    push rbx
    .seh_pushreg rbx
    push rdi
    .seh_pushreg rdi
    push rsi
    .seh_pushreg rsi
    sub rsp,96
    .seh_stackalloc 96
    movdqu [rsp+32],xmm6
    .seh_savexmm xmm6,32
    movdqu [rsp+48],xmm15
    .seh_savexmm xmm15,48
    .seh_endprologue
    mov rbx,rcx
    movdqu xmm6,[rbx+192]
    movdqu xmm15,[rbx+224]
    mov rdi,[rbx+256]
    mov rsi,[rbx+264]
    movdqu xmm4,[rbx+32]
    movdqu xmm5,[rbx+16]
    lea rax,[rsp-8]
    mov [rbx+56],rax
    mov rcx,rbx
    test byte ptr [0x7ffe0308],1
    pushfq
    pop qword ptr [rbx+176]
    call qword ptr [rbx+184]
    mov [rbx+64],rax
    movdqu [rbx+72],xmm4
    movdqu [rbx+88],xmm5
    movdqu [rbx+208],xmm6
    movdqu [rbx+240],xmm15
    mov [rbx+272],rdi
    mov [rbx+280],rsi
    movdqu xmm6,[rsp+32]
    movdqu xmm15,[rsp+48]
    add rsp,96
    pop rsi
    pop rdi
    pop rbx
    ret
.seh_endproc

.globl legion_nt_capture
.def legion_nt_capture; .scl 2; .type 32; .endef
legion_nt_capture:
    cmp qword ptr [rip + LEGION_NT_ACTIVE_CASE],0
    je .Lnt_unexpected
    cmp r10, qword ptr [rip + LEGION_NT_ACTIVE_CASE]
    jne .Lnt_unexpected
    mov [r10 + 104], rax
    mov [r10 + 112], rcx
    mov [r10 + 120], r11
    mov [r10 + 128], rsp
    movdqu [r10 + 136], xmm4
    movdqu [r10 + 152], xmm5
    mov qword ptr [r10 + 168], 1
    mov eax, 0x5a5a5a5a
    jmp qword ptr [r10 + 104]
.Lnt_unexpected:
    cmp qword ptr [rip+LEGION_NT_FORWARD_OTHER],0
    je .Lnt_reject
    cmp word ptr [rax-2],0x050f
    jne .Lnt_reject
    lock inc qword ptr [rip+LEGION_NT_OTHER_COUNT]
    lea rcx,[rax-2]
    movd eax,xmm4
    movq xmm5,qword ptr [rip+LEGION_NT_BYPASS]
    push r11
    popfq
    jmp rcx
.Lnt_reject:
    lock inc qword ptr [rip + LEGION_NT_UNEXPECTED]
    mov rcx, rax
    mov eax, 0xc0000002
    jmp rcx

.globl legion_nt_wrong
.def legion_nt_wrong; .scl 2; .type 32; .endef
legion_nt_wrong:
    cmp qword ptr [rip + LEGION_NT_ACTIVE_CASE],0
    je .Lnt_unexpected
    cmp r10, qword ptr [rip + LEGION_NT_ACTIVE_CASE]
    jne .Lnt_unexpected
    mov [r10 + 104], rax
    mov eax, 0xbad
    jmp qword ptr [r10 + 104]
