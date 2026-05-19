use slopos_abi::Errno;
use slopos_abi::{USER_FS_MAX_ENTRIES, UserFsEntry, UserFsList, UserFsStat};

use slopos_fs::fileio::{
    file_close_fd, file_list_path, file_mkdir_path, file_open_for_process, file_read_fd,
    file_stat_path, file_unlink_path, file_write_fd,
};

use slopos_mm::user_copy::{copy_bytes_to_user, copy_from_user, copy_to_user};
use slopos_mm::user_io_buf::{UserReadBuf, UserWriteBuf};
use slopos_mm::user_ptr::{UserBytes as MmUserBytes, UserPtr as MmUserPtr};
use slopos_ostd::KVec;
use slopos_ostd::util::byte_view::pod_slice_as_bytes;

use crate::syscall::args::{Fd, UserBytes, UserCStr, UserPtr};
use crate::syscall::common::USER_PATH_MAX;

fn errno_from_neg(rc: i32) -> Errno {
    Errno::from_raw(rc).unwrap_or(Errno::EINVAL)
}

define_syscall!(syscall_fs_open
    (ctx, path: UserCStr<USER_PATH_MAX>, flags: u32)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    let fd = file_open_for_process(pid, path.as_bytes(), flags);
    if fd < 0 {
        Err(errno_from_neg(fd))
    } else {
        Ok(fd as u64)
    }
});

define_syscall!(syscall_fs_close
    (ctx, fd: Fd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    let rc = file_close_fd(pid, fd.raw());
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_fs_read
    (ctx, fd: Fd, buf: UserBytes)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    if buf.base_u64() == 0 {
        return Err(Errno::EFAULT);
    }
    let count = buf.len();
    let mut io_buf = UserWriteBuf::new(buf.base_u64(), count).ok_or(Errno::EFAULT)?;
    let bytes = file_read_fd(pid, fd.raw(), &mut io_buf);
    if bytes == -512 {
        return Err(Errno::ERESTARTSYS);
    }
    if bytes < 0 {
        return Err(Errno::from_raw(bytes as i32).unwrap_or(Errno::EINVAL));
    }
    Ok(bytes as u64)
});

define_syscall!(syscall_fs_write
    (ctx, fd: Fd, buf: UserBytes)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    if buf.base_u64() == 0 {
        return Err(Errno::EFAULT);
    }
    let count = buf.len();
    let io_buf = UserReadBuf::new(buf.base_u64(), count).ok_or(Errno::EFAULT)?;
    let bytes = file_write_fd(pid, fd.raw(), &io_buf);
    if bytes < 0 {
        Err(Errno::from_raw(bytes as i32).unwrap_or(Errno::EINVAL))
    } else {
        Ok(bytes as u64)
    }
});

define_syscall!(syscall_fs_stat
    (ctx, path: UserCStr<USER_PATH_MAX>, out: UserPtr<UserFsStat>) -> Result<(), Errno>
{
    let mut stat = UserFsStat { type_: 0, size: 0 };
    let rc = file_stat_path(path.as_bytes(), &mut stat.type_, &mut stat.size);
    if rc != 0 {
        return Err(errno_from_neg(rc));
    }
    copy_to_user(out.inner(), &stat).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_fs_mkdir
    (ctx, path: UserCStr<USER_PATH_MAX>) -> Result<(), Errno>
{
    let rc = file_mkdir_path(path.as_bytes());
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_fs_unlink
    (ctx, path: UserCStr<USER_PATH_MAX>) -> Result<(), Errno>
{
    let rc = file_unlink_path(path.as_bytes());
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_fs_list
    (ctx, path: UserCStr<USER_PATH_MAX>, list: UserPtr<UserFsList>) -> Result<(), Errno>
{
    let _ = pod_slice_as_bytes::<i8>;  // keep the helper symbol live for legacy users
    let mut list_hdr = copy_from_user(list.inner()).map_err(|_| Errno::EFAULT)?;

    let cap = list_hdr.max_entries;
    if cap == 0 || cap > USER_FS_MAX_ENTRIES || list_hdr.entries.is_null() {
        return Err(Errno::EINVAL);
    }

    let cap_usize = cap as usize;
    let zero_entry = UserFsEntry::default();
    let mut tmp = KVec::<UserFsEntry>::with_capacity(cap_usize).map_err(|_| Errno::ENOMEM)?;
    for _ in 0..cap_usize {
        tmp.push(zero_entry).map_err(|_| Errno::ENOMEM)?;
    }

    let mut count: u32 = 0;
    let rc = file_list_path(path.as_bytes(), tmp.as_mut_slice(), &mut count);
    if rc != 0 {
        return Err(errno_from_neg(rc));
    }

    list_hdr.count = count;

    let entries_bytes =
        slopos_ostd::util::byte_view::pod_slice_as_bytes(&tmp[..count as usize]);
    let entries_user = MmUserBytes::try_new(list_hdr.entries as u64, entries_bytes.len())
        .map_err(|_| Errno::EFAULT)?;

    copy_bytes_to_user(entries_user, entries_bytes).map_err(|_| Errno::EFAULT)?;
    let hdr_ptr = MmUserPtr::<UserFsList>::try_new(list.as_u64()).map_err(|_| Errno::EFAULT)?;
    copy_to_user(hdr_ptr, &list_hdr).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_rename
    (ctx, old_path: UserCStr<USER_PATH_MAX>, new_path: UserCStr<USER_PATH_MAX>) -> Result<(), Errno>
{
    slopos_fs::vfs::ops::vfs_rename(old_path.as_bytes(), new_path.as_bytes())
        .map_err(|_| Errno::EINVAL)
});
