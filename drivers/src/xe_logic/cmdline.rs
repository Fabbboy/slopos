//! Parser for the `xe.*` kernel command-line knobs into an [`XeConfig`]. Pure
//! string parsing over `core` only — no allocation, no I/O.
//!
//! The defaults make the driver *work*: when it binds to a matching Intel
//! display device it inherits the firmware modeset and drives scanout, exactly
//! like any other kernel display driver. None of these knobs is required for
//! normal operation — they are expert overrides and recovery escapes:
//!
//! - `xe.diag=on` emits verbose register logging (like `drm.debug`).
//! - `xe.modeset=off` keeps the firmware framebuffer and writes no display
//!   register — the `nomodeset` recovery escape. Pair with `xe.diag=on` for a
//!   read-only boot that only reports what the silicon programmed.
//! - `xe.nocursor=on` forces the software cursor (skips the hardware cursor
//!   plane) — an escape if the hardware cursor ever misbehaves.
//! - `xe.pipe` / `xe.wdog_ms` / `xe.force_did` are expert overrides with sane
//!   defaults (auto-detect / 100 ms / real PCI ID).
//!
//! There is deliberately no knob for tear-free / double-buffering: like every
//! real KMS driver, the scanout is always vblank-synced double-buffered, with an
//! automatic single-buffer fallback only when the second scan buffer cannot be
//! allocated. `xe.modeset=off` is the recovery escape if the display misbehaves.

use super::regs::Pipe;

/// Parsed `xe.*` command-line configuration. Plain `Copy` data, no heap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XeConfig {
    /// Emit verbose `XE-DIAG:` register-decode logging while probing. Default
    /// off — purely a debug aid.
    pub diag: bool,
    /// Kernel modeset master switch. `true` (default) drives scanout on bind;
    /// `xe.modeset=off` keeps the firmware framebuffer and touches no display
    /// register — the `nomodeset` recovery escape.
    pub modeset: bool,
    /// Force the software cursor: skip binding the hardware cursor plane and let
    /// the compositor composite its own cursor. Default off (the hardware cursor
    /// is used). A recovery escape if the hardware cursor ever misbehaves.
    pub nocursor: bool,
    /// Active-pipe override; `None` (default) auto-detects the scanning pipe.
    pub pipe: Option<Pipe>,
    /// Watchdog budget, in milliseconds, for the post-repoint scanning check
    /// that gates the automatic firmware-framebuffer rollback. Default 100.
    pub wdog_ms: u32,
    /// Force a PCI Device ID for platform matching when the real one is not yet
    /// in the table; `None` (default) trusts the device's own ID.
    pub force_did: Option<u16>,
}

impl Default for XeConfig {
    fn default() -> Self {
        Self {
            diag: false,
            modeset: true,
            nocursor: false,
            pipe: None,
            wdog_ms: 100,
            force_did: None,
        }
    }
}

/// Parse whitespace-separated `xe.*` tokens into an [`XeConfig`]. Tokens that
/// are unrecognised or carry a malformed value are ignored, leaving that field
/// at its default — so the driver still works on a typo.
pub fn parse(cmdline: &str) -> XeConfig {
    let mut config = XeConfig::default();
    for token in cmdline.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        match key {
            "xe.diag" => {
                if let Some(flag) = parse_on_off(value) {
                    config.diag = flag;
                }
            }
            "xe.modeset" => {
                if let Some(flag) = parse_on_off(value) {
                    config.modeset = flag;
                }
            }
            "xe.nocursor" => {
                if let Some(flag) = parse_on_off(value) {
                    config.nocursor = flag;
                }
            }
            "xe.pipe" => {
                if let Some(pipe) = parse_pipe(value) {
                    config.pipe = Some(pipe);
                }
            }
            "xe.wdog_ms" => {
                if let Ok(ms) = u32::from_str_radix(value, 10) {
                    config.wdog_ms = ms;
                }
            }
            "xe.force_did" => {
                if let Some(did) = parse_hex_u16(value) {
                    config.force_did = Some(did);
                }
            }
            _ => {}
        }
    }
    config
}

/// `on` → `true`, `off` → `false`, anything else → keep default.
fn parse_on_off(value: &str) -> Option<bool> {
    match value {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// Case-insensitive single-letter pipe selector (`A`/`B`/`C`).
fn parse_pipe(value: &str) -> Option<Pipe> {
    match value.as_bytes() {
        [b'A' | b'a'] => Some(Pipe::A),
        [b'B' | b'b'] => Some(Pipe::B),
        [b'C' | b'c'] => Some(Pipe::C),
        _ => None,
    }
}

/// Parse a `0x`-prefixed (case-insensitive) hexadecimal `u16`.
fn parse_hex_u16(value: &str) -> Option<u16> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))?;
    u16::from_str_radix(digits, 16).ok()
}
