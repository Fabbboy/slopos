use slopos_abi::Errno;
use slopos_abi::KernelErrno;
use slopos_abi::damage::{DamageRect, MAX_DAMAGE_REGIONS};
use slopos_abi::fate::FateResult;
use slopos_abi::syscall::TtyIndex;
use slopos_abi::{DisplayInfo, InputEvent};

use slopos_fs::fileio::file_open_tty_fd;
use slopos_kernel_services::platform;
use slopos_kernel_services::syscall_services::{input, tty, video};
use slopos_sched::fate_api::{fate_apply_outcome, fate_set_pending, fate_spin, fate_take_pending};

use slopos_mm::user_copy::{copy_bytes_from_user, copy_bytes_to_user, copy_to_user};
use slopos_mm::user_ptr::{UserBytes as MmUserBytes, UserPtr as MmUserPtr};

use crate::syscall::args::{UserBytes, UserPtr};
use crate::syscall::result::SyscallResult;

define_syscall!(syscall_getrandom
    (ctx, buf: UserBytes, _flags: u32) -> Result<u64, Errno>
{
    if buf.base_u64() == 0 || buf.len() == 0 {
        return Ok(0);
    }

    // Cap at 256 bytes per call to limit IRQ-mutex hold time.
    let len = buf.len().min(256);
    let mut scratch = [0u8; 256];

    let mut pos = 0;
    while pos < len {
        let val = platform::rng_next();
        let bytes = val.to_le_bytes();
        let chunk = (len - pos).min(8);
        scratch[pos..pos + chunk].copy_from_slice(&bytes[..chunk]);
        pos += chunk;
    }

    let user_out = MmUserBytes::try_new(buf.base_u64(), len).map_err(|_| Errno::EFAULT)?;
    copy_bytes_to_user(user_out, &scratch[..len]).map_err(|_| Errno::EFAULT)?;
    Ok(len as u64)
});

define_syscall!(syscall_input_poll_batch
    (ctx, events_out: UserPtr<u8>, max_count: u64)
    requires(task_id: task_id)
    -> Result<u64, Errno>
{
    if events_out.as_u64() == 0 || max_count == 0 {
        return Ok(0);
    }
    let max_count = max_count as usize;

    if ctx.is_compositor() {
        input::register_compositor(task_id);
        if input::get_pointer_focus() == 0 {
            input::set_pointer_focus(task_id, 0);
        }
    }

    #[allow(dead_code)]
    #[derive(slopos_ostd::Zeroable, slopos_ostd::Pod, Copy, Clone)]
    #[repr(C, align(8))]
    struct InputEventScratch([u8; core::mem::size_of::<InputEvent>()]);

    const _: () = assert!(
        core::mem::align_of::<InputEvent>() <= 8,
        "InputEventScratch must be aligned for InputEvent",
    );

    const MAX_BATCH: usize = 64;
    let batch = max_count.min(MAX_BATCH);
    let mut scratch = slopos_ostd::KVec::<InputEventScratch>::zeroed(batch)
        .map_err(|_| Errno::ENOMEM)?;

    let count = input::drain_batch(
        task_id,
        scratch.as_mut_ptr() as *mut InputEvent,
        batch,
    );
    if count > 0 {
        let src_bytes = slopos_ostd::util::byte_view::pod_slice_as_bytes(&scratch[..count]);
        let user_out = MmUserBytes::try_new(events_out.as_u64(), src_bytes.len())
            .map_err(|_| Errno::EFAULT)?;
        copy_bytes_to_user(user_out, src_bytes).map_err(|_| Errno::EFAULT)?;
    }
    Ok(count as u64)
});

define_syscall!(syscall_clipboard_copy
    (ctx, src: UserBytes)
    requires(task_id: task_id)
    -> Result<u64, Errno>
{
    let _ = task_id;
    if src.base_u64() == 0 || src.len() == 0 {
        return Ok(0);
    }

    let copy_len = src.len().min(slopos_abi::CLIPBOARD_MAX_SIZE);
    let user_bytes = MmUserBytes::try_new(src.base_u64(), copy_len).map_err(|_| Errno::EFAULT)?;
    let mut buf = slopos_ostd::KVec::<u8>::zeroed(slopos_abi::CLIPBOARD_MAX_SIZE)
        .map_err(|_| Errno::ENOMEM)?;
    copy_bytes_from_user(user_bytes, &mut buf[..copy_len]).map_err(|_| Errno::EFAULT)?;
    let stored = input::clipboard_copy(&buf[..copy_len]);
    Ok(stored as u64)
});

