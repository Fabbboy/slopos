//! The diagnostic console: a key, pressed on the physical console, that makes
//! the kernel say what it is doing.
//!
//! # Shape
//!
//! A *trigger* turns a keypress into one call to [`request`], which sets a bit
//! and raises the bottom half. A *drain* at the bottom-half point runs every
//! queued command. Commands live in a linker registry, so the crate that owns
//! a subsystem's data also owns the command that prints it, and OSTD never
//! names one.
//!
//! # Why the trigger and the drain are separate
//!
//! Triggers run where nothing may allocate, log, or take a lock: the keyboard
//! IRQ handler, and the serial drain under the per-TTY lock. [`request`] is one
//! `fetch_or` and one `gs`-relative byte store, which is exactly what
//! [`crate::sync::bh::raise`] permits from those contexts. Everything a command
//! actually wants to do — walk the task registry, take the allocator's lock,
//! emit hundreds of lines through a 115200-baud UART — is legal only in
//! ordinary task context, and that is where the drain runs.
//!
//! # Why the queue is global and the timer pokes
//!
//! `bh::raise` marks the *calling* CPU. A request raised from the keyboard IRQ
//! would therefore drain only on the CPU that took that IRQ — which, when the
//! console is being used for its actual purpose, is a candidate for the wedged
//! one. So the pending set is a global bitmap, and every CPU's timer tick calls
//! [`poll_from_timer`] to raise its own bottom half while anything is queued.
//! Whichever CPU reaches the point first runs the command.
//!
//! # One execution tier
//!
//! Every command runs at the bottom-half point, with interrupts and preemption
//! enabled. There is no NMI-context tier and no handler may assume one: a
//! *returning* NMI handler must be fault-free, because the `iretq` of any fault
//! it takes would unblock NMI while it is still running, and the frame-pointer
//! walk a backtrace needs is fault-*recoverable* rather than fault-free. The
//! all-CPU probe gets its registers by asking each CPU to describe itself from
//! its own NMI handler, never by walking a peer from here.

use core::fmt;

use crate::ffi::registry::{RegistryEntry, RegistryId, registry_slice};

pub mod config;

pub use config::{KConfig, current as policy, enabled, install};

/// The command reads state and prints it.
///
/// It writes no kernel state, sends no signal, and cannot fail the machine.
/// Permitted by the default policy.
pub const KCMD_INFORMATIONAL: u8 = 1 << 0;

/// The command takes the machine down.
///
/// Registered unconditionally so the help command can list it, and refused unless the
/// policy mask names this bit — which the default does not.
pub const KCMD_DESTRUCTIVE: u8 = 1 << 1;

/// One diagnostic command.
///
/// `#[repr(C)]` and 48 bytes: the linker concatenates these into an array that
/// [`registry_slice`] divides by this stride, so the size is part of the
/// contract and `scripts/check_registry_sections.sh` holds the section span to
/// a whole number of them.
#[repr(C)]
pub struct KCommand {
    /// Runs at the bottom-half point, interrupts and preemption enabled.
    pub run: for<'a> fn(&mut KConsole<'a>),
    /// Short name, shown by the help command.
    pub name: &'static str,
    /// One line of help, shown by the help command.
    pub help: &'static str,
    /// The ASCII key that selects this command.
    pub key: u8,
    /// Exactly one of [`KCMD_INFORMATIONAL`] / [`KCMD_DESTRUCTIVE`].
    pub flags: u8,
}

const _: () = assert!(core::mem::size_of::<KCommand>() == 48);
const _: () = assert!(core::mem::align_of::<KCommand>() == 8);

impl RegistryEntry for KCommand {
    const REGISTRIES: &'static [RegistryId] = &[RegistryId::KConsole];
}

/// Every registered command, in link order.
pub fn commands() -> &'static [KCommand] {
    registry_slice::<KCommand>(RegistryId::KConsole)
}

