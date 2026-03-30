use slopos_abi::KernelErrno;
use slopos_abi::damage::{DamageRect, MAX_DAMAGE_REGIONS};
use slopos_abi::fate::FateResult;
use slopos_abi::syscall::{ERRNO_ERESTARTSYS, TtyIndex};
use slopos_abi::{DisplayInfo, InputEvent, WindowInfo};

use crate::fate_api::{fate_apply_outcome, fate_set_pending, fate_spin, fate_take_pending};
use slopos_fs::fileio::file_open_tty_fd;
use slopos_kernel_services::platform;
use slopos_kernel_services::syscall_services::{input, tty, video};

use slopos_mm::paging::{paging_get_kernel_directory, switch_page_directory};
use slopos_mm::process_vm::process_vm_get_page_dir;
use slopos_mm::user_copy::{copy_bytes_from_user, copy_bytes_to_user, copy_to_user};
use slopos_mm::user_ptr::{UserBytes, UserPtr};

define_syscall!(syscall_random_next(ctx, args) {
    let _ = args;
    let value = platform::rng_next();
    ctx.ok(value)
});

define_syscall!(syscall_shm_get_formats(ctx, args) {
    let _ = args;
    let formats = slopos_mm::shared_memory::shm_get_formats();
    ctx.ok(formats as u64)
});

define_syscall!(syscall_shm_poll_released(ctx, args) {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_poll_released(token);
    ctx.ok(result as u64)
});

define_syscall!(syscall_shm_create(ctx, args) requires(let process_id) {
    let size = args.arg0;
    let flags = args.arg1_u32();
    ctx.from_token(slopos_mm::shared_memory::shm_create(process_id, size, flags))
});

define_syscall!(syscall_shm_map(ctx, args) requires(let process_id) {
    let token = args.arg0_u32();
    let access_val = args.arg1_u32();
    let access = some_or_err!(ctx, slopos_mm::shared_memory::ShmAccess::from_u32(access_val));
    ctx.from_nonzero(slopos_mm::shared_memory::shm_map(process_id, token, access))
});

define_syscall!(syscall_shm_unmap(ctx, args) requires(let process_id) {
    let vaddr = args.arg0;
    let result = slopos_mm::shared_memory::shm_unmap(process_id, vaddr);
    check_result!(ctx, result);
    ctx.ok(0)
});

define_syscall!(syscall_shm_destroy(ctx, args) requires(let process_id) {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_destroy(process_id, token);
    check_result!(ctx, result);
    ctx.ok(0)
});

define_syscall!(syscall_shm_create_with_format(ctx, args) requires(let task_id) {
    let size = args.arg0;
    let format_val = args.arg1_u32();
    let format = some_or_err!(ctx, slopos_mm::shared_memory::PixelFormat::from_u32(format_val));
    ctx.from_token(slopos_mm::shared_memory::shm_create_with_format(task_id, size, format))
});

define_syscall!(syscall_input_poll_batch(ctx, args) requires(let task_id) {
    let buffer_ptr = args.arg0_ptr::<InputEvent>();
    let max_count = args.arg1_usize();

    if buffer_ptr.is_null() || max_count == 0 {
        return ctx.ok(0);
    }

    if ctx.is_compositor() {
        // Register the compositor so all raw input routes to its queue
        input::register_compositor(task_id);
        if input::get_pointer_focus() == 0 {
            input::set_pointer_focus(task_id, 0);
        }
    }

    ctx.ok(input::drain_batch(task_id, buffer_ptr, max_count) as u64)
});

define_syscall!(syscall_clipboard_copy(ctx, args) requires(let task_id) {
    let _ = task_id;
    let src_ptr = args.arg0;
    let src_len = args.arg1_usize();

    if src_ptr == 0 || src_len == 0 {
        return ctx.ok(0);
    }

    let copy_len = src_len.min(slopos_abi::CLIPBOARD_MAX_SIZE);
    let user_bytes = try_or_err!(ctx, UserBytes::try_new(src_ptr, copy_len));
    let mut buf = [0u8; slopos_abi::CLIPBOARD_MAX_SIZE];
    try_or_err!(ctx, copy_bytes_from_user(user_bytes, &mut buf[..copy_len]));
    let stored = input::clipboard_copy(&buf[..copy_len]);
    ctx.ok(stored as u64)
});

