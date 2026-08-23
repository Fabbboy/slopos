//! Early-boot serial driver, funnelling through `slopos_ostd::early_console`
//! and the safe `slopos_ostd::io::UartRegs` register window.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use slopos_arch::cpu;
use slopos_ostd::io::UartRegs;
use slopos_ostd::io::port_consts::{
    COM1, UART_FCR_14_BYTE_THRESHOLD as FCR_14_BYTE_THRESHOLD, UART_FCR_CLEAR_RX as FCR_CLEAR_RX,
    UART_FCR_CLEAR_TX as FCR_CLEAR_TX, UART_FCR_ENABLE_FIFO as FCR_ENABLE_FIFO,
    UART_IIR_FIFO_ENABLED as IIR_FIFO_ENABLED, UART_IIR_FIFO_MASK as IIR_FIFO_MASK,
    UART_LCR_DLAB as LCR_DLAB, UART_LSR_BREAK as LSR_BREAK, UART_LSR_DATA_READY as LSR_DATA_READY,
    UART_MCR_AUX2 as MCR_AUX2, UART_MCR_DTR as MCR_DTR, UART_MCR_RTS as MCR_RTS,
};
use slopos_ostd::io::raw_port::Port;
use slopos_ostd::klog_info;
use slopos_ostd::lock_class;
use slopos_ostd::ring_buffer::RingBuffer;
use slopos_ostd::sync::{LOCK_LEVEL_UNORDERED, SpinLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UartType {
    Uart8250,
    Uart16450,
    Uart16550,
    Uart16550A,
    Uart16650,
    Uart16750,
    Unknown,
}

#[derive(Debug, Clone, Copy)]
pub struct UartCapabilities {
    /// An unclaimed x86 I/O port reads `0xFF` everywhere, and `0xFF` has
    /// `LSR_DATA_READY` set — so a naive receive loop never finishes.
    pub present: bool,
    pub uart_type: UartType,
    pub has_fifo: bool,
    pub fifo_working: bool,
    pub fifo_size: usize,
}

// Diagnostic leaf locks: the panic handler writes through SERIAL while
// arbitrary kernel locks may still be held, so UNORDERED bypasses the OSTD
// walker's ordering check. Self-deadlock is still caught by SpinLock's ticket
// mechanism.
static SERIAL: SpinLock<SerialPort> = SpinLock::new(
    SerialPort::new(COM1),
    lock_class!("SERIAL", LOCK_LEVEL_UNORDERED),
);
const BUF_SIZE: usize = 256;

static SERIAL_PRESENT: AtomicBool = AtomicBool::new(true);

/// The drain runs under the per-TTY lock with interrupts disabled, so its cost
/// is a window in which this CPU answers no IPI, a TLB shootdown included.
const POLL_RECEIVE_BUDGET: usize = BUF_SIZE;

const SCRATCH_PROBE_A: u8 = 0xAA;
const SCRATCH_PROBE_B: u8 = 0x55;

/// A scratch register that echoes settles it; an 8250/16450 has none, so its
/// status registers decide.
pub(crate) fn uart_is_present(scratch_echoed: bool, lsr: u8, iir: u8) -> bool {
    scratch_echoed || lsr != 0xFF || iir != 0xFF
}

type SerialBuffer = RingBuffer<u8, BUF_SIZE>;

static INPUT_BUFFER: SpinLock<SerialBuffer> = SpinLock::new(
    SerialBuffer::new_with(0),
    lock_class!("INPUT_BUFFER", LOCK_LEVEL_UNORDERED),
);

pub fn init() {
    let mut port = SERIAL.lock();
    port.init();
    let present = port.capabilities().present;
    drop(port);

    SERIAL_PRESENT.store(present, Ordering::Relaxed);
    if !present {
        klog_info!("SERIAL: no UART at COM1; console input disabled");
    }

    slopos_ostd::klog::klog_register_backend(serial_klog_backend);
}

/// PCR-independent **ticket lock** for klog serial output.
///
/// `SpinLock` depends on the PCR via `PreemptGuard`, which is unavailable
/// during AP boot; this lock uses only `cli`/`sti` plus a ticket pair.  Every
/// code path that writes to COM1 outside the early-boot fallback must funnel
/// through `with_klog_lock` so writes do not byte-interleave on the wire.
static KLOG_NEXT_TICKET: AtomicU16 = AtomicU16::new(0);
static KLOG_NOW_SERVING: AtomicU16 = AtomicU16::new(0);

/// A taken ticket, released in a destructor: a panic while the lock is held
/// unwinds past any release written as a tail statement, and a ticket that is
/// never served leaves every later `klog_*!` on every CPU spinning on it forever
/// with interrupts disabled.
struct KlogTicket {
    saved_flags: u64,
}

impl Drop for KlogTicket {
    #[inline]
    fn drop(&mut self) {
        KLOG_NOW_SERVING.fetch_add(1, Ordering::Release);
        cpu::restore_flags(self.saved_flags);
    }
}

/// Run `f` holding the COM1 ticket lock, with interrupts disabled.
#[inline]
fn with_klog_lock<F: FnOnce()>(f: F) {
    let saved_flags = cpu::save_flags_cli();
    let my_ticket = KLOG_NEXT_TICKET.fetch_add(1, Ordering::Relaxed);
    let _ticket = KlogTicket { saved_flags };
    loop {
        let serving = KLOG_NOW_SERVING.load(Ordering::Acquire);
        if serving == my_ticket {
            break;
        }
        // A hand-rolled interrupts-off wait owes the same shootdown service the
        // lock primitives perform for their own waiters: without it, a holder
        // blocked on this CPU's TLB ack and this CPU blocked on that holder's
        // ticket are a closed cycle.
        slopos_ostd::sync::spin_relax();
        let distance = my_ticket.wrapping_sub(serving) as u32;
        for _ in 0..distance.min(64) {
            core::hint::spin_loop();
        }
    }

    f();
}

fn serial_klog_backend(args: fmt::Arguments<'_>) {
    with_klog_lock(|| {
        struct KlogWriter;
        impl fmt::Write for KlogWriter {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                slopos_ostd::early_console::write_bytes(s.as_bytes());
                Ok(())
            }
        }

        let _ = fmt::write(&mut KlogWriter, args);
        let _ = KlogWriter.write_str("\n");
    });
}

