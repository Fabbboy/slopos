#![feature(restricted_std)]

//! libc signal()/sigaction() handler-install end-to-end test.
//!
//! Regression guard for the EINVAL-on-install footgun: slibc's `signal()`
//! and `sigaction()` used to leave `sa_restorer` at 0, which the kernel
//! rejects for any catchable handler (it requires a nonzero restorer and
//! bails out of delivery when it is 0). libc must inject its own restorer
//! trampoline — exactly what glibc does. These cases install a real handler
//! through libc, `raise()` the signal, and prove the handler actually ran
//! by observing a volatile flag it sets.

// Pull in the `slopos-userland` lib crate so its `_start` ELF entry point
// is linked into the binary (same requirement as the sibling test bins;
// without it the linker emits entry 0x0 and `do_exec` rejects the ELF).
use slopos_userland as _;

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::signal::UserSigaction;
use slopos_slibc::signal::{self, SIG_DFL, SIGUSR1, SIGUSR2};

static SIGUSR1_COUNT: AtomicU32 = AtomicU32::new(0);
static SIGUSR2_COUNT: AtomicU32 = AtomicU32::new(0);
static CLOBBER_RAN: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_sigusr1(_sig: i32) {
    SIGUSR1_COUNT.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn on_sigusr2(_sig: i32) {
    SIGUSR2_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// Signal handler that deliberately poisons the vector register file. If
/// the kernel preserves the interrupted task's FPU/vector state across
/// signal delivery, sigreturn undoes this clobber and the interrupted
/// code never sees it.
extern "C" fn on_clobber_vectors(_sig: i32) {
    CLOBBER_RAN.fetch_add(1, Ordering::SeqCst);
    let poison: [u32; 4] = [0xDEAD_BEEF; 4];
    // SAFETY: overwrites xmm0..xmm7 with poison; all are caller-saved in
    // the SysV ABI, so the handler may clobber them freely.
    unsafe {
        core::arch::asm!(
            "movups xmm0, [{p}]",
            "movups xmm1, [{p}]",
            "movups xmm2, [{p}]",
            "movups xmm3, [{p}]",
            "movups xmm4, [{p}]",
            "movups xmm5, [{p}]",
            "movups xmm6, [{p}]",
            "movups xmm7, [{p}]",
            p = in(reg) poison.as_ptr(),
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
        );
    }
}

/// `signal()` must install a real handler (not fail with SIG_ERR) and the
/// handler must run when the signal is raised.
fn test_signal_installs_and_delivers() -> bool {
    SIGUSR1_COUNT.store(0, Ordering::SeqCst);

    let prev = unsafe { signal::signal(SIGUSR1, on_sigusr1 as *const () as usize) };
    if prev == usize::MAX {
        eprintln!("signal_handler_test: signal() returned SIG_ERR (install rejected)");
        return false;
    }

    if unsafe { signal::raise(SIGUSR1) } != 0 {
        eprintln!("signal_handler_test: raise(SIGUSR1) failed");
        return false;
    }

    let count = SIGUSR1_COUNT.load(Ordering::SeqCst);
    if count != 1 {
        eprintln!("signal_handler_test: handler ran {count} times, expected 1");
        return false;
    }

    // Restore default so a stray later signal doesn't re-enter the handler.
    let _ = unsafe { signal::signal(SIGUSR1, SIG_DFL) };
    true
}

/// `sigaction()` with `sa_restorer == 0` on a real handler must have libc
/// substitute its own restorer (glibc behavior), so the install succeeds and
/// the handler runs.
fn test_sigaction_injects_restorer() -> bool {
    SIGUSR2_COUNT.store(0, Ordering::SeqCst);

    let act = UserSigaction {
        sa_handler: on_sigusr2 as *const () as usize as u64,
        sa_flags: 0,
        sa_restorer: 0,
        sa_mask: 0,
    };

    let rc = unsafe { signal::sigaction(SIGUSR2, &act, core::ptr::null_mut()) };
    if rc != 0 {
        eprintln!("signal_handler_test: sigaction() returned {rc} (restorer not injected)");
        return false;
    }

    if unsafe { signal::raise(SIGUSR2) } != 0 {
        eprintln!("signal_handler_test: raise(SIGUSR2) failed");
        return false;
    }

    let count = SIGUSR2_COUNT.load(Ordering::SeqCst);
    if count != 1 {
        eprintln!("signal_handler_test: sigaction handler ran {count} times, expected 1");
        return false;
    }

    let _ = unsafe { signal::signal(SIGUSR2, SIG_DFL) };
    true
}

/// The kernel must preserve a task's vector (XMM/YMM) registers across
/// signal delivery: a handler that uses SSE/AVX must not corrupt the
/// interrupted code's state. Loads a known pattern into xmm0..xmm3,
/// raises SIGUSR1 (handler poisons the vector file) via an INLINE kill
/// syscall so the registers stay live across delivery, then reads them
/// back — they must still hold the pattern.
fn test_signal_preserves_vector_regs() -> bool {
    CLOBBER_RAN.store(0, Ordering::SeqCst);
    let prev = unsafe { signal::signal(SIGUSR1, on_clobber_vectors as *const () as usize) };
    if prev == usize::MAX {
        eprintln!("signal_handler_test: vector-preserve handler install rejected");
        return false;
    }

    let pid = unsafe { slopos_slibc::process::getpid() } as u64;
    let pattern: [u32; 16] = [
        0x1111_1111,
        0x2222_2222,
        0x3333_3333,
        0x4444_4444,
        0x5555_5555,
        0x6666_6666,
        0x7777_7777,
        0x8888_8888,
        0x9999_9999,
        0xAAAA_AAAA,
        0xBBBB_BBBB,
        0xCCCC_CCCC,
        0xDDDD_DDDD,
        0xEEEE_EEEE,
        0x0F0F_0F0F,
        0xF0F0_F0F0,
    ];
    let mut result: [u32; 16] = [0; 16];

    // SAFETY: one asm block so the vector regs stay live across delivery:
    // load xmm0..3 from `pattern`, run kill(pid, SIGUSR1) inline (rax=104),
    // then store xmm0..3 into `result`. rcx/r11 are syscall-clobbered;
    // xmm0..3 are listed as outputs so the compiler reloads from `result`.
    unsafe {
        core::arch::asm!(
            "movups xmm0, [{pat}]",
            "movups xmm1, [{pat} + 16]",
            "movups xmm2, [{pat} + 32]",
            "movups xmm3, [{pat} + 48]",
            "syscall",
            "movups [{res}], xmm0",
            "movups [{res} + 16], xmm1",
            "movups [{res} + 32], xmm2",
            "movups [{res} + 48], xmm3",
            pat = in(reg) pattern.as_ptr(),
            res = in(reg) result.as_mut_ptr(),
            inout("rax") 104u64 => _,
            in("rdi") pid,
            in("rsi") SIGUSR1 as u64,
            out("rcx") _,
            out("r11") _,
            out("xmm0") _,
            out("xmm1") _,
            out("xmm2") _,
            out("xmm3") _,
        );
    }

    let _ = unsafe { signal::signal(SIGUSR1, SIG_DFL) };

    if CLOBBER_RAN.load(Ordering::SeqCst) != 1 {
        eprintln!("signal_handler_test: clobber handler did not run exactly once");
        return false;
    }
    if result != pattern {
        eprintln!("signal_handler_test: vector regs corrupted across signal: {result:08x?}");
        return false;
    }
    true
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "signal_installs_and_delivers",
        test_signal_installs_and_delivers,
    ),
    (
        "sigaction_injects_restorer",
        test_sigaction_injects_restorer,
    ),
    (
        "signal_preserves_vector_regs",
        test_signal_preserves_vector_regs,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
