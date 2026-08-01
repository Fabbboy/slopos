use core::ffi::c_int;
use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use slopos_arch::cpu;
use slopos_drivers::keyboard::poll_wait_enter;
use slopos_mm::memory_init::is_memory_system_initialized;
use slopos_ostd::panic_recovery;
use slopos_ostd::stacktrace::{self, StacktraceEntry};
use slopos_ostd::sync::StateFlag;
use slopos_video::panic_screen;

use crate::shutdown::execute_kernel;

static PANIC_RIP: AtomicU64 = AtomicU64::new(0);
static PANIC_RSP: AtomicU64 = AtomicU64::new(0);
/// RBP of the *interrupted* context (from the trap frame) when the panic
/// originates in an exception handler. The backtrace walker prefers this
/// over `PANIC_ORIG_RBP` so a kernel-mode fault prints the faulting call
/// chain instead of the panic machinery's own frames.
static PANIC_FRAME_RBP: AtomicU64 = AtomicU64::new(0);
static PANIC_HAS_CPU_STATE: StateFlag = StateFlag::new();
const PANIC_BACKTRACE_MAX: usize = 16;

/// The panicking `&PanicInfo` re-narrowed to a raw pointer, stashed before the
/// emergency-stack switch so the reporter (running on the emergency stack) can
/// format the full location + message. The PanicInfo memory stays live — the
/// switch only moves `RSP`, it does not unwind the panicking frame.
static PANIC_INFO_PTR: AtomicUsize = AtomicUsize::new(0);
/// Original `RSP` captured before the switch (the reporter's own `RSP` is the
/// emergency stack, so the interrupted stack pointer must be stashed).
static PANIC_ORIG_RSP: AtomicU64 = AtomicU64::new(0);
/// Original `RBP` captured before the switch, so the backtrace walks the
/// interrupted call chain (the panic origin) rather than the reporter's own
/// frames on the emergency stack.
static PANIC_ORIG_RBP: AtomicU64 = AtomicU64::new(0);

/// Bounded spin budget the owner waits for peer CPUs to acknowledge the panic
/// stop before printing; a stuck peer must never block the report, so the
/// owner proceeds on timeout.
const PEER_STOP_SPIN_BUDGET: u64 = 50_000_000;

/// Set CPU state from an interrupt frame to be included in panic diagnostics.
/// `rbp` is the interrupted context's frame pointer; the panic backtrace
/// walks it so the report shows the faulting call chain.
#[inline]
pub fn set_panic_cpu_state(rip: u64, rsp: u64, rbp: u64) {
    PANIC_RIP.store(rip, Ordering::SeqCst);
    PANIC_RSP.store(rsp, Ordering::SeqCst);
    PANIC_FRAME_RBP.store(rbp, Ordering::SeqCst);
    PANIC_HAS_CPU_STATE.set_active();
}

fn take_panic_cpu_state() -> (Option<u64>, Option<u64>) {
    if PANIC_HAS_CPU_STATE.take() {
        (
            Some(PANIC_RIP.load(Ordering::SeqCst)),
            Some(PANIC_RSP.load(Ordering::SeqCst)),
        )
    } else {
        (None, None)
    }
}

fn panic_serial_write(s: &str) {
    // Write directly to COM1 via the lock-free early console rather than the
    // `SERIAL` spinlock. A CPU that faulted while holding `SERIAL` would
    // otherwise self-deadlock the moment it panics, and concurrent peer
    // panics would contend on the same lock — the cross-CPU wedge observed
    // alongside the recursive-panic cascade. `early_console` polls the UART
    // directly, so it is safe from any context.
    slopos_ostd::early_console::write_bytes(s.as_bytes());
    slopos_ostd::early_console::write_bytes(b"\n");
    // One emitted line is real progress, and a panic report — up to 16
    // symbolized backtrace frames over a polled UART, two port traps per
    // byte — runs long enough to outlast a timer tick when the panic
    // originated under an interrupt-disabling lock. The loop above is
    // bounded by the string already in hand and waits on nothing, so the
    // touch cannot mask a wedge: a dead UART stops it inside `write_bytes`,
    // before this line.
    slopos_ostd::watchdog::touch();
}

