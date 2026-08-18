//! Parser for the `xe.*` kernel command-line knobs into an [`XeConfig`]. Pure
//! string parsing over `core` only — no allocation, no I/O.
//!
//! Every knob is an expert override or a recovery escape; the defaults drive
//! scanout on bind. There is deliberately no tear-free / double-buffering knob:
//! scanout is always vblank-synced double-buffered, falling back to a single
//! buffer only when the second scan buffer cannot be allocated.

use super::regs::Pipe;

/// Parsed `xe.*` command-line configuration. Plain `Copy` data, no heap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XeConfig {
    /// Verbose `XE-DIAG:` register-decode logging while probing. Default off.
    pub diag: bool,
    /// Modeset master switch. `true` (default) drives scanout on bind; `off`
    /// keeps the firmware framebuffer and touches no display register.
    pub modeset: bool,
    /// Skip the hardware cursor plane and let the compositor composite its own.
    /// Default off.
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

fn parse_on_off(value: &str) -> Option<bool> {
    match value {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

fn parse_pipe(value: &str) -> Option<Pipe> {
    match value.as_bytes() {
        [b'A' | b'a'] => Some(Pipe::A),
        [b'B' | b'b'] => Some(Pipe::B),
        [b'C' | b'c'] => Some(Pipe::C),
        _ => None,
    }
}

fn parse_hex_u16(value: &str) -> Option<u16> {
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))?;
    u16::from_str_radix(digits, 16).ok()
}
