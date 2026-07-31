#![feature(restricted_std)]

//! slibc stdio conformance: the open-stream registry, per-stream locking, and
//! read/write direction state on an update stream.
//!
//! Every case drives the code under test through `slopos_slibc::stdio::shim`
//! and verifies the result with `std::fs`, which reaches the file through the
//! kernel rather than through the buffering being tested.

use std::sync::OnceLock;
use std::thread;

use slopos_slibc::process::shim as proc_shim;
use slopos_slibc::stdio::shim::{self, Stream};
use slopos_slibc::stdio::{_IONBF, SEEK_CUR, SEEK_SET};

use slopos_userland as _;

const DIGITS: &[u8] = b"0123456789";

fn seed(path: &str, contents: &[u8]) -> bool {
    let _ = std::fs::remove_file(path);
    std::fs::write(path, contents).is_ok()
}

fn contents(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

fn cstr(path: &str, out: &mut [u8; 128]) -> usize {
    let bytes = path.as_bytes();
    out[..bytes.len()].copy_from_slice(bytes);
    out[bytes.len()] = 0;
    bytes.len() + 1
}

/// Open `path` and hand back the stream, or report the failure and give up.
fn open(path: &str, mode: &[u8]) -> Option<Stream> {
    let mut buf = [0u8; 128];
    let len = cstr(path, &mut buf);
    shim::fopen(&buf[..len], mode)
}

// ---------------------------------------------------------------------------
// Regression guards — these hold before and after the registry exists
// ---------------------------------------------------------------------------

/// A `fopen`'d write stream delivers its buffer at `fclose`.
fn test_fopen_roundtrip() -> bool {
    const PATH: &str = "/tmp/stdio_roundtrip.txt";
    let _ = std::fs::remove_file(PATH);

    let Some(f) = open(PATH, b"w\0") else {
        eprintln!("stdio: fopen(w) failed");
        return false;
    };
    if shim::fwrite(f, b"roundtrip") != 9 {
        eprintln!("stdio: short fwrite");
        return false;
    }
    if shim::fclose(f) != 0 {
        eprintln!("stdio: fclose failed");
        return false;
    }

    contents(PATH) == b"roundtrip"
}

/// `_IONBF` reaches the descriptor without any flush.
fn test_unbuffered_writes_immediately() -> bool {
    const PATH: &str = "/tmp/stdio_unbuffered.txt";
    let _ = std::fs::remove_file(PATH);

    let Some(f) = open(PATH, b"w\0") else {
        return false;
    };
    if shim::setvbuf_mode(f, _IONBF) != 0 {
        eprintln!("stdio: setvbuf(_IONBF) failed");
        return false;
    }
    shim::fwrite(f, b"now");

    let seen = contents(PATH);
    shim::fclose(f);
    seen == b"now"
}

/// `fgets` stops at the newline and NUL-terminates.
fn test_fgets_reads_line() -> bool {
    const PATH: &str = "/tmp/stdio_fgets.txt";
    if !seed(PATH, b"alpha\nbeta\n") {
        return false;
    }

    let Some(f) = open(PATH, b"r\0") else {
        return false;
    };
    let mut line = [0u8; 32];
    let got = shim::fgets(f, &mut line);
    shim::fclose(f);

    match got {
        Some(n) => &line[..n] == b"alpha\n",
        None => {
            eprintln!("stdio: fgets returned end-of-file on a two-line file");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// `fflush(NULL)` must reach every open output stream, not just the standard
/// ones (C11 §7.21.5.2). Both an `fopen`'d and an `fdopen`'d stream count.
fn test_fflush_null_flushes_fopened() -> bool {
    const OPENED: &str = "/tmp/stdio_flushnull_a.txt";
    const ADOPTED: &str = "/tmp/stdio_flushnull_b.txt";
    let _ = std::fs::remove_file(OPENED);
    let _ = std::fs::remove_file(ADOPTED);

    let Some(a) = open(OPENED, b"w\0") else {
        return false;
    };

    let mut buf = [0u8; 128];
    let len = cstr(ADOPTED, &mut buf);
    let fd = shim::open_rw_create(&buf[..len]);
    if fd < 0 {
        eprintln!("stdio: raw open failed");
        return false;
    }
    let Some(b) = shim::fdopen(fd, b"w\0") else {
        eprintln!("stdio: fdopen failed");
        return false;
    };

    // Ten bytes each, no newline, well under the 4 KiB buffer.
    shim::fwrite(a, b"unfinished");
    shim::fwrite(b, b"adoptedten");

    if shim::fflush_all() != 0 {
        eprintln!("stdio: fflush(NULL) reported an error");
        return false;
    }

    let seen_a = contents(OPENED);
    let seen_b = contents(ADOPTED);
    shim::fclose(a);
    shim::fclose(b);

    if seen_a != b"unfinished" {
        eprintln!("stdio: fopen'd stream not flushed ({} bytes)", seen_a.len());
        return false;
    }
    if seen_b != b"adoptedten" {
        eprintln!(
            "stdio: fdopen'd stream not flushed ({} bytes)",
            seen_b.len()
        );
        return false;
    }
    true
}

const ATEXIT_PATH: &str = "/tmp/stdio_atexit.txt";
static ATEXIT_STREAM: OnceLock<Stream> = OnceLock::new();

extern "C" fn append_from_atexit() {
    if let Some(stream) = ATEXIT_STREAM.get() {
        shim::fputs(*stream, b"atexit\0");
    }
}

/// C11 §7.22.4.4: `exit` flushes every open output stream, and it does so
/// *after* the `atexit` handlers run — a handler's own output has to reach the
/// descriptor too. The child never calls `fclose`.
fn test_exit_flushes_after_atexit() -> bool {
    let _ = std::fs::remove_file(ATEXIT_PATH);

    let pid = proc_shim::fork();
    if pid < 0 {
        eprintln!("stdio: fork failed");
        return false;
    }

    if pid == 0 {
        let Some(f) = open(ATEXIT_PATH, b"w\0") else {
            proc_shim::_exit(70);
        };
        if ATEXIT_STREAM.set(f).is_err() {
            proc_shim::_exit(71);
        }
        if proc_shim::atexit(append_from_atexit) != 0 {
            proc_shim::_exit(72);
        }
        shim::fwrite(f, b"main-");
        proc_shim::exit(0);
    }

    let status = proc_shim::wait_for_child(pid);
    if status != 0 {
        eprintln!("stdio: child exited with status {status}");
        return false;
    }

    let seen = contents(ATEXIT_PATH);
    if seen != b"main-atexit" {
        eprintln!("stdio: exit produced {seen:?}, want b\"main-atexit\"");
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// Per-stream locking
// ---------------------------------------------------------------------------

const RECORD_LEN: usize = 64;
const RECORDS_PER_THREAD: usize = 1000;
const WRITERS: usize = 4;

/// POSIX §2.5.1 makes a whole `fwrite` atomic with respect to other stdio
/// calls on the same stream, so four threads sharing one stream must produce
/// exactly their bytes and no torn record.
fn test_threads_share_one_stream() -> bool {
    const PATH: &str = "/tmp/stdio_threads.txt";
    let _ = std::fs::remove_file(PATH);

    let Some(f) = open(PATH, b"w\0") else {
        return false;
    };

    let mut handles = Vec::with_capacity(WRITERS);
    for id in 0..WRITERS {
        let stream = f;
        handles.push(thread::spawn(move || {
            let record = [b'A' + id as u8; RECORD_LEN];
            for _ in 0..RECORDS_PER_THREAD {
                if id == 0 {
                    // Exercise the recursive path: `fwrite` takes the same
                    // lock the caller already holds.
                    shim::flockfile(stream);
                    shim::fwrite(stream, &record);
                    shim::funlockfile(stream);
                } else {
                    shim::fwrite(stream, &record);
                }
            }
        }));
    }
    for h in handles {
        if h.join().is_err() {
            eprintln!("stdio: writer thread panicked");
            return false;
        }
    }

    shim::fclose(f);

    let seen = contents(PATH);
    let want = WRITERS * RECORDS_PER_THREAD * RECORD_LEN;
    if seen.len() != want {
        eprintln!("stdio: wrote {} bytes, want {want}", seen.len());
        return false;
    }
    for (index, chunk) in seen.chunks(RECORD_LEN).enumerate() {
        let first = chunk[0];
        if !(b'A'..b'A' + WRITERS as u8).contains(&first) || chunk.iter().any(|&b| b != first) {
            eprintln!("stdio: record {index} is torn");
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Direction state
// ---------------------------------------------------------------------------

/// C11 §7.21.5.3: on an update stream, input after output flushes the pending
/// output. The buffered byte must reach the file, and the read must see the
/// file past it.
fn test_rplus_read_after_write() -> bool {
    const PATH: &str = "/tmp/stdio_rplus_rw.txt";
    if !seed(PATH, DIGITS) {
        return false;
    }

    let Some(f) = open(PATH, b"r+\0") else {
        return false;
    };
    shim::fputc(b'Z', f);
    let c = shim::fgetc(f);
    shim::fclose(f);

    if c != b'1' as i32 {
        eprintln!("stdio: fgetc after fputc returned {c}, want {}", b'1');
        return false;
    }
    let seen = contents(PATH);
    if seen != b"Z123456789" {
        eprintln!("stdio: r+ file is {seen:?}, want b\"Z123456789\"");
        return false;
    }
    true
}

/// The other direction: output after input gives back the read-ahead the
/// program never consumed, so the write lands at the stream position rather
/// than wherever the 4 KiB refill left the descriptor.
fn test_rplus_write_after_read() -> bool {
    const PATH: &str = "/tmp/stdio_rplus_wr.txt";
    if !seed(PATH, DIGITS) {
        return false;
    }

    let Some(f) = open(PATH, b"r+\0") else {
        return false;
    };
    let first = shim::fgetc(f);
    shim::fputc(b'X', f);
    shim::fclose(f);

    if first != b'0' as i32 {
        eprintln!("stdio: first fgetc returned {first}");
        return false;
    }
    let seen = contents(PATH);
    if seen != b"0X23456789" {
        eprintln!("stdio: r+ file is {seen:?}, want b\"0X23456789\"");
        return false;
    }
    true
}

/// `SEEK_CUR` is relative to the stream position. The descriptor sits a whole
/// buffer ahead of it after one `fgetc`, and `fseek` has to account for that.
fn test_fseek_cur_after_read_ahead() -> bool {
    const PATH: &str = "/tmp/stdio_seekcur.txt";
    if !seed(PATH, DIGITS) {
        return false;
    }

    let Some(f) = open(PATH, b"r\0") else {
        return false;
    };
    let first = shim::fgetc(f);
    if shim::fseek(f, 1, SEEK_CUR) != 0 {
        eprintln!("stdio: fseek(SEEK_CUR) failed");
        shim::fclose(f);
        return false;
    }
    let third = shim::fgetc(f);
    shim::fclose(f);

    if first != b'0' as i32 || third != b'2' as i32 {
        eprintln!("stdio: read {first} then {third}, want {} {}", b'0', b'2');
        return false;
    }
    true
}

/// C11 §7.21.9.2 has `fseek` clear the end-of-file indicator. It does not
/// license clearing the error indicator — only `clearerr` and `rewind` do that.
fn test_fseek_keeps_error_flag() -> bool {
    const PATH: &str = "/tmp/stdio_errflag.txt";
    if !seed(PATH, DIGITS) {
        return false;
    }

    let Some(f) = open(PATH, b"r\0") else {
        return false;
    };
    shim::fputc(b'x', f);
    if shim::ferror(f) == 0 {
        eprintln!("stdio: write to a read-only stream did not set the error flag");
        shim::fclose(f);
        return false;
    }

    shim::fseek(f, 0, SEEK_SET);
    let after_seek = shim::ferror(f);
    shim::clearerr(f);
    let after_clear = shim::ferror(f);
    shim::fclose(f);

    if after_seek == 0 {
        eprintln!("stdio: fseek cleared the error indicator");
        return false;
    }
    if after_clear != 0 {
        eprintln!("stdio: clearerr left the error indicator set");
        return false;
    }
    true
}

/// POSIX: `fflush` on an input stream sets the descriptor to the stream
/// position and drops the read-ahead. Reading the raw descriptor is the only
/// way to see where it actually ended up.
fn test_fflush_read_repositions_fd() -> bool {
    const PATH: &str = "/tmp/stdio_flushread.txt";
    if !seed(PATH, DIGITS) {
        return false;
    }

    let Some(f) = open(PATH, b"r\0") else {
        return false;
    };
    let first = shim::fgetc(f);
    if shim::fflush(f) != 0 {
        eprintln!("stdio: fflush on an input stream reported an error");
        shim::fclose(f);
        return false;
    }

    let fd = shim::fileno(f);
    let mut rest = [0u8; 16];
    let n = shim::read_fd(fd, &mut rest);
    shim::fclose(f);

    if first != b'0' as i32 {
        eprintln!("stdio: first fgetc returned {first}");
        return false;
    }
    if n != 9 || &rest[..9] != b"123456789" {
        eprintln!(
            "stdio: raw read after fflush returned {n} bytes: {:?}",
            &rest[..n.max(0) as usize]
        );
        return false;
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("fopen_roundtrip", test_fopen_roundtrip),
    (
        "unbuffered_writes_immediately",
        test_unbuffered_writes_immediately,
    ),
    ("fgets_reads_line", test_fgets_reads_line),
    (
        "fflush_null_flushes_fopened",
        test_fflush_null_flushes_fopened,
    ),
    ("exit_flushes_after_atexit", test_exit_flushes_after_atexit),
    ("threads_share_one_stream", test_threads_share_one_stream),
    ("rplus_read_after_write", test_rplus_read_after_write),
    ("rplus_write_after_read", test_rplus_write_after_read),
    (
        "fseek_cur_after_read_ahead",
        test_fseek_cur_after_read_ahead,
    ),
    ("fseek_keeps_error_flag", test_fseek_keeps_error_flag),
    (
        "fflush_read_repositions_fd",
        test_fflush_read_repositions_fd,
    ),
];

fn main() {
    slopos_slibc::test_harness::run_with_progress("stdio_stream", CASES);
}
