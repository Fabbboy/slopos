use slopos_alloc::KVec;
use slopos_sync::StateFlag;
use slopos_utils::klog_info;

const DEFAULT_ENABLED: bool = false;
const DEFAULT_VERBOSITY: Verbosity = Verbosity::Summary;
const DEFAULT_WARN_MS: u32 = 0;
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

/// Test-harness runtime configuration.
///
/// Globs are stored owned (`KVec<u8>`) so the source of each pattern —
/// cmdline buffer slice or synthesised legacy alias — is irrelevant to
/// the matcher.
#[derive(Debug, Default)]
pub struct TestConfig {
    pub enabled: bool,
    pub verbosity: Verbosity,
    pub warn_ms: u32,
    pub shutdown: bool,
    pub stacktrace_demo: bool,
    pub run_globs: KVec<KVec<u8>>,
    pub skip_globs: KVec<KVec<u8>>,
}

impl Default for Verbosity {
    fn default() -> Self {
        DEFAULT_VERBOSITY
    }
}

impl TestConfig {
    /// True iff `name` should run under the current filter.
    pub fn passes_filter(&self, name: &[u8]) -> bool {
        let run_match = self.run_globs.is_empty()
            || self
                .run_globs
                .iter()
                .any(|p| crate::filter::glob_match(p.as_slice(), name));
        if !run_match {
            return false;
        }
        let skip_match = self
            .skip_globs
            .iter()
            .any(|p| crate::filter::glob_match(p.as_slice(), name));
        !skip_match
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

fn push_owned_glob(target: &mut KVec<KVec<u8>>, pattern: &[u8]) {
    let mut owned = KVec::<u8>::new();
    for &b in pattern {
        if owned.push(b).is_err() {
            return;
        }
    }
    let _ = target.push(owned);
}

pub fn config_from_cmdline(cmdline: Option<&str>) -> TestConfig {
    let mut cfg = TestConfig {
        enabled: DEFAULT_ENABLED,
        verbosity: DEFAULT_VERBOSITY,
        warn_ms: DEFAULT_WARN_MS,
        shutdown: DEFAULT_SHUTDOWN,
        stacktrace_demo: DEFAULT_STACKTRACE_DEMO,
        run_globs: KVec::new(),
        skip_globs: KVec::new(),
    };
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
            } else if let Some(value) = match_dual_prefix(token, "suite=") {
                cfg.enabled = true;
                push_suite_glob(&mut cfg.run_globs, value);
            } else if let Some(value) = match_dual_prefix(token, "verbosity=") {
                cfg.verbosity = Verbosity::from_str(value);
            } else if let Some(value) = match_dual_prefix(token, "timeout=") {
                if let Ok(parsed) = value.trim_end_matches("ms").parse::<u32>() {
                    cfg.warn_ms = parsed;
                }
            } else if let Some(value) = match_dual_prefix(token, "warn_ms=") {
                if let Ok(parsed) = value.trim_end_matches("ms").parse::<u32>() {
                    cfg.warn_ms = parsed;
                }
            } else if let Some(value) = match_dual_prefix(token, "run=") {
                for piece in value.split(',') {
                    if !piece.is_empty() {
                        push_owned_glob(&mut cfg.run_globs, piece.as_bytes());
                    }
                }
            } else if let Some(value) = match_dual_prefix(token, "skip=") {
                for piece in value.split(',') {
                    if !piece.is_empty() {
                        push_owned_glob(&mut cfg.skip_globs, piece.as_bytes());
                    }
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

/// Translate `tests.suite=foo` (legacy alias) into the glob `*foo*` so
/// that any test fully-qualified name containing `foo` is admitted.
fn push_suite_glob(target: &mut KVec<KVec<u8>>, suite: &str) {
    let mut owned = KVec::<u8>::new();
    if owned.push(b'*').is_err() {
        return;
    }
    for &b in suite.as_bytes() {
        if owned.push(b).is_err() {
            return;
        }
    }
    if owned.push(b'*').is_err() {
        return;
    }
    let _ = target.push(owned);
}