/// Last-resort, **format-free** abort: print a fixed `&'static str` and halt
/// without ever building a `format_args!` value.
///
/// `format_args!`/`write!` materialise a `[core::fmt::Argument; N]` array as
/// an address-taken local, i.e. on the SafeStack *data* stack. That is fine
/// for the normal panic path — exception context now runs on its own mapped
/// per-CPU data stack — but it is precisely wrong for the one case where the
/// **exception data stack itself overflowed**: the normal panic path would
/// re-fault on the very stack that is exhausted. This routine writes only
/// pre-existing `&'static str`s through the serial line (no Argument array,
/// no stack buffer), so it consumes no data stack and cannot recurse.
///
/// Diverges. Interrupts are masked first so a nested IRQ cannot perturb the
/// halt.
pub fn panic_abort_raw(msg: &'static str) -> ! {
    cpu::disable_interrupts();
    // Best-effort: become the panic owner so a concurrent normal panic on
    // another CPU does not interleave; ignore the result either way.
    let _ = slopos_ostd::panic::claim_panic_owner(slopos_arch::get_current_cpu() as u32);
    panic_serial_write("\n\n=== KERNEL ABORT ===");
    panic_serial_write(msg);
    panic_serial_write("System halted.");
    cpu::halt_loop()
}

/// Capture the panicking call chain's return addresses into `out` by walking
/// frame pointers from the stashed rbp. Returns the frame count.
fn panic_capture_backtrace(out: &mut [u64]) -> usize {
    let frame_rbp = PANIC_FRAME_RBP.load(Ordering::SeqCst);
    let stashed = PANIC_ORIG_RBP.load(Ordering::SeqCst);
    let rbp = if frame_rbp != 0 {
        frame_rbp
    } else if stashed != 0 {
        stashed
    } else {
        cpu::read_rbp()
    };
    let mut entries: [StacktraceEntry; PANIC_BACKTRACE_MAX] = [StacktraceEntry {
        frame_pointer: 0,
        return_address: 0,
    }; PANIC_BACKTRACE_MAX];
    let captured = stacktrace::stacktrace_capture_from(
        rbp,
        entries.as_mut_ptr(),
        PANIC_BACKTRACE_MAX as c_int,
    );
    if captured <= 0 {
        return 0;
    }
    let n = (captured as usize).min(out.len());
    for (slot, entry) in out[..n].iter_mut().zip(entries.iter()) {
        *slot = entry.return_address;
    }
    n
}

fn panic_dump_backtrace() {
    // Fatal-path rbp selection: prefer the trap frame's RBP (the faulting
    // context) when an exception path stashed one; then the pre-switch RBP
    // (the panicking call chain); finally the live RBP.
    let frame_rbp = PANIC_FRAME_RBP.load(Ordering::SeqCst);
    let stashed = PANIC_ORIG_RBP.load(Ordering::SeqCst);
    let rbp = if frame_rbp != 0 {
        frame_rbp
    } else if stashed != 0 {
        stashed
    } else {
        cpu::read_rbp()
    };
    panic_dump_backtrace_from(rbp)
}