/// Write `bytes` to COM1 atomically with respect to klog output.
///
/// The **only** sanctioned path for non-klog code to write to COM1 outside the
/// early-boot fallback: it takes the same ticket lock as `serial_klog_backend`,
/// so direct `serial_write_batch` / `serial_putc` calls bypass that lock and can
/// corrupt klog output — notably the test harness's KTAP wire format.
pub fn serial_locked_write_bytes(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    with_klog_lock(|| {
        slopos_ostd::early_console::write_bytes(bytes);
    });
}

pub fn init_port(base: u16) -> Result<UartCapabilities, ()> {
    if base == COM1.address() {
        let mut port = SERIAL.lock();
        port.init();
        Ok(port.capabilities())
    } else {
        Err(())
    }
}

pub fn get_capabilities() -> UartCapabilities {
    SERIAL.lock().capabilities()
}

pub fn write_str(s: &str) {
    let _ = SERIAL.lock().write_str(s);
}

pub fn write_line(s: &str) {
    let mut guard = SERIAL.lock();
    let _ = guard.write_str(s);
    let _ = guard.write_str("\r\n");
}

pub fn serial_putc_com1(ch: u8) {
    SERIAL.lock().write_byte(ch);
}

pub fn print_args(args: fmt::Arguments<'_>) {
    let _ = SERIAL.lock().write_fmt(args);
}

/// What the reader should do with one received byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SerialAction {
    /// Ordinary input: hand it to the line discipline.
    Deliver(u8),
    /// A break condition. Its framing byte carries no data.
    Consumed,
    /// The key that followed a break: a diagnostic-console command.
    Command(u8),
}

/// Whether a break is waiting for its command key.
static BREAK_ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Classify one `(LSR, byte)` pair against the break-armed state.
///
/// The window is one byte rather than a deadline: the reader is polled from
/// contexts that exist before the clock service does, and reading a clock that
/// may not be registered is a worse failure than a trigger that expires on the
/// next character.
pub(crate) fn serial_console_step(
    lsr: u8,
    byte: u8,
    armed: &mut bool,
    trigger_enabled: bool,
) -> SerialAction {
    if lsr & LSR_BREAK != 0 {
        // A break cannot be forged by any byte pattern, which is what makes it
        // usable as a trigger on a line that also carries data. The UART pairs
        // it with a framing-error byte that is not input.
        *armed = trigger_enabled;
        return SerialAction::Consumed;
    }
    if core::mem::replace(armed, false) {
        return SerialAction::Command(byte);
    }
    SerialAction::Deliver(byte)
}

pub fn serial_poll_receive(base: u16) {
    if !SERIAL_PRESENT.load(Ordering::Relaxed) {
        return;
    }

    let regs = UartRegs::new(Port::<u8>::new(base));
    for _ in 0..POLL_RECEIVE_BUDGET {
        // One read: LSR's error bits are cleared by reading it, so the break
        // flag has to be taken from the same read that reports the byte.
        let lsr = regs.read_lsr();
        if lsr & LSR_DATA_READY == 0 {
            break;
        }
        let byte = regs.read_rbr();

        let trigger_enabled =
            slopos_ostd::kconsole::enabled() && slopos_ostd::kconsole::policy().serial;
        let mut armed = BREAK_ARMED.load(Ordering::Acquire);
        let action = serial_console_step(lsr, byte, &mut armed, trigger_enabled);
        BREAK_ARMED.store(armed, Ordering::Release);

        match action {
            SerialAction::Deliver(b) => {
                let mut buf = INPUT_BUFFER.lock();
                let _ = buf.try_push(b);
            }
            SerialAction::Consumed => {}
            // Requested rather than run: this is the polled reader, which the
            // per-TTY lock is held across on one of its two call paths.
            SerialAction::Command(key) => slopos_ostd::kconsole::request(key),
        }
    }
}

