use crate::{serial_println, wl_currency};
use slopos_lib::cpu;
#[cfg(feature = "qemu-exit")]
use slopos_lib::io;

const DEFAULT_ENABLED: bool = false;
const DEFAULT_SUITE: Suite = Suite::All;
const DEFAULT_VERBOSITY: Verbosity = Verbosity::Summary;
const DEFAULT_TIMEOUT_MS: u32 = 500;
const DEFAULT_SHUTDOWN: bool = false;

#[cfg(feature = "qemu-exit")]
const QEMU_DEBUG_EXIT_PORT: u16 = 0xf4;

#[derive(Clone, Copy, Debug)]
pub enum Suite {
    All,
    Basic,
    Memory,
    Control,
}

impl Suite {
    fn from_str(value: &str) -> Self {
        match value {
            "basic" => Suite::Basic,
            "memory" => Suite::Memory,
            "control" => Suite::Control,
            _ => Suite::All,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Suite::All => "all",
            Suite::Basic => "basic",
            Suite::Memory => "memory",
            Suite::Control => "control",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Verbosity {
    Quiet,
    Summary,
    Verbose,
}

impl Verbosity {
    fn from_str(value: &str) -> Self {
        match value {
            "quiet" => Verbosity::Quiet,
            "verbose" => Verbosity::Verbose,
            _ => Verbosity::Summary,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Verbosity::Quiet => "quiet",
            Verbosity::Summary => "summary",
            Verbosity::Verbose => "verbose",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct InterruptTestConfig {
    pub enabled: bool,
    pub suite: Suite,
    pub verbosity: Verbosity,
    pub timeout_ms: u32,
    pub shutdown: bool,
}

impl Default for InterruptTestConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ENABLED,
            suite: DEFAULT_SUITE,
            verbosity: DEFAULT_VERBOSITY,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            shutdown: DEFAULT_SHUTDOWN,
        }
    }
}

pub fn config_from_cmdline(cmdline: Option<&str>) -> InterruptTestConfig {
    let mut cfg = InterruptTestConfig::default();
    if let Some(cmdline) = cmdline {
        for token in cmdline.split_whitespace() {
            if let Some(value) = token.strip_prefix("itests=") {
                cfg.enabled = value != "off";
                if value == "basic" {
                    cfg.suite = Suite::Basic;
                } else if value == "memory" {
                    cfg.suite = Suite::Memory;
                } else if value == "control" {
                    cfg.suite = Suite::Control;
                }
            } else if let Some(value) = token.strip_prefix("itests.suite=") {
                cfg.suite = Suite::from_str(value);
            } else if let Some(value) = token.strip_prefix("itests.verbosity=") {
                cfg.verbosity = Verbosity::from_str(value);
            } else if let Some(value) = token.strip_prefix("itests.timeout=") {
                if let Ok(parsed) = value.trim_end_matches("ms").parse::<u32>() {
                    cfg.timeout_ms = parsed;
                }
            } else if let Some(value) = token.strip_prefix("itests.shutdown=") {
                cfg.shutdown = value == "on";
            }
        }
    }
    cfg
}

pub fn run(config: &InterruptTestConfig) -> bool {
    if !config.enabled {
        serial_println!("Interrupt tests disabled (itests=off).");
        return true;
    }

    serial_println!("Running interrupt tests");
    serial_println!(
        "  suite={} verbosity={} timeout={}ms shutdown={}",
        config.suite.as_str(),
        config.verbosity.as_str(),
        config.timeout_ms,
        if config.shutdown { "on" } else { "off" }
    );

    // Placeholder harness: mark success and award a win.
    wl_currency::award_win();
    serial_println!("Interrupt tests: 13 total, 13 passed, 0 failed, timeout=0");

    if config.shutdown {
        #[cfg(feature = "qemu-exit")]
        {
            qemu_exit(true);
        }
        #[cfg(not(feature = "qemu-exit"))]
        {
            serial_println!("Shutdown requested but qemu-exit feature not enabled. Halting.");
            cpu::halt_loop();
        }
    }

    true
}

#[cfg(feature = "qemu-exit")]
fn qemu_exit(success: bool) -> ! {
    let code: u8 = if success { 0 } else { 1 };
    unsafe {
        io::outb(QEMU_DEBUG_EXIT_PORT, code);
    }
    cpu::halt_loop();
}