/// Walk frame pointers from `rbp`, symbolize each return address, and print
/// the trace via the lock-free serial writer. Shared by the fatal reporter
/// and the task-scoped recovery report. All frames are printed, including
/// the panic machinery's own — they symbolize to self-identifying names and
/// a fixed skip count would silently rot as the handler's call shape
/// changes.
fn panic_dump_backtrace_from(rbp: u64) {
    let mut entries: [StacktraceEntry; PANIC_BACKTRACE_MAX] = [StacktraceEntry {
        frame_pointer: 0,
        return_address: 0,
    }; PANIC_BACKTRACE_MAX];

    let captured = stacktrace::stacktrace_capture_from(
        rbp,
        entries.as_mut_ptr(),
        PANIC_BACKTRACE_MAX as c_int,
    );
    if captured <= 0 {
        panic_serial_write("Backtrace: <empty>");
        return;
    }

    panic_serial_write("Backtrace (most recent call first):");
    for i in 0..captured as usize {
        let entry = &entries[i];
        let mut line = MessageBuffer::new();
        if let Some(sym) = slopos_ostd::ksym::lookup(entry.return_address) {
            let _ = write!(
                line,
                "  #{} rbp=0x{:016x} rip=0x{:016x} {}+0x{:x}",
                i, entry.frame_pointer, entry.return_address, sym.symbol, sym.offset
            );
        } else {
            let _ = write!(
                line,
                "  #{} rbp=0x{:016x} rip=0x{:016x}",
                i, entry.frame_pointer, entry.return_address
            );
        }
        panic_serial_write(line.as_str());
    }
}

