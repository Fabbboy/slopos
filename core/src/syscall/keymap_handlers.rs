//! Keyboard-layout syscall handlers.
//!
//! Thin forwarders to the keymap service the keyboard driver registers; the
//! active layout lives in the driver, which `core` cannot depend on directly.
//!
//! Loading is console administration: the one layout table feeds every TTY and
//! the compositor's input path, not a single client's view of a seat.

use slopos_abi::Errno;
use slopos_abi::input::layout::LAYOUT_NAME_LEN;
use slopos_kernel_services::syscall_services::keymap;
use slopos_mm::user_copy::copy_bytes_to_user;
use slopos_mm::user_ptr::UserBytes;

define_syscall!(syscall_keymap_load
    (ctx, data_ptr: u64, len: u64)
    cap(ConsoleConfig)
    requires(console_admin)
    -> Result<(), Errno>
{
    keymap::load(data_ptr, len as usize)
});

define_syscall!(syscall_keymap_get_name
    (ctx, buf_ptr: u64, buf_len: u64)
    cap(NoneSelf)
    -> Result<u64, Errno>
{
    let mut name = [0u8; LAYOUT_NAME_LEN];
    let n = keymap::current_name(&mut name);
    let want = (n as u64).min(buf_len) as usize;
    if want == 0 {
        return Ok(0);
    }
    let dst = UserBytes::try_new(buf_ptr, want).map_err(|_| Errno::EFAULT)?;
    let written = copy_bytes_to_user(dst, &name[..want]).map_err(|_| Errno::EFAULT)?;
    Ok(written as u64)
});
