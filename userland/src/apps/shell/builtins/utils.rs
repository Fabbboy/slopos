//! Utility builtins: sleep, true, false, seq, yes, random, roulette, wl.

use crate::runtime;
use crate::syscall::{UserSysInfo, core as sys_core, roulette};
use std::thread;
use std::time::Duration;

use super::super::NL;
use super::super::display::{COLOR_ERROR_RED, shell_write, shell_write_idx};
use super::super::jobs::{arg_as_str, parse_u32_arg, write_u64};

fn parse_u64_arg(ptr: *const u8) -> Option<u64> {
    arg_as_str(ptr)?.parse().ok()
}

// ─── Commands ───────────────────────────────────────────────────────────────

pub fn cmd_sleep(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write_idx(b"sleep: missing operand (milliseconds)\n", COLOR_ERROR_RED);
        return 1;
    }
    let Some(ms) = parse_u32_arg(argv[1]) else {
        shell_write_idx(b"sleep: invalid number\n", COLOR_ERROR_RED);
        return 1;
    };
    if ms == 0 {
        return 0;
    }
    thread::sleep(Duration::from_millis(ms as u64));
    0
}

pub fn cmd_true(_argc: i32, _argv: &[*const u8]) -> i32 {
    0
}

pub fn cmd_false(_argc: i32, _argv: &[*const u8]) -> i32 {
    1
}

pub fn cmd_seq(argc: i32, argv: &[*const u8]) -> i32 {
    if argc < 2 {
        shell_write_idx(b"seq: missing operand\n", COLOR_ERROR_RED);
        return 1;
    }

    let (start, end) = if argc >= 3 {
        let Some(s) = parse_u64_arg(argv[1]) else {
            shell_write_idx(b"seq: invalid start\n", COLOR_ERROR_RED);
            return 1;
        };
        let Some(e) = parse_u64_arg(argv[2]) else {
            shell_write_idx(b"seq: invalid end\n", COLOR_ERROR_RED);
            return 1;
        };
        (s, e)
    } else {
        let Some(e) = parse_u64_arg(argv[1]) else {
            shell_write_idx(b"seq: invalid number\n", COLOR_ERROR_RED);
            return 1;
        };
        (1u64, e)
    };

    if start > end {
        return 0;
    }

    let mut i = start;
    loop {
        write_u64(i);
        if !shell_write(NL.as_bytes()) {
            break;
        }
        if i == end {
            break;
        }
        i += 1;
    }
    0
}

pub fn cmd_yes(argc: i32, argv: &[*const u8]) -> i32 {
    const MAX_ITERATIONS: u32 = 100_000;

    let text: &[u8] = if argc >= 2 && !argv[1].is_null() {
        let len = runtime::u_strlen(argv[1]);
        if len > 0 {
            unsafe { core::slice::from_raw_parts(argv[1], len) }
        } else {
            b"y"
        }
    } else {
        b"y"
    };

    for _ in 0..MAX_ITERATIONS {
        if !shell_write(text) || !shell_write(NL.as_bytes()) {
            break;
        }
        sys_core::yield_now();
    }
    0
}

pub fn cmd_random(argc: i32, argv: &[*const u8]) -> i32 {
    let raw = sys_core::random_next();
    let value = if argc >= 2 {
        let Some(max) = parse_u32_arg(argv[1]) else {
            shell_write_idx(b"random: invalid max\n", COLOR_ERROR_RED);
            return 1;
        };
        if max == 0 {
            shell_write_idx(b"random: max must be > 0\n", COLOR_ERROR_RED);
            return 1;
        }
        raw % max
    } else {
        raw
    };
    write_u64(value as u64);
    shell_write(NL.as_bytes());
    0
}

pub fn cmd_roulette(_argc: i32, _argv: &[*const u8]) -> i32 {
    shell_write(b"=== WHEEL OF FATE ===\n");
    shell_write(b"Spinning...\n");

    let spin = roulette::spin();
    let fate = spin as u32;

    thread::sleep(Duration::from_millis(200));

    shell_write(b"Fate number: ");
    write_u64(fate as u64);
    shell_write(NL.as_bytes());

    let is_win = (fate & 1) == 1;

    if is_win {
        shell_write(b"The Wheel smiles upon you. W +10\n");
    } else {
        shell_write(b"The Wheel demands its toll. Rebooting...\n");
    }

    // On loss the kernel reboots — this call may not return.
    roulette::result(spin);

    0
}

pub fn cmd_wl(_argc: i32, _argv: &[*const u8]) -> i32 {
    let mut info = UserSysInfo::default();
    if sys_core::sys_info(&mut info) != 0 {
        shell_write_idx(b"wl: failed to query balance\n", COLOR_ERROR_RED);
        return 1;
    }

    let balance = info.wl_balance;

    shell_write(format!("W/L Balance: {balance}\n").as_bytes());

    if balance > 100 {
        shell_write(b"The Wheel favors the bold.\n");
    } else if balance > 0 {
        shell_write(b"Fate is cautiously on your side.\n");
    } else if balance == 0 {
        shell_write(b"Perfectly balanced, as all slop should be.\n");
    } else if balance > -100 {
        shell_write(b"The house is winning. Spin again?\n");
    } else {
        shell_write(b"Deep in the red. The Wheel remembers.\n");
    }
    0
}
