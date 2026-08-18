//! Diagnostic-console policy, parsed from the Limine cmdline.
//!
//! The installed state is one packed `AtomicU64` rather than a lock: the read
//! side runs from the keyboard ISR and from the serial drain under the per-TTY
//! lock, and lockdep classes are a budgeted resource.

use core::sync::atomic::{AtomicU64, Ordering};

use super::{KCMD_DESTRUCTIVE, KCMD_INFORMATIONAL};

/// Permission mask for `kconsole=on`: informational commands only.
pub const MASK_DEFAULT: u8 = KCMD_INFORMATIONAL;
/// Every command, including the ones that take the machine down.
pub const MASK_ALL: u8 = KCMD_INFORMATIONAL | KCMD_DESTRUCTIVE;

const DEFAULT_ARM_MS: u16 = 3000;
const DEFAULT_MAX_LINES: u16 = 512;
const DEFAULT_PROBE_MS: u16 = 250;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KConfig {
    /// Command classes that may run; a command sharing no bit is refused, and
    /// `0` disables the console outright.
    pub mask: u8,
    /// Whether the serial BREAK trigger is armed.
    pub serial: bool,
    /// How long a trigger stays armed waiting for its command key, in ms.
    pub arm_ms: u16,
    /// Per-command line budget.
    pub max_lines: u16,
    /// How long the all-CPU probe waits for one CPU to answer, in ms.
    pub probe_ms: u16,
}

impl KConfig {
    pub const fn defaults() -> Self {
        Self {
            mask: MASK_DEFAULT,
            serial: true,
            arm_ms: DEFAULT_ARM_MS,
            max_lines: DEFAULT_MAX_LINES,
            probe_ms: DEFAULT_PROBE_MS,
        }
    }

    const fn pack(self) -> u64 {
        (self.mask as u64)
            | ((self.serial as u64) << 8)
            | ((self.arm_ms as u64) << 16)
            | ((self.max_lines as u64) << 32)
            | ((self.probe_ms as u64) << 48)
    }

    const fn unpack(raw: u64) -> Self {
        Self {
            mask: raw as u8,
            serial: (raw >> 8) & 1 != 0,
            arm_ms: (raw >> 16) as u16,
            max_lines: (raw >> 32) as u16,
            probe_ms: (raw >> 48) as u16,
        }
    }
}

/// The live policy. Written once at boot, read from every trigger path.
static PACKED: AtomicU64 = AtomicU64::new(KConfig::defaults().pack());

pub fn install(cfg: KConfig) {
    PACKED.store(cfg.pack(), Ordering::Release);
}

#[inline]
pub fn current() -> KConfig {
    KConfig::unpack(PACKED.load(Ordering::Acquire))
}

/// Whether any command may run at all. Relaxed: this is on the keyboard ISR's
/// path for every keystroke.
#[inline]
pub fn enabled() -> bool {
    (PACKED.load(Ordering::Relaxed) as u8) != 0
}

/// Accepts the spellings the test harness already does, so one boot line reads
/// consistently.
fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "on" | "true" | "yes" | "enabled" | "1" => Some(true),
        "off" | "false" | "no" | "disabled" | "0" => Some(false),
        _ => None,
    }
}

fn parse_u16(value: &str) -> Option<u16> {
    value.parse::<u16>().ok()
}

/// `kconsole=off | on | <hex mask>`.
///
/// The hex form is what lets an operator name a class the default refuses
/// without turning on everything a future release might add.
fn parse_mask(value: &str) -> Option<u8> {
    if let Some(flag) = parse_bool(value) {
        return Some(if flag { MASK_DEFAULT } else { 0 });
    }
    let digits = value.strip_prefix("0x").unwrap_or(value);
    u8::from_str_radix(digits, 16).ok()
}

/// Parse every `kconsole*` key out of a cmdline.
///
/// Unknown keys and malformed values keep the default, so a typo degrades to
/// the shipped policy rather than to a disabled console.
pub fn parse(cmdline: &str) -> KConfig {
    let mut cfg = KConfig::defaults();
    for token in cmdline.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        match key {
            "kconsole" => {
                if let Some(mask) = parse_mask(value) {
                    cfg.mask = mask;
                }
            }
            "kconsole.serial" => {
                if let Some(flag) = parse_bool(value) {
                    cfg.serial = flag;
                }
            }
            "kconsole.arm_ms" => {
                if let Some(ms) = parse_u16(value) {
                    cfg.arm_ms = ms;
                }
            }
            "kconsole.max_lines" => {
                if let Some(lines) = parse_u16(value) {
                    cfg.max_lines = lines;
                }
            }
            "kconsole.probe_ms" => {
                if let Some(ms) = parse_u16(value) {
                    cfg.probe_ms = ms;
                }
            }
            _ => {}
        }
    }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cmdline_is_the_default() {
        assert_eq!(parse(""), KConfig::defaults());
        assert_eq!(parse("tests=on lockdep=warn"), KConfig::defaults());
    }

    #[test]
    fn master_switch_accepts_both_spellings() {
        assert_eq!(parse("kconsole=off").mask, 0);
        assert_eq!(parse("kconsole=0").mask, 0);
        assert_eq!(parse("kconsole=on").mask, MASK_DEFAULT);
    }

    #[test]
    fn hex_mask_names_a_class_the_default_refuses() {
        assert_eq!(parse("kconsole=0x3").mask, MASK_ALL);
        assert_eq!(parse("kconsole=3").mask, MASK_ALL);
        assert!(parse("kconsole=0x3").mask & KCMD_DESTRUCTIVE != 0);
        assert!(parse("kconsole=on").mask & KCMD_DESTRUCTIVE == 0);
    }

    #[test]
    fn sub_knobs_parse() {
        let cfg = parse(
            "kconsole.serial=off kconsole.arm_ms=500 kconsole.max_lines=64 kconsole.probe_ms=10",
        );
        assert!(!cfg.serial);
        assert_eq!(cfg.arm_ms, 500);
        assert_eq!(cfg.max_lines, 64);
        assert_eq!(cfg.probe_ms, 10);
    }

    #[test]
    fn malformed_values_keep_the_default() {
        assert_eq!(parse("kconsole=maybe").mask, MASK_DEFAULT);
        assert_eq!(parse("kconsole.arm_ms=99999999").arm_ms, DEFAULT_ARM_MS);
        assert_eq!(parse("kconsole.arm_ms=").arm_ms, DEFAULT_ARM_MS);
        assert!(parse("kconsole.serial=perhaps").serial);
    }

    #[test]
    fn a_bare_key_is_ignored() {
        assert_eq!(parse("kconsole"), KConfig::defaults());
    }

    #[test]
    fn packing_round_trips_every_field() {
        let cfg = KConfig {
            mask: MASK_ALL,
            serial: false,
            arm_ms: 1234,
            max_lines: 4321,
            probe_ms: 777,
        };
        assert_eq!(KConfig::unpack(cfg.pack()), cfg);
    }
}