define_syscall!(syscall_clipboard_paste(ctx, args) requires(let task_id) {
    let _ = task_id;
    let dst_ptr = args.arg0;
    let max_len = args.arg1_usize();

    if dst_ptr == 0 || max_len == 0 {
        return ctx.ok(0);
    }

    let mut buf = [0u8; slopos_abi::CLIPBOARD_MAX_SIZE];
    let pasted = input::clipboard_paste(&mut buf);
    if pasted == 0 {
        return ctx.ok(0);
    }

    let write_len = pasted.min(max_len);
    let user_ptr = try_or_err!(ctx, UserBytes::try_new(dst_ptr, write_len));
    try_or_err!(ctx, copy_bytes_to_user(user_ptr, &buf[..write_len]));
    ctx.ok(write_len as u64)
});

define_syscall!(syscall_openpty(ctx, args) {
    let master_out = try_or_err!(ctx, UserPtr::<u32>::try_new(args.arg0));
    let slave_out = try_or_err!(ctx, UserPtr::<u32>::try_new(args.arg1));

    let master_idx = match tty::alloc_pty() {
        Ok(idx) => idx,
        Err(e) => return ctx.ok_i64(e.to_errno() as i64),
    };

    let slave_num = match tty::get_pty_number(master_idx) {
        Ok(n) => n,
        Err(e) => return ctx.ok_i64(e.to_errno() as i64),
    };

    if let Err(e) = tty::grantpt(master_idx) {
        return ctx.ok_i64(e.to_errno() as i64);
    }

    try_or_err!(ctx, copy_to_user(master_out, &(master_idx.0 as u32)));
    try_or_err!(ctx, copy_to_user(slave_out, &slave_num));
    ctx.ok(0)
});

define_syscall!(syscall_tty_read(ctx, args) {
    let tty_idx = TtyIndex(args.arg0 as u8);
    let user_ptr = args.arg1;
    let max_len = args.arg2_usize();

    if user_ptr == 0 || max_len == 0 {
        return ctx.ok(0);
    }

    const MAX_COPY: usize = 512;
    let mut scratch = [0u8; MAX_COPY];
    let read_len = max_len.min(MAX_COPY);

    match tty::read_cooked(tty_idx, scratch.as_mut_ptr(), read_len, true) {
        Ok(n) => {
            let user_bytes = try_or_err!(ctx, UserBytes::try_new(user_ptr, n));
            try_or_err!(ctx, copy_bytes_to_user(user_bytes, &scratch[..n]));
            ctx.ok(n as u64)
        }
        Err(e) => {
            let errno = e.to_errno() as i64;
            if errno == -512 {
                ctx.err_with(ERRNO_ERESTARTSYS)
            } else {
                ctx.ok_i64(errno)
            }
        }
    }
});

define_syscall!(syscall_tty_write(ctx, args) {
    let tty_idx = TtyIndex(args.arg0 as u8);
    let user_ptr = args.arg1;
    let len = args.arg2_usize();

    if user_ptr == 0 || len == 0 {
        return ctx.ok(0);
    }

    const MAX_COPY: usize = 512;
    let mut scratch = [0u8; MAX_COPY];
    let write_len = len.min(MAX_COPY);
    let user_bytes = try_or_err!(ctx, UserBytes::try_new(user_ptr, write_len));
    try_or_err!(ctx, copy_bytes_from_user(user_bytes, &mut scratch[..write_len]));

    match tty::write_bytes(tty_idx, scratch.as_ptr(), write_len, true) {
        Ok(n) => ctx.ok(n as u64),
        Err(e) => {
            let errno = e.to_errno() as i64;
            if errno == -512 {
                ctx.err_with(ERRNO_ERESTARTSYS)
            } else {
                ctx.ok_i64(errno)
            }
        }
    }
});

// Open a file descriptor pointing to a TTY by its kernel index.
// Increments the TTY's open_count first (matching the VFS open path).
define_syscall!(syscall_open_tty_fd(ctx, args) requires(let pid: process_id) {
    let tty_idx = TtyIndex(args.arg0 as u8);
    if tty::open_ref(tty_idx).is_err() {
        return ctx.err();
    }
    let fd = file_open_tty_fd(pid, tty_idx, 0);
    if fd < 0 {
        let _ = tty::close_ref(tty_idx);
        ctx.ok_i64(fd as i64)
    } else {
        ctx.ok(fd as u64)
    }
});

