#![allow(clippy::too_many_arguments)]

use core::ffi::{c_char, c_int, c_void};
use core::mem;

use slopos_abi::{USER_FS_MAX_ENTRIES, UserFsEntry, UserFsList, UserFsStat};

use crate::syscall::common::{
    USER_IO_MAX_BYTES, USER_PATH_MAX, syscall_bounded_from_user, syscall_copy_to_user_bounded,
    syscall_copy_user_str_to_cstr,
};

use slopos_fs::fileio::{
    file_close_fd, file_list_path, file_mkdir_path, file_open_for_process, file_read_fd,
    file_stat_path, file_unlink_path, file_write_fd,
};

use slopos_mm::kernel_heap::{kfree, kmalloc};
use slopos_mm::user_copy::{copy_bytes_to_user, copy_from_user, copy_to_user};
use slopos_mm::user_ptr::{UserBytes, UserPtr};

define_syscall!(syscall_fs_open(ctx, args, pid) requires process_id {
    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));
    let fd = file_open_for_process(pid, path.as_ptr(), args.arg1_u32());
    ctx.from_rc_value(fd as i64)
});

define_syscall!(syscall_fs_close(ctx, args, pid) requires process_id {
    ctx.from_zero_success(file_close_fd(pid, args.arg0 as c_int))
});

define_syscall!(syscall_fs_read(ctx, args, pid) requires process_id {
    require_nonzero!(ctx, args.arg1);

    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let capped_len = args.arg2_usize().min(USER_IO_MAX_BYTES);

    let bytes = file_read_fd(pid, args.arg0 as c_int, tmp.as_mut_ptr() as *mut c_char, capped_len);
    if bytes < 0 {
        return ctx.err();
    }

    try_or_err!(ctx, syscall_copy_to_user_bounded(args.arg1, &tmp[..bytes as usize]));
    ctx.ok(bytes as u64)
});

define_syscall!(syscall_fs_write(ctx, args, pid) requires process_id {
    require_nonzero!(ctx, args.arg1);

    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let write_len = try_or_err!(ctx, syscall_bounded_from_user(&mut tmp, args.arg1, args.arg2, USER_IO_MAX_BYTES));

    let bytes = file_write_fd(pid, args.arg0 as c_int, tmp.as_ptr() as *const c_char, write_len);
    ctx.from_rc_value(bytes as i64)
});

define_syscall!(syscall_fs_stat(ctx, args) {
    require_nonzero!(ctx, args.arg0);
    require_nonzero!(ctx, args.arg1);

    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));

    let mut stat = UserFsStat { type_: 0, size: 0 };
    check_result!(ctx, file_stat_path(path.as_ptr(), &mut stat.type_, &mut stat.size));

    let stat_ptr = try_or_err!(ctx, UserPtr::<UserFsStat>::try_new(args.arg1));
    try_or_err!(ctx, copy_to_user(stat_ptr, &stat));
    ctx.ok(0)
});

define_syscall!(syscall_fs_mkdir(ctx, args) {
    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));
    ctx.from_zero_success(file_mkdir_path(path.as_ptr()))
});

define_syscall!(syscall_fs_unlink(ctx, args) {
    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));
    ctx.from_zero_success(file_unlink_path(path.as_ptr()))
});

define_syscall!(syscall_fs_list(ctx, args) {
    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));
    require_nonzero!(ctx, args.arg1);

    let list_hdr_ptr = try_or_err!(ctx, UserPtr::<UserFsList>::try_new(args.arg1));
    let mut list_hdr = try_or_err!(ctx, copy_from_user(list_hdr_ptr));

    let cap = list_hdr.max_entries;
    if cap == 0 || cap > USER_FS_MAX_ENTRIES || list_hdr.entries.is_null() {
        return ctx.err();
    }

    let tmp_size = mem::size_of::<UserFsEntry>() * cap as usize;
    let tmp_ptr = kmalloc(tmp_size) as *mut UserFsEntry;
    require_nonnull!(ctx, tmp_ptr);
    unsafe { core::ptr::write_bytes(tmp_ptr as *mut u8, 0, tmp_size); }

    let mut count: u32 = 0;
    let rc = file_list_path(path.as_ptr(), tmp_ptr, cap, &mut count);
    if rc != 0 {
        kfree(tmp_ptr as *mut c_void);
        return ctx.err();
    }

    list_hdr.count = count;

    let entries_bytes = unsafe {
        core::slice::from_raw_parts(tmp_ptr as *const u8, mem::size_of::<UserFsEntry>() * count as usize)
    };
    let entries_user = match UserBytes::try_new(list_hdr.entries as u64, entries_bytes.len()) {
        Ok(b) => b,
        Err(_) => {
            kfree(tmp_ptr as *mut c_void);
            return ctx.err();
        }
    };

    let rc_entries = copy_bytes_to_user(entries_user, entries_bytes);
    let rc_hdr = if rc_entries.is_ok() {
        let hdr_ptr = match UserPtr::<UserFsList>::try_new(args.arg1) {
            Ok(p) => p,
            Err(_) => {
                kfree(tmp_ptr as *mut c_void);
                return ctx.err();
            }
        };
        copy_to_user(hdr_ptr, &list_hdr)
    } else {
        rc_entries.map(|_| ())
    };

    kfree(tmp_ptr as *mut c_void);
    ctx.from_result(rc_hdr)
});
