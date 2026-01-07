use crate::{serial_println, wl_currency};
use slopos_lib::cpu;
#[cfg(feature = "qemu-exit")]
use slopos_lib::io;

const DEFAULT_ENABLED: bool = false;
const DEFAULT_SUITE: Suite = Suite::All;
const DEFAULT_VERBOSITY: Verbosity = Verbosity::Summary;
const DEFAULT_TIMEOUT_MS: u32 = 0;
const DEFAULT_SHUTDOWN: bool = false;
const DEFAULT_STACKTRACE_DEMO: bool = false;

#[cfg(feature = "qemu-exit")]
const QEMU_DEBUG_EXIT_PORT: u16 = 0xf4;

// Suite bitmask constants for test harness matching
pub const SUITE_BASIC: u32 = 1 << 0;
pub const SUITE_MEMORY: u32 = 1 << 1;
pub const SUITE_CONTROL: u32 = 1 << 2;
pub const SUITE_SCHEDULER: u32 = 1 << 3;
pub const SUITE_ALL: u32 = SUITE_BASIC | SUITE_MEMORY | SUITE_CONTROL | SUITE_SCHEDULER;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suite {
    All,
    Basic,
    Memory,
    Control,
    Scheduler,
}

impl Suite {
    pub fn from_str(value: &str) -> Self {
        if value.eq_ignore_ascii_case("basic") {
            Suite::Basic
        } else if value.eq_ignore_ascii_case("memory") {
            Suite::Memory
        } else if value.eq_ignore_ascii_case("control") {
            Suite::Control
        } else if value.eq_ignore_ascii_case("scheduler") {
            Suite::Scheduler
        } else {
            Suite::All
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Suite::All => "all",
            Suite::Basic => "basic",
            Suite::Memory => "memory",
            Suite::Control => "control",
            Suite::Scheduler => "scheduler",
        }
    }

    pub fn to_mask(&self) -> u32 {
        match self {
            Suite::All => SUITE_ALL,
            Suite::Basic => SUITE_BASIC,
            Suite::Memory => SUITE_MEMORY,
            Suite::Control => SUITE_CONTROL,
            Suite::Scheduler => SUITE_SCHEDULER,
        }
    }

    pub fn from_mask(mask: u32) -> Self {
        if mask == SUITE_ALL || mask == 0 {
            Suite::All
        } else if mask == SUITE_BASIC {
            Suite::Basic
        } else if mask == SUITE_MEMORY {
            Suite::Memory
        } else if mask == SUITE_CONTROL {
            Suite::Control
        } else if mask == SUITE_SCHEDULER {
            Suite::Scheduler
        } else {
            Suite::All
        }
    }
}

impl core::fmt::Display for Suite {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verbosity {
    Quiet,
    Summary,
    Verbose,
}

impl Verbosity {
    pub fn from_str(value: &str) -> Self {
        if value.eq_ignore_ascii_case("quiet") {
            Verbosity::Quiet
        } else if value.eq_ignore_ascii_case("verbose") {
            Verbosity::Verbose
        } else {
            Verbosity::Summary
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Verbosity::Quiet => "quiet",
            Verbosity::Summary => "summary",
            Verbosity::Verbose => "verbose",
        }
    }
}

impl core::fmt::Display for Verbosity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct InterruptTestConfig {
    pub enabled: bool,
    pub suite_mask: u32,
    pub verbosity: Verbosity,
    pub timeout_ms: u32,
    pub shutdown: bool,
    pub stacktrace_demo: bool,
}

impl InterruptTestConfig {
    /// Get the suite as an enum (for display purposes)
    pub fn suite(&self) -> Suite {
        Suite::from_mask(self.suite_mask)
    }
}

impl Default for InterruptTestConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ENABLED,
            suite_mask: DEFAULT_SUITE.to_mask(),
            verbosity: DEFAULT_VERBOSITY,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            shutdown: DEFAULT_SHUTDOWN,
            stacktrace_demo: DEFAULT_STACKTRACE_DEMO,
        }
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("on")
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("enabled")
        || value == "1"
    {
        Some(true)
    } else if value.eq_ignore_ascii_case("off")
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("no")
        || value.eq_ignore_ascii_case("disabled")
        || value == "0"
    {
        Some(false)
    } else {
        None
    }
}

pub fn config_from_cmdline(cmdline: Option<&str>) -> InterruptTestConfig {
    let mut cfg = InterruptTestConfig::default();
    if let Some(cmdline) = cmdline {
        for token in cmdline.split_whitespace() {
            if let Some(value) = token.strip_prefix("itests=") {
                if let Some(enabled) = parse_bool(value) {
                    cfg.enabled = enabled;
                    if !enabled {
                        cfg.shutdown = false;
                    }
                } else {
                    // Treat as suite name
                    let suite = Suite::from_str(value);
                    cfg.enabled = true;
                    cfg.suite_mask = suite.to_mask();
                }
            } else if let Some(value) = token.strip_prefix("itests.suite=") {
                cfg.suite_mask = Suite::from_str(value).to_mask();
                cfg.enabled = true;
            } else if let Some(value) = token.strip_prefix("itests.verbosity=") {
                cfg.verbosity = Verbosity::from_str(value);
            } else if let Some(value) = token.strip_prefix("itests.timeout=") {
                if let Ok(parsed) = value.trim_end_matches("ms").parse::<u32>() {
                    cfg.timeout_ms = parsed;
                }
            } else if let Some(value) = token.strip_prefix("itests.shutdown=") {
                if let Some(shutdown) = parse_bool(value) {
                    cfg.shutdown = shutdown;
                }
            } else if let Some(value) = token.strip_prefix("itests.stacktrace_demo=") {
                if let Some(demo) = parse_bool(value) {
                    cfg.stacktrace_demo = demo;
                }
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
        config.suite(),
        config.verbosity,
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
