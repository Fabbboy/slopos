define_syscall!(syscall_brk(ctx, args) requires(let process_id) {
    let new_brk = args.arg0;
    let result = slopos_mm::process_vm::process_vm_brk(process_id, new_brk);
    if result == 0 && new_brk != 0 {
        ctx.err_with(slopos_abi::syscall::ERRNO_ENOMEM)
    } else {
        ctx.ok(result)
    }
});

define_syscall!(syscall_mmap(ctx, args) requires(let process_id) {
    let addr = args.arg0;
    let length = args.arg1;
    let prot = args.arg2;
    let flags = args.arg3;
    let fd = args.arg4 as i64;
    let offset = args.arg5;
    let result = slopos_mm::process_vm::process_vm_mmap(
        process_id, addr, length, prot, flags, fd, offset,
    );
    ctx.from_nonzero(result)
});

define_syscall!(syscall_munmap(ctx, args) requires(let process_id) {
    let addr = args.arg0;
    let length = args.arg1;
    let rc = slopos_mm::process_vm::process_vm_munmap(process_id, addr, length);
    ctx.from_rc(rc)
});

define_syscall!(syscall_mprotect(ctx, args) requires(let process_id) {
    let addr = args.arg0;
    let length = args.arg1;
    let prot = args.arg2;
    let rc = slopos_mm::process_vm::process_vm_mprotect(process_id, addr, length, prot);
    ctx.from_rc(rc)
});
