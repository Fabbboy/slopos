//! Rust-typed eXpress Data Path — safe-Rust packet filters on the RX hot path.
//!
//! A filter is an ordinary safe-Rust type implementing [`XdpFilter`], run inline
//! in [`crate::ingress::net_rx`] on the full L2 frame ahead of the protocol
//! stack. There is no bytecode VM, verifier or JIT: the verifier is `rustc` plus
//! the `#![forbid(unsafe_code)]` discipline every kernel crate already carries.
//! [`XDP`] holds the chain in an [`RcuCell`], and membership is an explicit
//! sequence of [`register`](XdpHookChain::register) calls, not linker layout.

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
/// `execute` runs under a [`NET_EPOCH`] read guard, so it MUST be lock-free and
/// non-blocking: no tracked lock, no sleeping, no yielding. Stateful filters use
/// atomics or an `RcuCell`.
pub trait XdpFilter: Send + Sync + 'static {
    fn execute(&self, pkt: &mut PacketView<'_>) -> XdpAction;
}

/// The kernel-wide XDP filter chain.
pub struct XdpHookChain {
    chain: RcuCell<KVec<&'static dyn XdpFilter>>,
}

/// The global XDP filter chain. Empty until filters are installed; an empty
/// chain passes every packet through unchanged.
pub static XDP: XdpHookChain = XdpHookChain::new();

impl XdpHookChain {
    pub const fn new() -> Self {
        Self {
            chain: RcuCell::empty(),
        }
    }

    /// Run the installed filters against `view`; the first
    /// non-[`Pass`](XdpAction::Pass) verdict wins. Both the epoch and RCU read
    /// guards are released on return, so the caller may take locks acting on it.
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

    /// Append a filter (control plane only); registration order is execution
    /// order.
    pub fn register(&self, filter: &'static dyn XdpFilter) -> Result<(), AllocError> {
        let mut next = match self.chain.load() {
            Some(current) => (*current).clone(),
            None => KVec::new(),
        };
        next.push(filter)?;
        self.chain.replace(KBox::try_new(next)?);
        Ok(())
    }

    /// Replace the whole chain (control plane only).
    pub fn install(&self, chain: KVec<&'static dyn XdpFilter>) -> Result<(), AllocError> {
        self.chain.replace(KBox::try_new(chain)?);
        Ok(())
    }

    /// Remove all filters (control plane only).
    pub fn clear(&self) {
        let _ = self.install(KVec::new());
    }

    pub fn len(&self) -> usize {
        self.chain.load().map_or(0, |c| c.len())
    }

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
/// Runs before NAPI delivers any packet, so there is no concurrent reader and it
/// may publish via [`RcuCell::store_pre_scheduler`].
pub fn init() {
    if let Ok(empty) = KBox::try_new(KVec::new()) {
        XDP.chain.store_pre_scheduler(empty);
    }
}
