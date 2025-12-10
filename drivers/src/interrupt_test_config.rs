#![allow(dead_code)]
#![allow(non_camel_case_types)]

use core::ffi::CStr;
use core::ffi::{c_char};

pub const INTERRUPT_TESTS_DEFAULT_ENABLED: bool = false;
pub const INTERRUPT_TESTS_DEFAULT_TIMEOUT_MS: u32 = 0;
pub const INTERRUPT_TESTS_DEFAULT_SUITE: &str = "all";
pub const INTERRUPT_TESTS_DEFAULT_VERBOSITY: &str = "summary";
pub const INTERRUPT_TESTS_DEFAULT_SHUTDOWN: bool = false;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub enum interrupt_test_verbosity {
    INTERRUPT_TEST_VERBOSITY_QUIET = 0,
    INTERRUPT_TEST_VERBOSITY_SUMMARY = 1,
    INTERRUPT_TEST_VERBOSITY_VERBOSE = 2,
}

pub const INTERRUPT_TEST_SUITE_BASIC: u32 = 1 << 0;
pub const INTERRUPT_TEST_SUITE_MEMORY: u32 = 1 << 1;
pub const INTERRUPT_TEST_SUITE_CONTROL: u32 = 1 << 2;
pub const INTERRUPT_TEST_SUITE_SCHEDULER: u32 = 1 << 3;
pub const INTERRUPT_TEST_SUITE_ALL: u32 = INTERRUPT_TEST_SUITE_BASIC
    | INTERRUPT_TEST_SUITE_MEMORY
    | INTERRUPT_TEST_SUITE_CONTROL
    | INTERRUPT_TEST_SUITE_SCHEDULER;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct interrupt_test_config {
    pub enabled: c_int,
    pub verbosity: interrupt_test_verbosity,
    pub suite_mask: u32,
    pub timeout_ms: u32,
    pub shutdown_on_complete: c_int,
    pub stacktrace_demo: c_int,
}

impl Default for interrupt_test_config {
    fn default() -> Self {
        Self {
            enabled: if INTERRUPT_TESTS_DEFAULT_ENABLED { 1 } else { 0 },
            verbosity: verbosity_from_string(INTERRUPT_TESTS_DEFAULT_VERBOSITY),
            suite_mask: suite_from_string(INTERRUPT_TESTS_DEFAULT_SUITE),
            timeout_ms: INTERRUPT_TESTS_DEFAULT_TIMEOUT_MS,
            shutdown_on_complete: if INTERRUPT_TESTS_DEFAULT_SHUTDOWN { 1 } else { 0 },
            stacktrace_demo: 0,
        }
    }
}

type c_int = i32;

fn verbosity_from_string(value: &str) -> interrupt_test_verbosity {
    if value.eq_ignore_ascii_case("quiet") {
        interrupt_test_verbosity::INTERRUPT_TEST_VERBOSITY_QUIET
    } else if value.eq_ignore_ascii_case("verbose") {
        interrupt_test_verbosity::INTERRUPT_TEST_VERBOSITY_VERBOSE
    } else {
        interrupt_test_verbosity::INTERRUPT_TEST_VERBOSITY_SUMMARY
    }
}

fn suite_from_string(value: &str) -> u32 {
    if value.eq_ignore_ascii_case("none") || value.eq_ignore_ascii_case("off") {
        0
    } else if value.eq_ignore_ascii_case("all") {
        INTERRUPT_TEST_SUITE_ALL
    } else if value.eq_ignore_ascii_case("basic") {
        INTERRUPT_TEST_SUITE_BASIC
    } else if value.eq_ignore_ascii_case("memory") {
        INTERRUPT_TEST_SUITE_MEMORY
    } else if value.eq_ignore_ascii_case("control") {
        INTERRUPT_TEST_SUITE_CONTROL
    } else if value.eq_ignore_ascii_case("basic+memory") || value.eq_ignore_ascii_case("memory+basic") {
        INTERRUPT_TEST_SUITE_BASIC | INTERRUPT_TEST_SUITE_MEMORY
    } else if value.eq_ignore_ascii_case("basic+control") || value.eq_ignore_ascii_case("control+basic") {
        INTERRUPT_TEST_SUITE_BASIC | INTERRUPT_TEST_SUITE_CONTROL
    } else if value.eq_ignore_ascii_case("memory+control") || value.eq_ignore_ascii_case("control+memory") {
        INTERRUPT_TEST_SUITE_MEMORY | INTERRUPT_TEST_SUITE_CONTROL
    } else {
        INTERRUPT_TEST_SUITE_ALL
    }
}

fn parse_on_off_flag(value: &str, current: c_int) -> c_int {
    if value.eq_ignore_ascii_case("on")
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("enabled")
        || value.eq_ignore_ascii_case("1")
    {
        1
    } else if value.eq_ignore_ascii_case("off")
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("no")
        || value.eq_ignore_ascii_case("disabled")
        || value.eq_ignore_ascii_case("0")
    {
        0
    } else {
        current
    }
}

