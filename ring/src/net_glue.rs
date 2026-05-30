//! Socket-opcode glue (SLOPRING § 12, cross-cutting reality 2).
//!
//! `OP_ACCEPT` needs a *non-blocking* accept that installs the new fd
//! in the caller's fd table. AF_INET and AF_UNIX have different ABIs, so
//! this routes per family via `FileOps::is_unix_socket()`, forcing the
//! socket's stored nonblocking flag across the probe (and restoring it).

use slopos_abi::Errno;
use slopos_abi::file_ops::FileKind;

use slopos_net::unix_socket::SocketHandle;
use slopos_net::{socket, unix_socket, unix_socket_file_ops};

/// Non-blocking accept on `fd` for process `pid`. Returns:
///   * `Ok(Some(new_fd))` — a connection was accepted and installed;
///   * `Ok(None)`         — would block (no pending connection);
///   * `Err(errno)`       — a real error (`ENOTSOCK`, `ENOMEM`, …).
///
/// Reserve-before-side-effect (SLOPRING § 11) is the *caller's*
/// responsibility — by the time we install the fd here the CQE slot is
/// already reserved, so the accepted fd can always be reported.
pub fn accept_nonblock(pid: u32, fd: i32) -> Result<Option<i32>, Errno> {
    let Some((handle, ops)) = slopos_fs::fileio::fileio_get_handle_and_ops(pid, fd) else {
        return Err(Errno::ENOTSOCK);
    };
    if ops.kind() != FileKind::Socket {
        return Err(Errno::ENOTSOCK);
    }

    if ops.is_unix_socket() {
        let listener = SocketHandle::from_usize(handle);
        // Force nonblocking across the accept, restore after.
        let _ = unix_socket::unix_set_nonblocking(listener, true);
        let result = unix_socket::unix_accept(listener);
        let _ = unix_socket::unix_set_nonblocking(listener, false);
        match result {
            Ok(accepted) => {
                let new_fd = slopos_fs::fileio_open_fd_with_ops(
                    pid,
                    &unix_socket_file_ops::UNIX_SOCKET_FILE_OPS,
                    accepted.as_usize(),
                );
                if new_fd < 0 {
                    let _ = unix_socket::unix_close(accepted);
                    return Err(Errno::ENOMEM);
                }
                Ok(Some(new_fd))
            }
            Err(rc) => {
                let e = Errno::from_raw(rc).unwrap_or(Errno::EINVAL);
                if e == Errno::EAGAIN { Ok(None) } else { Err(e) }
            }
        }
    } else {
        let sock_idx = handle as u32;
        let _ = socket::socket_set_nonblocking(sock_idx, true);
        let accepted =
            socket::socket_accept(sock_idx, core::ptr::null_mut(), core::ptr::null_mut());
        let _ = socket::socket_set_nonblocking(sock_idx, false);
        if accepted < 0 {
            let e = Errno::from_raw(accepted).unwrap_or(Errno::EINVAL);
            return if e == Errno::EAGAIN { Ok(None) } else { Err(e) };
        }
        let new_fd = slopos_fs::fileio_open_socket_fd(pid, accepted as u32);
        if new_fd < 0 {
            let _ = socket::socket_close(accepted as u32);
            return Err(Errno::ENOMEM);
        }
        Ok(Some(new_fd))
    }
}