/// Core panic implementation. Called by the kernel's `#[panic_handler]`.
pub fn panic_handler_impl(info: &PanicInfo) -> ! {
    let prior_in_flight = slopos_ostd::panic::panic_in_flight_enter();

    // Check before disabling interrupts so state restores correctly. Recovery
    // is task-scoped only: unwind only for a first-level panic at a recovery
    // boundary outside interrupt/exception context. A nested panic
    // (`prior_in_flight != 0`, e.g. a Drop panicking mid-unwind) falls through
    // to the fatal path, which carries its own `panic_depth` reentrancy guard.
    if prior_in_flight == 0
        && panic_recovery::recovery_is_active()
        && !slopos_ostd::panic::in_interrupt_context()
    {
        // Only production oopses consume the recovered-panic budget;
        // test-harness catches are expected control flow.
        let production = panic_recovery::production_recovery_enabled();
        let (oops_count, limit_reached) = if production {
            panic_recovery::oops_record()
        } else {
            (0, false)
        };

        // Disable interrupts only temporarily for the message
        let interrupts_were_enabled = cpu::are_interrupts_enabled();
        cpu::disable_interrupts();

        if limit_reached {
            // The limit-crossing panic itself becomes fatal. Interrupts stay
            // disabled: this CPU is committed to the fatal path below.
            let mut buf = MessageBuffer::new();
            let _ = write!(
                buf,
                "\n[PANIC] oops limit reached ({}/{}); escalating to fatal",
                oops_count,
                panic_recovery::oops_limit()
            );
            panic_serial_write(buf.as_str());
        } else {
            panic_serial_write("\n[PANIC — task-scoped recovery]");

            if let Some(location) = info.location() {
                let mut buf = MessageBuffer::new();
                let _ = write!(
                    buf,
                    "  at {}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                );
                panic_serial_write(buf.as_str());
            }

            // Also print the panic message for diagnostics
            {
                let mut msg_buf = MessageBuffer::new();
                if let Some(msg) = info.message().as_str() {
                    let _ = write!(msg_buf, "  message: {}", msg);
                } else {
                    let _ = write!(msg_buf, "  message: {}", info.message());
                }
                panic_serial_write(msg_buf.as_str());
            }

            if production {
                let mut buf = MessageBuffer::new();
                let _ = write!(buf, "  oops count: {}", oops_count);
                panic_serial_write(buf.as_str());
            }

            // Walk from the live rbp: the stashed rbp statics belong to the
            // fatal/exception path and may be stale here. The walk starts in
            // this handler, so the panic origin sits below a few
            // panic-machinery frames.
            panic_dump_backtrace_from(cpu::read_rbp());

            // The unwind is caught at the recovery boundary (syscall/kthread
            // edge, or `catch_panic!` in the test harness); do not exit here.

            // Unwinding restores Rust frames, not the interrupt flag.
            if interrupts_were_enabled {
                cpu::enable_interrupts();
            }

            match slopos_ostd::unwind::begin_panic(info) {
                Ok(never) => match never {},
                Err(code) => {
                    let mut buf = MessageBuffer::new();
                    let _ = write!(buf, "  unwind initiation failed: {}", code.0);
                    panic_serial_write(buf.as_str());
                }
            }
        }
    }

    // --- Fatal (uncaught) path: the Reliable Abort Core. ---
    cpu::disable_interrupts();

    // (1) Per-CPU recursion guard. A non-zero prior depth means the fatal path
    // itself faulted (e.g. the emergency stack overflowed). Do not recurse
    // through the (now suspect) reporter — degrade to the format-free abort,
    // which the #PF guard-fault path lands on a fresh IST data stack.
    if slopos_ostd::panic::panic_depth_enter() >= 1 {
        panic_abort_raw("recursive fatal fault — emergency reporter re-entered");
    }

    // (2) Single-owner election: first CAS wins. A peer that loses quietly
    // self-stops so it neither contends on the console nor holds a lock the
    // owner needs; the owner's NMI broadcast (below) will halt it for good.
    let cpu_id = slopos_arch::get_current_cpu() as u32;
    if !slopos_ostd::panic::claim_panic_owner(cpu_id) {
        loop {
            cpu::disable_interrupts();
            cpu::halt_loop();
        }
    }

    // (3) Stash what the emergency reporter needs. The switch to the emergency
    // stacks discards RSP-relative locals, so everything travels via statics;
    // the PanicInfo itself stays live in the (still-mapped) panicking frame.
    // The backtrace is walked from the stashed frame pointer inside
    // `emergency_report`, on the guard-paged emergency stack — the kernel is
    // built `-C force-frame-pointers=yes`, so every frame carries an rbp.
    PANIC_INFO_PTR.store(info as *const PanicInfo as usize, Ordering::SeqCst);
    PANIC_ORIG_RSP.store(cpu::read_rsp(), Ordering::SeqCst);
    PANIC_ORIG_RBP.store(cpu::read_rbp(), Ordering::SeqCst);

    // (4) Stop the world: NMI all peers (the only delivery that pierces a
    // wedged IF=0 spin), then wait — bounded — for them to acknowledge before
    // printing. This is also what dissolves the TLB-shootdown ack wedge.
    slopos_arch::pcr::send_nmi_broadcast();
    wait_for_peer_stop();

    // (5) Run the report on this CPU's emergency SAFE + DATA stacks so panic
    // `core::fmt` has guaranteed headroom and cannot recurse via a guard #PF.
    slopos_ostd::panic::run_on_emergency_stacks(emergency_report)
}

/// Bounded wait for peer CPUs to acknowledge the panic-stop NMI. Proceeds on
/// timeout so a stuck CPU can never block the report.
fn wait_for_peer_stop() {
    let expected = (slopos_arch::pcr::get_pcr_count() as u32).saturating_sub(1);
    if expected == 0 {
        return;
    }
    let mut spins: u64 = 0;
    while slopos_ostd::panic::stopped_cpu_count() < expected && spins < PEER_STOP_SPIN_BUDGET {
        spins = spins.wrapping_add(1);
        cpu::pause();
    }
}

/// The fatal-fault report. Runs on this CPU's emergency SAFE + DATA stacks (via
/// `run_on_emergency_stacks`), so it is the sole console writer (peers stopped)
/// with guaranteed stack headroom. `extern "sysv64"` + `-> !` to match the
/// trampoline's bare-fn entry; all state arrives through statics.
extern "sysv64" fn emergency_report() -> ! {
    let (extra_rip, extra_rsp) = take_panic_cpu_state();
    let display_rsp = extra_rsp.unwrap_or_else(|| PANIC_ORIG_RSP.load(Ordering::SeqCst));
    let cr0 = cpu::read_cr0();
    let cr2 = cpu::read_cr2();
    let cr3 = cpu::read_cr3();
    let cr4 = cpu::read_cr4();

    panic_serial_write("\n\n=== KERNEL PANIC ===");

    let mut message_buf = MessageBuffer::new();
    let info_ptr = PANIC_INFO_PTR.load(Ordering::SeqCst) as *const PanicInfo;
    slopos_ostd::panic::format_panic_location_message(info_ptr, &mut message_buf);
    let message_str = message_buf.as_str();
    panic_serial_write(message_str);

    // Recovered panics may have left non-RAII kernel state skewed; a
    // non-zero count marks this report as post-degradation.
    let oopses = panic_recovery::oops_count();
    if oopses > 0 {
        let mut taint_buf = MessageBuffer::new();
        let _ = write!(taint_buf, "tainted: oops={}", oopses);
        panic_serial_write(taint_buf.as_str());
    }

    panic_serial_write("Register snapshot:");
    if let Some(rip) = extra_rip {
        let mut hex_buf = HexBuffer::new();
        panic_serial_write(hex_buf.format_labeled("RIP", rip));
    }
    {
        let mut hex_buf = HexBuffer::new();
        panic_serial_write(hex_buf.format_labeled("RSP", display_rsp));
    }
    {
        let mut hex_buf = HexBuffer::new();
        panic_serial_write(hex_buf.format_labeled("CR0", cr0));
    }
    {
        // CR2 = faulting linear address on a #PF; invaluable for diagnosing
        // bad-pointer derefs on bare metal.
        let mut hex_buf = HexBuffer::new();
        panic_serial_write(hex_buf.format_labeled("CR2", cr2));
    }
    {
        let mut hex_buf = HexBuffer::new();
        panic_serial_write(hex_buf.format_labeled("CR3", cr3));
    }
    {
        let mut hex_buf = HexBuffer::new();
        panic_serial_write(hex_buf.format_labeled("CR4", cr4));
    }

    panic_dump_backtrace();

    panic_serial_write("===================");
    panic_serial_write("Kernel panic: unrecoverable error");

    #[cfg(feature = "tests")]
    {
        panic_serial_write("TEST MODE: Exiting QEMU with failure code");
        slopos_testing::tests_request_shutdown(1);
    }

    let mut bt = [0u64; 8];
    let bt_n = panic_capture_backtrace(&mut bt);

    if panic_screen::display_panic_screen(
        Some(message_str),
        extra_rip,
        Some(display_rsp),
        cr0,
        cr2,
        cr3,
        cr4,
        &bt[..bt_n],
        false,
    ) {
        panic_serial_write("Press ENTER to shutdown...");
        poll_wait_enter();
    } else {
        panic_serial_write("System halted.");
    }

    if is_memory_system_initialized() != 0 {
        execute_kernel();
    } else {
        panic_serial_write("Memory system unavailable; skipping paint ritual");
    }

    slopos_ostd::sync::panic_recovery::poison_all_held_locks();
}

struct MessageBuffer {
    buf: [u8; 256],
    len: usize,
}

impl MessageBuffer {
    const fn new() -> Self {
        Self {
            buf: [0u8; 256],
            len: 0,
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl Write for MessageBuffer {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let available = self.buf.len() - self.len;
        let to_copy = bytes.len().min(available);
        self.buf[self.len..self.len + to_copy].copy_from_slice(&bytes[..to_copy]);
        self.len += to_copy;
        Ok(())
    }
}

struct HexBuffer {
    buf: [u8; 32],
}

impl HexBuffer {
    const fn new() -> Self {
        Self { buf: [0u8; 32] }
    }

    fn format_labeled(&mut self, label: &str, value: u64) -> &str {
        const HEX_CHARS: &[u8] = b"0123456789ABCDEF";

        let mut pos = 0;

        for &b in label.as_bytes() {
            if pos < self.buf.len() {
                self.buf[pos] = b;
                pos += 1;
            }
        }

        if pos + 4 <= self.buf.len() {
            self.buf[pos] = b':';
            self.buf[pos + 1] = b' ';
            self.buf[pos + 2] = b'0';
            self.buf[pos + 3] = b'x';
            pos += 4;
        }

        for i in 0..16 {
            if pos < self.buf.len() {
                let nibble = ((value >> (60 - i * 4)) & 0xF) as usize;
                self.buf[pos] = HEX_CHARS[nibble];
                pos += 1;
            }
        }

        core::str::from_utf8(&self.buf[..pos]).unwrap_or("")
    }
}