define_syscall!(syscall_clipboard_paste
    (ctx, dst: UserBytes)
    requires(task_id: task_id)
    -> Result<u64, Errno>
{
    let _ = task_id;
    if dst.base_u64() == 0 || dst.len() == 0 {
        return Ok(0);
    }

    let mut buf = slopos_ostd::KVec::<u8>::zeroed(slopos_abi::CLIPBOARD_MAX_SIZE)
        .map_err(|_| Errno::ENOMEM)?;
    let pasted = input::clipboard_paste(&mut buf);
    if pasted == 0 {
        return Ok(0);
    }

    let write_len = pasted.min(dst.len());
    let user_ptr = MmUserBytes::try_new(dst.base_u64(), write_len).map_err(|_| Errno::EFAULT)?;
    copy_bytes_to_user(user_ptr, &buf[..write_len]).map_err(|_| Errno::EFAULT)?;
    Ok(write_len as u64)
});

define_syscall!(syscall_openpty
    (ctx, master_out: UserPtr<u32>, slave_out: UserPtr<u32>) -> Result<(), Errno>
{
    let master_idx = match tty::alloc_pty() {
        Ok(idx) => idx,
        Err(e) => return Err(Errno::from_raw(e.to_errno()).unwrap_or(Errno::EINVAL)),
    };

    let slave_num = match tty::get_pty_number(master_idx) {
        Ok(n) => n,
        Err(e) => return Err(Errno::from_raw(e.to_errno()).unwrap_or(Errno::EINVAL)),
    };

    if let Err(e) = tty::grantpt(master_idx) {
        return Err(Errno::from_raw(e.to_errno()).unwrap_or(Errno::EINVAL));
    }

    copy_to_user(master_out.inner(), &(master_idx.0 as u32)).map_err(|_| Errno::EFAULT)?;
    copy_to_user(slave_out.inner(), &slave_num).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_tty_read
    (ctx, tty_idx: u8, dst: UserBytes) -> Result<u64, Errno>
{
    let tty_idx = TtyIndex(tty_idx);
    if dst.base_u64() == 0 || dst.len() == 0 {
        return Ok(0);
    }

    const MAX_COPY: usize = 512;
    let mut scratch = [0u8; MAX_COPY];
    let read_len = dst.len().min(MAX_COPY);

    match tty::read_cooked(tty_idx, scratch.as_mut_ptr(), read_len, true) {
        Ok(n) => {
            let user_bytes = MmUserBytes::try_new(dst.base_u64(), n).map_err(|_| Errno::EFAULT)?;
            copy_bytes_to_user(user_bytes, &scratch[..n]).map_err(|_| Errno::EFAULT)?;
            Ok(n as u64)
        }
        Err(e) => {
            let errno = e.to_errno();
            if errno == -512 {
                Err(Errno::ERESTARTSYS)
            } else {
                Err(Errno::from_raw(errno).unwrap_or(Errno::EINVAL))
            }
        }
    }
});

define_syscall!(syscall_tty_write
    (ctx, tty_idx: u8, src: UserBytes) -> Result<u64, Errno>
{
    let tty_idx = TtyIndex(tty_idx);
    if src.base_u64() == 0 || src.len() == 0 {
        return Ok(0);
    }

    const MAX_COPY: usize = 512;
    let mut scratch = [0u8; MAX_COPY];
    let write_len = src.len().min(MAX_COPY);
    let user_bytes = MmUserBytes::try_new(src.base_u64(), write_len).map_err(|_| Errno::EFAULT)?;
    copy_bytes_from_user(user_bytes, &mut scratch[..write_len]).map_err(|_| Errno::EFAULT)?;

    match tty::write_bytes(tty_idx, scratch.as_ptr(), write_len, true) {
        Ok(n) => Ok(n as u64),
        Err(e) => {
            let errno = e.to_errno();
            if errno == -512 {
                Err(Errno::ERESTARTSYS)
            } else {
                Err(Errno::from_raw(errno).unwrap_or(Errno::EINVAL))
            }
        }
    }
});

define_syscall!(syscall_open_tty_fd
    (ctx, tty_idx: u8)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    let tty_idx = TtyIndex(tty_idx);
    if tty::open_ref(tty_idx).is_err() {
        return Err(Errno::EINVAL);
    }
    // `file_open_tty_fd` transfers ownership of the `open_ref` minted
    // above into the new `OpenFile`; on failure the install already
    // released it exactly once, so the error arm must NOT close_ref again
    // (a second decrement is the premature-close root cause).
    let fd = file_open_tty_fd(pid, tty_idx, 0);
    if fd < 0 {
        Err(Errno::from_raw(fd).unwrap_or(Errno::EINVAL))
    } else {
        Ok(fd as u64)
    }
});