define_syscall!(syscall_enumerate_windows(ctx, args) requires(compositor) {
    let out_buffer = args.arg0_ptr::<WindowInfo>();
    let max_count = args.arg1_u32();
    require_nonnull!(ctx, out_buffer);
    require_nonzero!(ctx, max_count);
    ctx.ok(video::surface_enumerate_windows(out_buffer, max_count) as u64)
});

define_syscall!(syscall_fb_flip(ctx, args) requires(compositor) {
    let token = args.arg0_u32();
    let damage_ptr = args.arg1;
    let damage_count = args.arg2_usize();
    let phys_addr = slopos_mm::shared_memory::shm_get_phys_addr(token);
    let size = slopos_mm::shared_memory::shm_get_size(token);
    if phys_addr.is_null() || size == 0 {
        return ctx.err();
    }

    let mut damage_regions = [DamageRect::invalid(); MAX_DAMAGE_REGIONS];
    let mut damage_region_count = 0u32;
    if damage_ptr != 0 && damage_count > 0 {
        let clamped = damage_count.min(MAX_DAMAGE_REGIONS);
        let byte_len = core::mem::size_of::<DamageRect>() * clamped;
        let user_bytes = match UserBytes::try_new(damage_ptr, byte_len) {
            Ok(ptr) => ptr,
            Err(_) => return ctx.err(),
        };
        let dst = &mut damage_regions[..clamped];
        let dst_bytes = unsafe {
            core::slice::from_raw_parts_mut(dst.as_mut_ptr() as *mut u8, byte_len)
        };
        if copy_bytes_from_user(user_bytes, dst_bytes).is_err() {
            return ctx.err();
        }
        damage_region_count = clamped as u32;
    }

    some_or_err!(ctx, video::get_display_info());
    let damage_ptr = if damage_region_count > 0 {
        damage_regions.as_ptr()
    } else {
        core::ptr::null()
    };
    check_result!(ctx, video::fb_flip_from_shm(
        phys_addr,
        size,
        damage_ptr,
        damage_region_count,
    ));
    ctx.ok(0)
});

define_syscall!(syscall_shm_acquire(ctx, args) requires(compositor) {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_acquire(token);
    ctx.ok(result as u64)
});

define_syscall!(syscall_shm_release(ctx, args) requires(compositor) {
    let token = args.arg0_u32();
    let result = slopos_mm::shared_memory::shm_release(token);
    ctx.ok(result as u64)
});

define_syscall!(syscall_roulette_draw(ctx, args) requires(display_exclusive) {
    let fate = args.arg0_u32();
    let caller_dir = match ctx.process_id() {
        Some(pid) => {
            let dir = process_vm_get_page_dir(pid);
            if dir.is_null() {
                core::ptr::null_mut()
            } else {
                dir
            }
        }
        None => core::ptr::null_mut(),
    };
    let kernel_dir = paging_get_kernel_directory();
    if !kernel_dir.is_null() {
        let _ = switch_page_directory(kernel_dir);
    }
    let disp = ctx.from_result(video::roulette_draw(fate));
    if !caller_dir.is_null() {
        let _ = switch_page_directory(caller_dir);
    }
    disp
});

define_syscall!(syscall_roulette_spin(ctx, args) requires(let task_id) {
    let _ = args;
    let res = fate_spin();
    check_result!(ctx, fate_set_pending(res, task_id));
    let packed = ((res.token as u64) << 32) | res.value as u64;
    ctx.ok(packed)
});

define_syscall!(syscall_roulette_result(ctx, args) requires(let task_id) {
    let mut stored = FateResult { token: 0, value: 0 };
    check_result!(ctx, fate_take_pending(task_id, &mut stored));

    let token = (args.arg0 >> 32) as u32;
    if token != stored.token {
        return ctx.err();
    }

    let is_win = (stored.value & 1) == 1;

    if is_win {
        fate_apply_outcome(&stored as *const FateResult, 0, true);
        ctx.ok(0)
    } else {
        fate_apply_outcome(&stored as *const FateResult, 0, false);
        platform::kernel_reboot(b"Roulette loss - spinning again\0".as_ptr() as *const i8);
    }
});

define_syscall!(syscall_fb_info(ctx, args) {
    let display_info = some_or_err!(ctx, video::get_display_info());
    let user_ptr = try_or_err!(ctx, UserPtr::<DisplayInfo>::try_new(args.arg0));
    try_or_err!(ctx, copy_to_user(user_ptr, &display_info));
    ctx.ok(0)
});
