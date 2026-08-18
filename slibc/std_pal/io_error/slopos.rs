//! `std::io::Error` ↔ errno decoder for `target_os = "slopos"`, replacing the
//! upstream `generic.rs` decoder that maps every errno to `Uncategorized`.
//!
//! Errno numbering matches `slopos-abi::syscall::errno_defs` (the
//! negative-i64 values there are stored positive in
//! `io::Error::raw_os_error`).

use crate::io::ErrorKind;

pub fn errno() -> i32 {
    0
}

pub fn is_interrupted(code: i32) -> bool {
    code == 4 // EINTR
}

pub fn decode_error_kind(code: i32) -> ErrorKind {
    use ErrorKind::*;
    match code {
        1 => PermissionDenied,     // EPERM
        2 => NotFound,             // ENOENT
        3 => Uncategorized,        // ESRCH
        4 => Interrupted,          // EINTR
        5 => Uncategorized,        // EIO
        6 => Uncategorized,        // ENXIO
        10 => Uncategorized,       // ECHILD
        11 => WouldBlock,          // EAGAIN / EWOULDBLOCK
        12 => OutOfMemory,         // ENOMEM
        13 => PermissionDenied,    // EACCES (POSIX value; slopos never emits it)
        14 => Uncategorized,       // EFAULT
        16 => ResourceBusy,        // EBUSY
        17 => AlreadyExists,       // EEXIST
        20 => NotADirectory,       // ENOTDIR
        21 => IsADirectory,        // EISDIR
        22 => InvalidInput,        // EINVAL
        24 => Uncategorized,       // EMFILE
        28 => StorageFull,         // ENOSPC
        32 => BrokenPipe,          // EPIPE
        34 => Uncategorized,       // ERANGE
        38 => Unsupported,         // ENOSYS
        39 => DirectoryNotEmpty,   // ENOTEMPTY
        88 => Uncategorized,       // ENOTSOCK
        89 => Uncategorized,       // EDESTADDRREQ
        93 => Unsupported,         // EPROTONOSUPPORT
        95 => Unsupported,         // EOPNOTSUPP
        97 => Unsupported,         // EAFNOSUPPORT
        98 => AddrInUse,           // EADDRINUSE
        99 => AddrNotAvailable,    // EADDRNOTAVAIL
        101 => NetworkUnreachable, // ENETUNREACH
        103 => ConnectionAborted,  // ECONNABORTED
        104 => ConnectionReset,    // ECONNRESET
        105 => Uncategorized,      // ENOBUFS
        106 => Uncategorized,      // EISCONN
        107 => NotConnected,       // ENOTCONN
        110 => TimedOut,           // ETIMEDOUT
        111 => ConnectionRefused,  // ECONNREFUSED
        113 => HostUnreachable,    // EHOSTUNREACH
        115 => InProgress,         // EINPROGRESS
        _ => Uncategorized,
    }
}

pub fn error_string(errno: i32) -> String {
    let msg = match errno {
        1 => "operation not permitted",
        2 => "no such file or directory",
        3 => "no such process",
        4 => "interrupted system call",
        5 => "input/output error",
        6 => "no such device or address",
        10 => "no child processes",
        11 => "resource temporarily unavailable",
        12 => "cannot allocate memory",
        13 => "permission denied",
        14 => "bad address",
        16 => "device or resource busy",
        17 => "file exists",
        20 => "not a directory",
        21 => "is a directory",
        22 => "invalid argument",
        24 => "too many open files",
        28 => "no space left on device",
        32 => "broken pipe",
        34 => "numerical result out of range",
        38 => "function not implemented",
        39 => "directory not empty",
        88 => "socket operation on non-socket",
        89 => "destination address required",
        93 => "protocol not supported",
        95 => "operation not supported",
        97 => "address family not supported",
        98 => "address in use",
        99 => "address not available",
        101 => "network unreachable",
        103 => "connection aborted",
        104 => "connection reset",
        105 => "no buffer space available",
        106 => "transport endpoint already connected",
        107 => "transport endpoint not connected",
        110 => "connection timed out",
        111 => "connection refused",
        113 => "host unreachable",
        115 => "operation now in progress",
        _ => "unknown error",
    };
    msg.to_string()
}
