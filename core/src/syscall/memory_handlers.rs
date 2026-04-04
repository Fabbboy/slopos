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

    // If fd >= 0, resolve it to a memfd handle for shared mappings.
    if fd >= 0 {
        let Some((kind, handle)) =
            slopos_fs::fileio::fileio_get_open_file_handle(process_id, fd as i32)
        else {
            return ctx.err_with(slopos_abi::Errno::EBADF.as_u64());
        };
        if kind != slopos_abi::file_ops::FileKind::Memfd {
            return ctx.err_with(slopos_abi::Errno::EINVAL.as_u64());
        }
        let result = slopos_mm::process_vm::process_vm_mmap_shared(
            process_id,
            addr,
            length,
            prot,
            flags,
            offset,
            slopos_mm::memfd::MemfdHandle::from_usize(handle),
        );
        return ctx.from_nonzero(result);
    }

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

define_syscall!(syscall_memfd_create(ctx, args) requires(let process_id) {
    let flags = args.arg0 as u32;
    let Some((handle, ops)) = slopos_mm::memfd::memfd_create(flags) else {
        return ctx.err_with(slopos_abi::Errno::ENOMEM.as_u64());
    };
    let fd = slopos_fs::fileio::fileio_open_fd_with_ops(process_id, ops, handle);
    if fd < 0 {
        // fileio_open_fd_with_ops already called ops.release on failure
        return ctx.err_with((-fd) as u64);
    }
    ctx.ok(fd as u64)
});

define_syscall!(syscall_ftruncate(ctx, args) requires(let process_id) {
    let fd = args.arg0 as i32;
    let size = args.arg1 as usize;

    let Some((kind, handle)) = slopos_fs::fileio::fileio_get_open_file_handle(process_id, fd) else {
        return ctx.err_with(slopos_abi::Errno::EBADF.as_u64());
    };
    if kind != slopos_abi::file_ops::FileKind::Memfd {
        return ctx.err_with(slopos_abi::Errno::EINVAL.as_u64());
    }

    let rc = slopos_mm::memfd::memfd_ftruncate(handle, size);
    ctx.from_rc(rc)
});