pub fn serial_buffer_pending(port: u16) -> i32 {
    serial_poll_receive(port);
    let buf = INPUT_BUFFER.lock();
    (!buf.is_empty()) as i32
}

pub fn serial_buffer_read(port: u16, out: *mut u8) -> i32 {
    serial_poll_receive(port);
    let mut buf = INPUT_BUFFER.lock();
    match buf.try_pop() {
        Some(b) => {
            slopos_ostd::util::ptr_buf::write_if_non_null(out, b);
            0
        }
        None => -1,
    }
}

/// Lock the serial INPUT_BUFFER without polling the UART; the caller is
/// expected to have called `serial_poll_receive` already.
pub fn input_buffer_lock() -> slopos_ostd::sync::SpinLockGuard<'static, SerialBuffer> {
    INPUT_BUFFER.lock()
}

struct SerialPort {
    regs: UartRegs,
    caps: UartCapabilities,
}

impl SerialPort {
    const fn new(base: Port<u8>) -> Self {
        Self {
            regs: UartRegs::new(base),
            caps: UartCapabilities {
                present: true,
                uart_type: UartType::Unknown,
                has_fifo: false,
                fifo_working: false,
                fifo_size: 0,
            },
        }
    }

    fn probe_present(&mut self) -> bool {
        let mut scratch_echoed = false;
        for probe in [SCRATCH_PROBE_A, SCRATCH_PROBE_B] {
            self.regs.write_scr(probe);
            if self.regs.read_scr() == probe {
                scratch_echoed = true;
            }
        }
        uart_is_present(scratch_echoed, self.regs.read_lsr(), self.regs.read_iir())
    }

    fn detect_uart(&mut self) -> UartCapabilities {
        if !self.probe_present() {
            return UartCapabilities {
                present: false,
                uart_type: UartType::Unknown,
                has_fifo: false,
                fifo_working: false,
                fifo_size: 0,
            };
        }

        self.regs
            .write_fcr(FCR_ENABLE_FIFO | FCR_CLEAR_RX | FCR_CLEAR_TX);

        for _ in 0..10 {
            core::hint::spin_loop();
        }

        let iir_after = self.regs.read_iir();
        let has_fifo = (iir_after & IIR_FIFO_MASK) == IIR_FIFO_ENABLED;

        self.regs.write_fcr(0);

        if !has_fifo {
            return UartCapabilities {
                present: true,
                uart_type: UartType::Uart16450,
                has_fifo: false,
                fifo_working: false,
                fifo_size: 0,
            };
        }

        let test_value = 0xAA;
        self.regs.write_scr(test_value);
        let scratch_read = self.regs.read_scr();
        let fifo_working = scratch_read == test_value;

        let fifo_size = 16;
        let uart_type = if fifo_working {
            UartType::Uart16550A
        } else {
            UartType::Uart16550
        };

        UartCapabilities {
            present: true,
            uart_type,
            has_fifo: true,
            fifo_working,
            fifo_size,
        }
    }

    fn init(&mut self) {
        self.caps = self.detect_uart();
        if !self.caps.present {
            return;
        }

        self.regs.write_ier(0x00);
        self.regs.write_lcr(LCR_DLAB);
        self.regs.write_rbr(0x01);
        self.regs.write_ier(0x00);
        self.regs.write_lcr(0x03);

        if self.caps.has_fifo {
            if self.caps.fifo_working {
                self.regs.write_fcr(
                    FCR_ENABLE_FIFO | FCR_CLEAR_RX | FCR_CLEAR_TX | FCR_14_BYTE_THRESHOLD,
                );
            } else {
                self.regs
                    .write_fcr(FCR_ENABLE_FIFO | FCR_CLEAR_RX | FCR_CLEAR_TX);
            }
        }

        self.regs.write_mcr(MCR_DTR | MCR_RTS | MCR_AUX2);
    }

    fn write_byte(&mut self, byte: u8) {
        // TODO(tech-debt): writes via `early_console`, not `self.regs` — a
        // non-COM1 port would land on COM1.
        let _ = self.regs;
        slopos_ostd::early_console::write_byte(byte);
    }

    pub fn capabilities(&self) -> UartCapabilities {
        self.caps
    }
}

impl Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let _ = self.regs;
        slopos_ostd::early_console::write_bytes(s.as_bytes());
        Ok(())
    }
}
