//! The diagnostic console: a key, pressed on the physical console, that makes
//! the kernel say what it is doing.
//!
//! A *trigger* turns a keypress into one call to [`request`], which sets a bit
//! and raises the bottom half. A *drain* at the bottom-half point runs every
//! queued command. Commands live in a linker registry, so the crate that owns
//! a subsystem's data also owns the command that prints it, and OSTD never
//! names one.
//!
//! Trigger and drain are separate because triggers run where nothing may
//! allocate, log, or take a lock — the keyboard IRQ handler, the serial drain
//! under the per-TTY lock — while what a command wants to do (walk the task
//! registry, take the allocator's lock, emit hundreds of UART lines) is legal
//! only in ordinary task context.
//!
//! The pending set is global rather than per-CPU because [`crate::sync::bh::raise`]
//! marks only the *calling* CPU, and the CPU that took the keyboard IRQ is a
//! candidate for the wedged one; every CPU's timer tick calls
//! [`poll_from_timer`] while anything is queued.
//!
//! Every command runs at the bottom-half point, with interrupts and preemption
//! enabled, and none may assume an NMI-context tier: a *returning* NMI handler
//! must be fault-free, because the `iretq` of any fault it takes would unblock
//! NMI while it is still running, and the frame-pointer walk a backtrace needs
//! is fault-*recoverable* rather than fault-free. The all-CPU probe therefore
//! asks each CPU to describe itself from its own NMI handler rather than
//! walking a peer.

use core::fmt;

use crate::ffi::registry::{RegistryEntry, RegistryId, registry_slice};

pub mod config;
pub mod probe;

pub use config::{KConfig, current as policy, enabled, install};

/// The command reads state and prints it: no kernel writes, no signals, and it
/// cannot fail the machine. Permitted by the default policy.
pub const KCMD_INFORMATIONAL: u8 = 1 << 0;

/// The command takes the machine down. Registered unconditionally so the help
/// command can list it, and refused unless the policy mask names this bit —
/// which the default does not.
pub const KCMD_DESTRUCTIVE: u8 = 1 << 1;

/// One diagnostic command.
///
/// `#[repr(C)]` and 48 bytes: [`registry_slice`] divides the concatenated
/// section by this stride, so the size is part of the contract and
/// `scripts/check_registry_sections.sh` holds the span to a whole number of
/// them.
#[repr(C)]
pub struct KCommand {
    pub run: for<'a> fn(&mut KConsole<'a>),
    pub name: &'static str,
    pub help: &'static str,
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

/// The only way to emit from a command.
///
/// Invariant-branded and `!Send + !Sync`: the handle describes one drain on one
/// CPU and cannot be stashed for later or handed to another. The line budget
/// lives in the handle rather than in a rule each command is asked to respect,
/// so a command walking an unbounded structure cannot monopolise the console.
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
    /// `#[inline(never)]` on purpose: inlining a `format_args!` emitter into
    /// each caller grows every one of their frames against
    /// `check_stack_sizes.sh`'s 2 KiB cap.
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

    /// Lines still allowed, so a command walking an unbounded structure can
    /// stop at a coherent boundary rather than mid-record.
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

/// Queued command keys, one bit per ASCII code. A bitmap rather than a ring
/// because a command is idempotent — asking for the task table twice before
/// either runs should print it once.
static PENDING: [core::sync::atomic::AtomicU64; 2] = [
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
];

/// Queue `key` for the next drain. No lock, no allocation, no registry read:
/// legal from a hard IRQ handler and from under a cli-spinlock, which is what
/// every trigger path needs.
pub fn request(key: u8) {
    if key >= 128 || !enabled() {
        return;
    }
    let word = (key >> 6) as usize;
    PENDING[word].fetch_or(1u64 << (key & 63), core::sync::atomic::Ordering::Release);
    crate::sync::bh::raise();
}

/// Raise this CPU's bottom half if anything is queued. Called from every CPU's
/// timer tick, so a request raised on one CPU can be answered by another.
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
/// pending set between them, so each queued command runs exactly once.
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
/// Runs *every* matching entry rather than the first: a duplicate key is a
/// build-time mistake `kcon_keys_are_unique` fails on, and printing both beats
/// silently picking whichever landed earlier in link order.
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

/// Body of the help command, which a kernel crate registers.
///
/// OSTD supplies the text but not the registry entry: OSTD is linked into
/// userland binaries too, whose linker script brackets no kernel registry, so
/// an entry here would leave every userland binary referencing
/// `__start_kconsole_registry` and failing to link.
pub fn help_body(kc: &mut KConsole<'_>) {
    let cfg = config::current();
    kline!(kc, "kconsole: commands (policy mask 0x{:x})", cfg.mask);
    for cmd in commands() {
        let permitted = if cmd.flags & cfg.mask != 0 {
            ' '
        } else {
            // So an operator sees, before pressing, that policy will refuse it.
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
