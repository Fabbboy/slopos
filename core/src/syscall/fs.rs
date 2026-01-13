#![allow(clippy::too_many_arguments)]

use core::ffi::{c_char, c_int, c_void};
use core::mem;

use slopos_abi::task::Task;
use slopos_abi::{USER_FS_MAX_ENTRIES, UserFsEntry, UserFsList, UserFsStat};
use slopos_lib::InterruptFrame;

use crate::syscall::common::{
    SyscallDisposition, USER_IO_MAX_BYTES, USER_PATH_MAX, syscall_bounded_from_user,
    syscall_copy_to_user_bounded, syscall_copy_user_str_to_cstr, syscall_return_err,
};
use crate::syscall::context::SyscallContext;

use slopos_fs::fileio::{
    file_close_fd, file_list_path, file_mkdir_path, file_open_for_process, file_read_fd,
    file_stat_path, file_unlink_path, file_write_fd,
};

use slopos_mm::kernel_heap::{kfree, kmalloc};
use slopos_mm::user_copy::{copy_bytes_to_user, copy_from_user, copy_to_user};
use slopos_mm::user_ptr::{UserBytes, UserPtr};

define_syscall!(syscall_fs_open(ctx, args, pid) requires process_id {
    let mut path = [0i8; USER_PATH_MAX];
    if syscall_copy_user_str_to_cstr(&mut path, args.arg0) != 0 {
        return ctx.err();
    }

    let flags = args.arg1_u32();
    let fd = file_open_for_process(pid, path.as_ptr(), flags);
    if fd < 0 {
        ctx.err()
    } else {
        ctx.ok(fd as u64)
    }
});

define_syscall!(syscall_fs_close(ctx, args, pid) requires process_id {
    let fd = args.arg0 as c_int;
    let rc = file_close_fd(pid, fd);
    if rc != 0 {
        ctx.err()
    } else {
        ctx.ok(0)
    }
});

define_syscall!(syscall_fs_read(ctx, args, pid) requires process_id {
    if args.arg1 == 0 {
        return ctx.err();
    }

    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let request_len = args.arg2_usize();
    let capped_len = request_len.min(USER_IO_MAX_BYTES);

    let bytes = file_read_fd(
        pid,
        args.arg0 as c_int,
        tmp.as_mut_ptr() as *mut c_char,
        capped_len,
    );
    if bytes < 0 {
        return ctx.err();
    }

    let copy_len = bytes as usize;
    if syscall_copy_to_user_bounded(args.arg1, &tmp[..copy_len]).is_err() {
        return ctx.err();
    }

    ctx.ok(bytes as u64)
});

define_syscall!(syscall_fs_write(ctx, args, pid) requires process_id {
    if args.arg1 == 0 {
        return ctx.err();
    }

    let mut tmp = [0u8; USER_IO_MAX_BYTES];
    let write_len = match syscall_bounded_from_user(
        &mut tmp,
        args.arg1,
        args.arg2,
        USER_IO_MAX_BYTES,
    ) {
        Ok(len) => len,
        Err(_) => return ctx.err(),
    };

    let bytes = file_write_fd(
        pid,
        args.arg0 as c_int,
        tmp.as_ptr() as *const c_char,
        write_len,
    );
    if bytes < 0 {
        ctx.err()
    } else {
        ctx.ok(bytes as u64)
    }
});

define_syscall!(syscall_fs_stat(ctx, args) {
    if args.arg0 == 0 || args.arg1 == 0 {
        return ctx.err();
    }

    let mut path = [0i8; USER_PATH_MAX];
    if syscall_copy_user_str_to_cstr(&mut path, args.arg0) != 0 {
        return ctx.err();
    }

    let mut stat = UserFsStat { type_: 0, size: 0 };
    if file_stat_path(path.as_ptr(), &mut stat.type_, &mut stat.size) != 0 {
        return ctx.err();
    }

    let stat_ptr = match UserPtr::<UserFsStat>::try_new(args.arg1) {
        Ok(p) => p,
        Err(_) => return ctx.err(),
    };
    if copy_to_user(stat_ptr, &stat).is_err() {
        return ctx.err();
    }

    ctx.ok(0)
});

define_syscall!(syscall_fs_mkdir(ctx, args) {
    let mut path = [0i8; USER_PATH_MAX];
    if syscall_copy_user_str_to_cstr(&mut path, args.arg0) != 0 {
        return ctx.err();
    }

    if file_mkdir_path(path.as_ptr()) == 0 {
        ctx.ok(0)
    } else {
        ctx.err()
    }
});

define_syscall!(syscall_fs_unlink(ctx, args) {
    let mut path = [0i8; USER_PATH_MAX];
    if syscall_copy_user_str_to_cstr(&mut path, args.arg0) != 0 {
        return ctx.err();
    }

    if file_unlink_path(path.as_ptr()) != 0 {
        ctx.err()
    } else {
        ctx.ok(0)
    }
});

pub fn syscall_fs_list(task: *mut Task, frame: *mut InterruptFrame) -> SyscallDisposition {
    let Some(ctx) = SyscallContext::new(task, frame) else {
        return syscall_return_err(frame, u64::MAX);
    };

    let args = ctx.args();
    let mut path = [0i8; USER_PATH_MAX];

    if syscall_copy_user_str_to_cstr(&mut path, args.arg0) != 0 || args.arg1 == 0 {
        return ctx.err();
    }

    let list_hdr_ptr = match UserPtr::<UserFsList>::try_new(args.arg1) {
        Ok(p) => p,
        Err(_) => return ctx.err(),
    };
    let mut list_hdr = match copy_from_user(list_hdr_ptr) {
        Ok(h) => h,
        Err(_) => return ctx.err(),
    };

    let cap = list_hdr.max_entries;
    if cap == 0 || cap > USER_FS_MAX_ENTRIES || list_hdr.entries.is_null() {
        return ctx.err();
    }

    let tmp_size = mem::size_of::<UserFsEntry>() * cap as usize;
    let tmp_ptr = kmalloc(tmp_size) as *mut UserFsEntry;
    if tmp_ptr.is_null() {
        return ctx.err();
    }
    unsafe {
        core::ptr::write_bytes(tmp_ptr as *mut u8, 0, tmp_size);
    }

    let mut count: u32 = 0;
    let rc = file_list_path(path.as_ptr(), tmp_ptr, cap, &mut count);
    if rc != 0 {
        kfree(tmp_ptr as *mut c_void);
        return ctx.err();
    }

    list_hdr.count = count;

    let entries_bytes = unsafe {
        core::slice::from_raw_parts(
            tmp_ptr as *const u8,
            mem::size_of::<UserFsEntry>() * count as usize,
        )
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

    if rc_hdr.is_err() {
        ctx.err()
    } else {
        ctx.ok(0)
    }
}