define_syscall!(syscall_fb_flip
    (ctx, fd: i64, damage_ptr: u64, damage_count: u64)
    requires(compositor)
    -> Result<(), Errno>
{
    let fd = fd as i32;
    let damage_count = damage_count as usize;

    let process_id = ctx.process_id().ok_or(Errno::ESRCH)?;
    let (kind, handle) = slopos_fs::fileio::fileio_get_open_file_handle(process_id, fd)
        .ok_or(Errno::EBADF)?;
    if kind != slopos_abi::file_ops::FileKind::Memfd {
        return Err(Errno::EINVAL);
    }
    let (phys_addr, size) = slopos_mm::memfd::memfd_get_phys(handle);
    if phys_addr.is_null() || size == 0 {
        return Err(Errno::EINVAL);
    }

    let mut damage_regions = [DamageRect::invalid(); MAX_DAMAGE_REGIONS];
    let mut damage_region_count = 0u32;
    if damage_ptr != 0 && damage_count > 0 {
        let clamped = damage_count.min(MAX_DAMAGE_REGIONS);
        let byte_len = core::mem::size_of::<DamageRect>() * clamped;
        let user_bytes = MmUserBytes::try_new(damage_ptr, byte_len).map_err(|_| Errno::EFAULT)?;
        let dst = &mut damage_regions[..clamped];
        let dst_bytes = slopos_ostd::util::byte_view::pod_slice_as_bytes_mut(dst);
        debug_assert_eq!(dst_bytes.len(), byte_len);
        copy_bytes_from_user(user_bytes, dst_bytes).map_err(|_| Errno::EFAULT)?;
        damage_region_count = clamped as u32;
    }

    video::get_display_info().ok_or(Errno::EINVAL)?;
    let damage_ptr_ffi = if damage_region_count > 0 {
        damage_regions.as_ptr()
    } else {
        core::ptr::null()
    };
    let rc = video::fb_flip_from_shm(phys_addr, size, damage_ptr_ffi, damage_region_count);
    if rc != 0 {
        return Err(Errno::EINVAL);
    }
    video::set_compositor_task_id(ctx.task_id().unwrap_or(0));
    Ok(())
});

define_syscall!(syscall_roulette_draw
    (ctx, fate: u32)
    requires(display_exclusive)
    -> Result<(), Errno>
{
    use slopos_kernel_services::kernel_vm_space::kernel_vm_space;
    let caller_pid = ctx.process_id();
    kernel_vm_space().lock().activate_kernel_master();
    let result = video::roulette_draw(fate);
    if let Some(pid) = caller_pid {
        let _ = slopos_mm::process_vm::process_vm_activate(pid);
    }
    result.map_err(|_| Errno::EINVAL)
});

define_syscall!(syscall_roulette_spin (ctx)
    requires(task_id: task_id)
    -> Result<u64, Errno>
{
    let res = fate_spin();
    if fate_set_pending(res, task_id) != 0 {
        return Err(Errno::EINVAL);
    }
    let packed = ((res.token as u64) << 32) | res.value as u64;
    Ok(packed)
});

define_syscall!(syscall_roulette_result
    (ctx, packed: u64)
    requires(task_id: task_id)
    -> SyscallResult
{
    let mut stored = FateResult { token: 0, value: 0 };
    if fate_take_pending(task_id, &mut stored) != 0 {
        return SyscallResult::Err(Errno::EINVAL);
    }

    let token = (packed >> 32) as u32;
    if token != stored.token {
        return SyscallResult::Err(Errno::EINVAL);
    }

    let is_win = (stored.value & 1) == 1;

    if is_win {
        fate_apply_outcome(&stored as *const FateResult, 0, true);
        SyscallResult::Ok(0)
    } else {
        fate_apply_outcome(&stored as *const FateResult, 0, false);
        platform::kernel_reboot(b"Roulette loss - spinning again\0".as_ptr() as *const i8);
        #[allow(unreachable_code)]
        SyscallResult::NoReturn
    }
});

define_syscall!(syscall_fb_info
    (ctx, info_out: UserPtr<DisplayInfo>) -> Result<(), Errno>
{
    let info = video::get_display_info().ok_or(Errno::EINVAL)?;
    copy_to_user(info_out.inner(), &info).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

// Silence unused warning for the legacy alias.
#[allow(dead_code)]
type _Unused = MmUserPtr<u8>;