/// Register a diagnostic command.
///
/// `name` is an ident rather than a string because the registry's writer macro
/// places a named static and `paste!` cannot build an identifier out of the
/// byte literal the key is written as.
///
/// ```ignore
/// slopos_ostd::kcommand! {
///     name  = tasks,
///     key   = b't',
///     help  = "task table",
///     flags = slopos_ostd::kconsole::KCMD_INFORMATIONAL,
///     run   = dump_tasks,
/// }
/// ```
#[macro_export]
macro_rules! kcommand {
    (
        name = $id:ident,
        key = $key:expr,
        help = $help:expr,
        flags = $flags:expr,
        run = $run:path $(,)?
    ) => {
        $crate::__paste::paste! {
            $crate::registry_entry! {
                kconsole,
                #[allow(non_upper_case_globals)]
                pub static [<KCON_CMD_ $id>]: $crate::kconsole::KCommand =
                    $crate::kconsole::KCommand {
                        run: $run,
                        name: ::core::stringify!($id),
                        help: $help,
                        key: $key,
                        flags: $flags,
                    };
            }
        }
    };
}

// ---------------------------------------------------------------------------
// The emit handle
// ---------------------------------------------------------------------------

/// The only way to emit from a command.
///
/// Invariant-branded and `!Send + !Sync`: the handle describes one drain on one
/// CPU and cannot be stashed for later or handed to another. The line budget
/// lives in the handle rather than in a rule each command is asked to respect,
/// so a command that walks a structure the kernel lets grow without bound
/// cannot monopolise the console by accident.
pub struct KConsole<'brand> {
    budget: u32,
    emitted: u32,
    truncated: bool,
    _brand: core::marker::PhantomData<fn(&'brand ()) -> &'brand ()>,
    _not_send: core::marker::PhantomData<*const ()>,
}

impl KConsole<'_> {
    /// Private: [`drain`] is the only minter, which is what makes possessing
    /// one proof of running in the context commands are permitted in.
    fn new(budget: u16) -> Self {
        Self {
            budget: budget as u32,
            emitted: 0,
            truncated: false,
            _brand: core::marker::PhantomData,
            _not_send: core::marker::PhantomData,
        }
    }

    /// Emit one line. Stops at the budget and says so exactly once.
    ///
    /// `#[inline(never)]` on purpose: `check_stack_sizes.sh` measures the debug
    /// ELF, where inlining a `format_args!` emitter into each of its callers
    /// grows every one of their frames against a 2 KiB cap.
    #[inline(never)]
    pub fn line(&mut self, args: fmt::Arguments<'_>) {
        if self.emitted >= self.budget {
            if !self.truncated {
                self.truncated = true;
                crate::klog::log_forced(format_args!(
                    "kconsole: output truncated at {} lines (kconsole.max_lines=)",
                    self.budget
                ));
            }
            return;
        }
        self.emitted += 1;
        crate::klog::log_forced(args);
    }

    /// Emit `prefix` followed by a symbolized code address.
    ///
    /// A bare address is only useful to someone holding the matching ELF; the
    /// symbol is what makes a dump readable in a bug report.
    #[inline(never)]
    pub fn sym(&mut self, prefix: fmt::Arguments<'_>, addr: u64) {
        match crate::ksym::lookup(addr) {
            Some(s) => self.line(format_args!(
                "{}0x{:016x} <{}+0x{:x}>",
                prefix, addr, s.symbol, s.offset
            )),
            None => self.line(format_args!("{}0x{:016x}", prefix, addr)),
        }
    }

    /// Lines still allowed. A command walking an unbounded structure checks
    /// this so it can stop at a coherent boundary rather than mid-record.
    #[inline]
    pub fn budget_left(&self) -> u32 {
        self.budget.saturating_sub(self.emitted)
    }

    /// Whether the budget has been hit and output is being dropped.
    #[inline]
    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Emit one formatted line from a command.
#[macro_export]
macro_rules! kline {
    ($kc:expr, $($arg:tt)*) => {
        $kc.line(::core::format_args!($($arg)*))
    };
}

/// Emit a symbolized code address from a command.
#[macro_export]
macro_rules! ksymline {
    ($kc:expr, $addr:expr, $($arg:tt)*) => {
        $kc.sym(::core::format_args!($($arg)*), $addr)
    };
}

// ---------------------------------------------------------------------------
// The pending set
// ---------------------------------------------------------------------------