fn apply_enable_token(config: &mut interrupt_test_config, value: &str) {
    if value.eq_ignore_ascii_case("on") || value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("enabled") {
        config.enabled = 1;
        return;
    } else if value.eq_ignore_ascii_case("off")
        || value.eq_ignore_ascii_case("false")
        || value.eq_ignore_ascii_case("disabled")
    {
        config.enabled = 0;
        config.shutdown_on_complete = 0;
        return;
    }

    let suite = suite_from_string(value);
    if suite != 0 {
        config.enabled = 1;
        config.suite_mask = suite;
    } else {
        config.enabled = 0;
        config.suite_mask = 0;
        config.shutdown_on_complete = 0;
    }
}

fn process_token(config: &mut interrupt_test_config, token: &str) {
    if let Some(value) = token.strip_prefix("itests=") {
        apply_enable_token(config, value);
        return;
    }
    if let Some(value) = token.strip_prefix("interrupt_tests=") {
        apply_enable_token(config, value);
        return;
    }
    if let Some(value) = token.strip_prefix("itests.suite=") {
        let suite = suite_from_string(value);
        config.suite_mask = suite;
        if suite != 0 {
            config.enabled = 1;
        }
        return;
    }
    if let Some(value) = token.strip_prefix("interrupt_tests.suite=") {
        let suite = suite_from_string(value);
        config.suite_mask = suite;
        if suite != 0 {
            config.enabled = 1;
        }
        return;
    }
    if let Some(value) = token.strip_prefix("itests.verbosity=") {
        config.verbosity = verbosity_from_string(value);
        return;
    }
    if let Some(value) = token.strip_prefix("interrupt_tests.verbosity=") {
        config.verbosity = verbosity_from_string(value);
        return;
    }
    if let Some(value) = token.strip_prefix("itests.timeout=") {
        if let Ok(parsed) = value.parse::<u32>() {
            config.timeout_ms = parsed;
        }
        return;
    }
    if let Some(value) = token.strip_prefix("interrupt_tests.timeout=") {
        if let Ok(parsed) = value.parse::<u32>() {
            config.timeout_ms = parsed;
        }
        return;
    }
    if let Some(value) = token.strip_prefix("itests.shutdown=") {
        config.shutdown_on_complete = parse_on_off_flag(value, config.shutdown_on_complete);
        return;
    }
    if let Some(value) = token.strip_prefix("interrupt_tests.shutdown=") {
        config.shutdown_on_complete = parse_on_off_flag(value, config.shutdown_on_complete);
        return;
    }
    if let Some(value) = token.strip_prefix("itests.stacktrace_demo=") {
        config.stacktrace_demo = parse_on_off_flag(value, config.stacktrace_demo);
        return;
    }
    if let Some(value) = token.strip_prefix("interrupt_tests.stacktrace_demo=") {
        config.stacktrace_demo = parse_on_off_flag(value, config.stacktrace_demo);
        return;
    }
}

#[no_mangle]
pub extern "C" fn interrupt_test_config_init_defaults(config: *mut interrupt_test_config) {
    if config.is_null() {
        return;
    }
    unsafe {
        *config = interrupt_test_config::default();
    }
}

#[no_mangle]
pub extern "C" fn interrupt_test_config_parse_cmdline(
    config: *mut interrupt_test_config,
    cmdline: *const c_char,
) {
    if config.is_null() || cmdline.is_null() {
        return;
    }

    let raw = unsafe { CStr::from_ptr(cmdline) };
    if let Ok(cmd) = raw.to_str() {
        let mut cfg = unsafe { *config };
        for token in cmd.split_whitespace() {
            process_token(&mut cfg, token);
        }
        unsafe {
            *config = cfg;
        }
    }
}

#[no_mangle]
pub extern "C" fn interrupt_test_verbosity_string(
    verbosity: interrupt_test_verbosity,
) -> *const c_char {
    match verbosity {
        interrupt_test_verbosity::INTERRUPT_TEST_VERBOSITY_QUIET => b"quiet\0".as_ptr() as *const c_char,
        interrupt_test_verbosity::INTERRUPT_TEST_VERBOSITY_VERBOSE => b"verbose\0".as_ptr() as *const c_char,
        _ => b"summary\0".as_ptr() as *const c_char,
    }
}

#[no_mangle]
pub extern "C" fn interrupt_test_suite_string(suite_mask: u32) -> *const c_char {
    match suite_mask {
        0 => b"none\0".as_ptr() as *const c_char,
        x if x == INTERRUPT_TEST_SUITE_ALL => b"all\0".as_ptr() as *const c_char,
        x if x == INTERRUPT_TEST_SUITE_BASIC => b"basic\0".as_ptr() as *const c_char,
        x if x == INTERRUPT_TEST_SUITE_MEMORY => b"memory\0".as_ptr() as *const c_char,
        x if x == INTERRUPT_TEST_SUITE_CONTROL => b"control\0".as_ptr() as *const c_char,
        x if x == (INTERRUPT_TEST_SUITE_BASIC | INTERRUPT_TEST_SUITE_MEMORY) => {
            b"basic+memory\0".as_ptr() as *const c_char
        }
        x if x == (INTERRUPT_TEST_SUITE_BASIC | INTERRUPT_TEST_SUITE_CONTROL) => {
            b"basic+control\0".as_ptr() as *const c_char
        }
        x if x == (INTERRUPT_TEST_SUITE_MEMORY | INTERRUPT_TEST_SUITE_CONTROL) => {
            b"memory+control\0".as_ptr() as *const c_char
        }
        _ => b"custom\0".as_ptr() as *const c_char,
    }
}

