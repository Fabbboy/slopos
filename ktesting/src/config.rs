use slopos_sync::StateFlag;
use slopos_utils::klog_info;

const DEFAULT_ENABLED: bool = false;
const DEFAULT_VERBOSITY: Verbosity = Verbosity::Summary;
const DEFAULT_TIMEOUT_MS: u32 = 0;
const DEFAULT_SHUTDOWN: bool = false;
const DEFAULT_STACKTRACE_DEMO: bool = false;

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
pub struct TestConfig {
    pub enabled: bool,
    pub verbosity: Verbosity,
    pub timeout_ms: u32,
    pub shutdown: bool,
    pub stacktrace_demo: bool,
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_ENABLED,
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

/// One-shot guard ensuring the legacy `itests.*` cmdline warning fires at
/// most once per boot, even if the cmdline contains multiple legacy keys.
static LEGACY_WARNED: StateFlag = StateFlag::new();

fn warn_legacy_once() {
    if !LEGACY_WARNED.is_active() {
        LEGACY_WARNED.set_active();
        klog_info!("TESTS: legacy 'itests.*' cmdline key in use; rename to 'tests.*'");
    }
}

/// Match either a new `tests.<suffix>` prefix or its legacy `itests.<suffix>`
/// alias. Returns the value substring on match. Emits a one-shot legacy
/// warning if the legacy form was used.
fn match_dual_prefix<'a>(token: &'a str, suffix: &'static str) -> Option<&'a str> {
    if let Some(rest) = token.strip_prefix("tests.") {
        rest.strip_prefix(suffix)
    } else if let Some(rest) = token.strip_prefix("itests.") {
        if let Some(value) = rest.strip_prefix(suffix) {
            warn_legacy_once();
            Some(value)
        } else {
            None
        }
    } else {
        None
    }
}

pub fn config_from_cmdline(cmdline: Option<&str>) -> TestConfig {
    let mut cfg = TestConfig::default();
    if let Some(cmdline) = cmdline {
        for token in cmdline.split_whitespace() {
            if let Some(value) = token.strip_prefix("tests=") {
                if let Some(enabled) = parse_bool(value) {
                    cfg.enabled = enabled;
                    if !enabled {
                        cfg.shutdown = false;
                    }
                } else {
                    cfg.enabled = true;
                }
            } else if let Some(value) = token.strip_prefix("itests=") {
                warn_legacy_once();
                if let Some(enabled) = parse_bool(value) {
                    cfg.enabled = enabled;
                    if !enabled {
                        cfg.shutdown = false;
                    }
                } else {
                    cfg.enabled = true;
                }
            } else if match_dual_prefix(token, "suite=").is_some() {
                // Accepted for backward compatibility; all suites always run.
                cfg.enabled = true;
            } else if let Some(value) = match_dual_prefix(token, "verbosity=") {
                cfg.verbosity = Verbosity::from_str(value);
            } else if let Some(value) = match_dual_prefix(token, "timeout=") {
                if let Ok(parsed) = value.trim_end_matches("ms").parse::<u32>() {
                    cfg.timeout_ms = parsed;
                }
            } else if let Some(value) = match_dual_prefix(token, "warn_ms=") {
                if let Ok(parsed) = value.trim_end_matches("ms").parse::<u32>() {
                    cfg.timeout_ms = parsed;
                }
            } else if let Some(value) = match_dual_prefix(token, "shutdown=") {
                if let Some(shutdown) = parse_bool(value) {
                    cfg.shutdown = shutdown;
                }
            } else if let Some(value) = match_dual_prefix(token, "stacktrace_demo=") {
                if let Some(demo) = parse_bool(value) {
                    cfg.stacktrace_demo = demo;
                }
            }
        }
    }
    cfg
}