/// Queued command keys, one bit per ASCII code.
///
/// Global rather than per-CPU: see the module header. A bitmap rather than a
/// ring because a command is idempotent — asking for the task table twice
/// before either runs should print it once.
static PENDING: [core::sync::atomic::AtomicU64; 2] = [
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
];

/// Queue `key` for the next drain.
///
/// One `fetch_or` and one `gs`-relative byte store: no lock, no allocation, no
/// registry read. Legal from a hard IRQ handler and from under a cli-spinlock,
/// which is what every trigger path needs.
pub fn request(key: u8) {
    if key >= 128 || !enabled() {
        return;
    }
    let word = (key >> 6) as usize;
    PENDING[word].fetch_or(1u64 << (key & 63), core::sync::atomic::Ordering::Release);
    crate::sync::bh::raise();
}

/// Raise this CPU's bottom half if anything is queued.
///
/// Called from the timer tick on every CPU. Two relaxed loads in the common
/// case, and it never emits or dispatches — it exists purely so a request
/// raised on one CPU can be answered by another.
#[inline]
pub fn poll_from_timer() {
    use core::sync::atomic::Ordering;
    if PENDING[0].load(Ordering::Relaxed) | PENDING[1].load(Ordering::Relaxed) != 0 {
        crate::sync::bh::raise();
    }
}

/// Run every queued command. Returns whether it did anything.
///
/// Concurrent drains need no single-flight flag: the `swap` partitions the
/// pending set between them, so each queued command runs exactly once no
/// matter how many CPUs reach this at once.
pub fn drain() -> bool {
    use core::sync::atomic::Ordering;
    let words = [
        PENDING[0].swap(0, Ordering::AcqRel),
        PENDING[1].swap(0, Ordering::AcqRel),
    ];
    if words[0] | words[1] == 0 {
        return false;
    }
    let cfg = config::current();
    for (index, mut bits) in words.into_iter().enumerate() {
        while bits != 0 {
            let bit = bits.trailing_zeros();
            bits &= bits - 1;
            run_key(((index as u8) << 6) | bit as u8, &cfg);
        }
    }
    true
}

/// Dispatch one key.
///
/// Runs *every* entry whose key matches rather than the first. A duplicate key
/// is a build-time mistake that `kcon_keys_are_unique` fails on; producing both
/// outputs is a far better failure than silently picking whichever landed
/// earlier in link order.
fn run_key(key: u8, cfg: &KConfig) {
    let mut matched = false;
    for cmd in commands() {
        if cmd.key != key {
            continue;
        }
        matched = true;
        if cmd.flags & cfg.mask == 0 {
            crate::klog::log_forced(format_args!(
                "kconsole: '{}' ({}) refused by policy (kconsole=0x{:x})",
                key as char, cmd.name, cfg.mask
            ));
            continue;
        }
        let mut console = KConsole::new(cfg.max_lines);
        (cmd.run)(&mut console);
    }
    if !matched {
        crate::klog::log_forced(format_args!(
            "kconsole: no command for '{}' — press the trigger then 'h' for the list",
            key as char
        ));
    }
}

// ---------------------------------------------------------------------------
// Help
// ---------------------------------------------------------------------------

/// Body of the help command, which a kernel crate registers.
///
/// OSTD supplies the text but not the registry entry. A `#[used]` static in
/// `.kconsole_registry` keeps its `run` pointer alive through `--gc-sections`,
/// and OSTD is linked into userland binaries too, whose linker script brackets
/// no kernel registry — so a command registered here would leave every
/// userland binary referencing `__start_kconsole_registry` and failing to
/// link. Registration belongs to crates only the kernel links, which is what
/// this module claims about commands in general.
pub fn help_body(kc: &mut KConsole<'_>) {
    let cfg = config::current();
    kline!(kc, "kconsole: commands (policy mask 0x{:x})", cfg.mask);
    for cmd in commands() {
        let permitted = if cmd.flags & cfg.mask != 0 {
            ' '
        } else {
            // An operator who presses a listed key and gets nothing should be
            // able to see beforehand that policy is why.
            'x'
        };
        kline!(
            kc,
            "  {} {}  {:<10} {}",
            permitted,
            cmd.key as char,
            cmd.name,
            cmd.help
        );
    }
}
