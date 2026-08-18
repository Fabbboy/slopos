//! Host-side tests for `slopos_ostd::early_console`.

use std::sync::{Mutex, MutexGuard};

use slopos_ostd::early_console;

/// The host stub records into one process-global buffer, so parallel tests
/// would interleave bytes; each test holds this for its full body.
static MOCK_GUARD: Mutex<()> = Mutex::new(());

fn lock_and_drain() -> MutexGuard<'static, ()> {
    let guard = match MOCK_GUARD.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    // Residue from a poisoned test would otherwise leak into this one.
    let _ = early_console::take_recorded_bytes_for_tests();
    guard
}

#[test]
fn write_byte_appends_to_mock_buffer() {
    let _g = lock_and_drain();
    early_console::write_byte(0xAB);
    let buf = early_console::take_recorded_bytes_for_tests();
    assert_eq!(buf, vec![0xAB]);
}

#[test]
fn write_bytes_converts_lone_newline_to_crlf() {
    let _g = lock_and_drain();
    early_console::write_bytes(b"a\nb");
    let buf = early_console::take_recorded_bytes_for_tests();
    assert_eq!(buf, vec![b'a', b'\r', b'\n', b'b']);
}

#[test]
fn write_bytes_preserves_existing_crlf() {
    let _g = lock_and_drain();
    early_console::write_bytes(b"\r\n");
    let buf = early_console::take_recorded_bytes_for_tests();
    assert_eq!(buf, vec![b'\r', b'\n']);
}

#[test]
fn flush_does_not_panic_on_empty_buffer() {
    let _g = lock_and_drain();
    early_console::flush();
    let buf = early_console::take_recorded_bytes_for_tests();
    assert!(buf.is_empty(), "flush must not write bytes");
}

#[test]
fn write_bytes_handles_multiple_newlines() {
    let _g = lock_and_drain();
    early_console::write_bytes(b"x\ny\nz");
    let buf = early_console::take_recorded_bytes_for_tests();
    assert_eq!(
        buf,
        vec![b'x', b'\r', b'\n', b'y', b'\r', b'\n', b'z'],
        "every lone '\\n' must be preceded by '\\r'"
    );
}
