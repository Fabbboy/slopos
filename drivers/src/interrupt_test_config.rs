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
    match value.to_ascii_lowercase().as_str() {
        "quiet" => interrupt_test_verbosity::INTERRUPT_TEST_VERBOSITY_QUIET,
        "verbose" => interrupt_test_verbosity::INTERRUPT_TEST_VERBOSITY_VERBOSE,
        _ => interrupt_test_verbosity::INTERRUPT_TEST_VERBOSITY_SUMMARY,
    }
}

fn suite_from_string(value: &str) -> u32 {
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "none" | "off" => 0,
        "all" => INTERRUPT_TEST_SUITE_ALL,
        "basic" => INTERRUPT_TEST_SUITE_BASIC,
        "memory" => INTERRUPT_TEST_SUITE_MEMORY,
        "control" => INTERRUPT_TEST_SUITE_CONTROL,
        "basic+memory" | "memory+basic" => INTERRUPT_TEST_SUITE_BASIC | INTERRUPT_TEST_SUITE_MEMORY,
        "basic+control" | "control+basic" => INTERRUPT_TEST_SUITE_BASIC | INTERRUPT_TEST_SUITE_CONTROL,
        "memory+control" | "control+memory" => INTERRUPT_TEST_SUITE_MEMORY | INTERRUPT_TEST_SUITE_CONTROL,
        _ => INTERRUPT_TEST_SUITE_ALL,
    }
}

fn parse_on_off_flag(value: &str, current: c_int) -> c_int {
    match value.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "enabled" | "1" => 1,
        "off" | "false" | "no" | "disabled" | "0" => 0,
        _ => current,
    }
}

fn apply_enable_token(config: &mut interrupt_test_config, value: &str) {
    let lower = value.to_ascii_lowercase();
    match lower.as_str() {
        "on" | "true" | "enabled" => {
            config.enabled = 1;
            return;
        }
        "off" | "false" | "disabled" => {
            config.enabled = 0;
            config.shutdown_on_complete = 0;
            return;
        }
        _ => {}
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

