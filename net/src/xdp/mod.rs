//! Rust-typed eXpress Data Path — safe-Rust packet filters on the RX hot path.
//!
//! A filter is an ordinary safe-Rust type implementing [`XdpFilter`]. Filters
//! run inline in the ingress pipeline ([`crate::ingress::net_rx`]) on the full
//! L2 frame, before the protocol stack processes it. There is no bytecode VM,
//! no verifier, and no JIT: the verifier is `rustc` plus the
//! `#![forbid(unsafe_code)]` discipline every kernel crate (this one included)
//! already carries.
//!
//! # Hook chain
//!
//! [`XDP`] holds the installed chain in an [`RcuCell`], so the hot-path read in
//! [`XdpHookChain::execute`] is lock-free and reclaimed under RCU grace periods
//! (the same epoch infrastructure the TCP demux uses). Membership and order are
//! an explicit sequence of [`register`](XdpHookChain::register) calls — not an
//! emergent property of linker layout — keeping the chain auditable.
//!
//! # Filter contract
//!
//! [`XdpFilter::execute`] runs under a [`NET_EPOCH`] read guard, so it MUST be
//! non-blocking and lock-free: no `SpinLock`, no sleeping, no yielding. The
//! kernel's lock graph enforces this — acquiring a tracked lock inside the
//! epoch is a hard error. Stateful filters use atomics or an `RcuCell`, never a
//! tracked lock.

use slopos_ostd::mm::AllocError;
use slopos_ostd::sync::RcuCell;
use slopos_ostd::{KBox, KVec};

use crate::tcp::table::NET_EPOCH;
use crate::types::DevIndex;

pub use packet_view::{EthernetView, Ipv4View, PacketView, UdpView};
pub use slopos_ostd_derive::xdp_filter;

mod packet_view;

/// The verdict a filter returns for a packet.
///
/// The first non-[`Pass`](XdpAction::Pass) verdict in the chain wins; the
/// remaining filters are not consulted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XdpAction {
    /// Let the packet continue into the protocol stack.
    Pass,
    /// Drop the packet; its buffer is recycled.
    Drop,
    /// Re-transmit the (possibly mutated) frame out the ingress device.
    Tx,
    /// Transmit the frame out a different device by index.
    Redirect(DevIndex),
}

/// A safe-Rust packet filter executed on the RX hot path.
///
/// `execute` runs under a [`NET_EPOCH`] read guard and MUST be lock-free and
/// non-blocking (see the module docs).
pub trait XdpFilter: Send + Sync + 'static {
    /// Inspect (and optionally mutate) the frame and return a verdict.
    fn execute(&self, pkt: &mut PacketView<'_>) -> XdpAction;
}

/// The kernel-wide XDP filter chain.
///
/// Stores `&'static dyn XdpFilter` references (filters are `'static` values, so
/// the chain needs no per-filter allocation and the backing `KVec` is cheaply
/// clonable for copy-on-write installs).
pub struct XdpHookChain {
    chain: RcuCell<KVec<&'static dyn XdpFilter>>,
}

/// The global XDP filter chain. Empty until filters are installed; an empty
/// chain passes every packet through unchanged.
pub static XDP: XdpHookChain = XdpHookChain::new();

impl XdpHookChain {
    /// Create an empty chain (no filters installed).
    pub const fn new() -> Self {
        Self {
            chain: RcuCell::empty(),
        }
    }

    /// Run the installed filters against `view` and return the winning verdict.
    ///
    /// Lock-free: enters [`NET_EPOCH`], loads the chain via [`RcuCell`], and
    /// iterates. The first non-[`Pass`](XdpAction::Pass) verdict wins. Both the
    /// epoch guard and the RCU read guard are released when this returns, so the
    /// caller may take locks (TX, stack dispatch) on the returned verdict.
    pub fn execute(&self, view: &mut PacketView<'_>) -> XdpAction {
        let _epoch = NET_EPOCH.enter();
        let Some(chain) = self.chain.load() else {
            return XdpAction::Pass;
        };
        for filter in chain.iter() {
            match filter.execute(view) {
                XdpAction::Pass => continue,
                verdict => return verdict,
            }
        }
        XdpAction::Pass
    }

    /// Append a filter to the end of the chain (control plane only).
    ///
    /// Copy-on-write: clones the current chain, appends, and publishes the new
    /// chain via [`RcuCell::replace`] (the displaced chain is reclaimed after a
    /// grace period). Registration order is execution order.
    pub fn register(&self, filter: &'static dyn XdpFilter) -> Result<(), AllocError> {
        let mut next = match self.chain.load() {
            Some(current) => (*current).clone(),
            None => KVec::new(),
        };
        next.push(filter)?;
        self.chain.replace(KBox::try_new(next)?);
        Ok(())
    }

    /// Replace the whole chain (control plane only). Used by tests to install a
    /// specific ordered chain.
    pub fn install(&self, chain: KVec<&'static dyn XdpFilter>) -> Result<(), AllocError> {
        self.chain.replace(KBox::try_new(chain)?);
        Ok(())
    }

    /// Remove all filters (control plane only).
    pub fn clear(&self) {
        let _ = self.install(KVec::new());
    }

    /// Number of installed filters (diagnostics / tests).
    pub fn len(&self) -> usize {
        self.chain.load().map_or(0, |c| c.len())
    }

    /// `true` if no filters are installed.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for XdpHookChain {
    fn default() -> Self {
        Self::new()
    }
}

/// Install the built-in filter chain at boot.
///
/// Runs before NAPI delivers any packet (no concurrent reader yet), so it
/// publishes via [`RcuCell::store_pre_scheduler`]. This is the single,
/// auditable place to register built-in filters; none are built in today, so
/// the default chain is empty and behaviour is unchanged.
pub fn init() {
    if let Ok(empty) = KBox::try_new(KVec::new()) {
        XDP.chain.store_pre_scheduler(empty);
    }
}
