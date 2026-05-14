use core::ffi::c_int;

use slopos_abi::{USER_FS_MAX_ENTRIES, UserFsEntry, UserFsList, UserFsStat};

use crate::syscall::common::{USER_PATH_MAX, syscall_copy_user_str, syscall_copy_user_str_to_cstr};

use slopos_fs::fileio::{
    file_close_fd, file_list_path, file_mkdir_path, file_open_for_process, file_read_fd,
    file_stat_path, file_unlink_path, file_write_fd,
};

use slopos_mm::user_copy::{copy_bytes_to_user, copy_from_user, copy_to_user};
use slopos_mm::user_io_buf::{UserReadBuf, UserWriteBuf};
use slopos_mm::user_ptr::{UserBytes, UserPtr};
use slopos_ostd::KVec;
use slopos_ostd::util::byte_view::pod_slice_as_bytes;

/// Convert a NUL-terminated `[i8; N]` kernel buffer to a `&[u8]` covering
/// just the path bytes (everything before the first NUL).
///
/// `syscall_copy_user_str_to_cstr` produces such buffers; the conversion
/// uses the OSTD `pod_slice_as_bytes` helper so no syscall-layer code
/// needs to write `unsafe`.
fn cstr_buf_to_bytes(buf: &[i8]) -> &[u8] {
    let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let bytes = pod_slice_as_bytes(buf);
    &bytes[..nul]
}

define_syscall!(syscall_fs_open(ctx, args) requires(let pid: process_id) {
    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));
    let fd = file_open_for_process(pid, cstr_buf_to_bytes(&path), args.arg1_u32());
    ctx.from_rc_value(fd as i64)
});

define_syscall!(syscall_fs_close(ctx, args) requires(let pid: process_id) {
    ctx.from_zero_success(file_close_fd(pid, args.arg0 as c_int))
});

define_syscall!(syscall_fs_read(ctx, args) requires(let pid: process_id) {
    require_nonzero!(ctx, args.arg1);

    let count = args.arg2_usize();
    let Some(mut io_buf) = UserWriteBuf::new(args.arg1, count) else {
        return ctx.bad_address();
    };

    let bytes = file_read_fd(pid, args.arg0 as c_int, &mut io_buf);
    if bytes < 0 {
        // Propagate ERESTARTSYS (-512) directly so the
        // syscall dispatch restart logic can intercept it.
        if bytes == -512 {
            return ctx.err_with(slopos_abi::syscall::ERRNO_ERESTARTSYS);
        }
        return ctx.err();
    }

    ctx.ok(bytes as u64)
});

define_syscall!(syscall_fs_write(ctx, args) requires(let pid: process_id) {
    require_nonzero!(ctx, args.arg1);

    let count = args.arg2_usize();
    let Some(io_buf) = UserReadBuf::new(args.arg1, count) else {
        return ctx.bad_address();
    };

    let bytes = file_write_fd(pid, args.arg0 as c_int, &io_buf);
    ctx.from_rc_value(bytes as i64)
});

define_syscall!(syscall_fs_stat(ctx, args) {
    require_nonzero!(ctx, args.arg0);
    require_nonzero!(ctx, args.arg1);

    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));

    let mut stat = UserFsStat { type_: 0, size: 0 };
    check_result!(ctx, file_stat_path(cstr_buf_to_bytes(&path), &mut stat.type_, &mut stat.size));

    let stat_ptr = try_or_err!(ctx, UserPtr::<UserFsStat>::try_new(args.arg1));
    try_or_err!(ctx, copy_to_user(stat_ptr, &stat));
    ctx.ok(0)
});

define_syscall!(syscall_fs_mkdir(ctx, args) {
    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));
    ctx.from_zero_success(file_mkdir_path(cstr_buf_to_bytes(&path)))
});

define_syscall!(syscall_fs_unlink(ctx, args) {
    let mut path = [0i8; USER_PATH_MAX];
    check_result!(ctx, syscall_copy_user_str_to_cstr(&mut path, args.arg0));
    ctx.from_zero_success(file_unlink_path(cstr_buf_to_bytes(&path)))
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

    let cap_usize = cap as usize;
    let zero_entry = UserFsEntry::default();
    let mut tmp = match KVec::<UserFsEntry>::with_capacity(cap_usize) {
        Ok(v) => v,
        Err(_) => return ctx.err(),
    };
    for _ in 0..cap_usize {
        if tmp.push(zero_entry).is_err() {
            return ctx.err();
        }
    }

    let mut count: u32 = 0;
    let rc = file_list_path(cstr_buf_to_bytes(&path), tmp.as_mut_slice(), &mut count);
    if rc != 0 {
        return ctx.err();
    }

    list_hdr.count = count;

    let entries_bytes =
        slopos_ostd::util::byte_view::pod_slice_as_bytes(&tmp[..count as usize]);
    let entries_user = match UserBytes::try_new(list_hdr.entries as u64, entries_bytes.len()) {
        Ok(b) => b,
        Err(_) => return ctx.err(),
    };

    let rc_entries = copy_bytes_to_user(entries_user, entries_bytes);
    let rc_hdr = if rc_entries.is_ok() {
        let hdr_ptr = match UserPtr::<UserFsList>::try_new(args.arg1) {
            Ok(p) => p,
            Err(_) => return ctx.err(),
        };
        copy_to_user(hdr_ptr, &list_hdr)
    } else {
        rc_entries.map(|_| ())
    };

    ctx.from_result(rc_hdr)
});

define_syscall!(syscall_rename(ctx, args) {
    let old_path_ptr = args.arg0;
    let new_path_ptr = args.arg1;

    if old_path_ptr == 0 || new_path_ptr == 0 {
        return ctx.bad_address();
    }

    let mut old_path = [0u8; 256];
    if syscall_copy_user_str(&mut old_path, old_path_ptr).is_err() {
        return ctx.bad_address();
    }

    let mut new_path = [0u8; 256];
    if syscall_copy_user_str(&mut new_path, new_path_ptr).is_err() {
        return ctx.bad_address();
    }

    let old_len = old_path
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(old_path.len());
    let new_len = new_path
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(new_path.len());

    match slopos_fs::vfs::ops::vfs_rename(&old_path[..old_len], &new_path[..new_len]) {
        Ok(()) => ctx.ok(0),
        Err(_) => ctx.err(),
    }
});
