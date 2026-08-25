//! Test-only sink device: accepts every transmit and yields nothing on RX.
//!
//! `tx` must return `Ok`. `ipv4::send` propagates an error up through
//! `socket_send_tcp_segment` into `socket_connect`, which aborts the PCB, so a
//! rejecting device would make a connection *fail* rather than go nowhere.
//!
//! Every counter is an atomic: the tree's other mock devices keep theirs behind
//! `SpinLock`s, and each such lock is a lockdep class the tests-build class caps
//! count exactly.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use slopos_ostd::KVec;

use crate::iface::IfaceKind;
use crate::netdev::{NetDevice, NetDeviceFeatures, NetDeviceStats};
use crate::packetbuf::PacketBuf;
use crate::pool::PacketPool;
use crate::types::{MacAddr, NetError};

pub struct BlackholeDev {
    mac: MacAddr,
    up: AtomicBool,
    tx_packets: AtomicU64,
    tx_bytes: AtomicU64,
    tx_dropped: AtomicU64,
}

impl BlackholeDev {
    pub const fn new(mac: MacAddr) -> Self {
        Self {
            mac,
            up: AtomicBool::new(true),
            tx_packets: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            tx_dropped: AtomicU64::new(0),
        }
    }

    /// Frames this device swallowed — how a test proves a send happened without
    /// a peer existing.
    pub fn tx_packets(&self) -> u64 {
        self.tx_packets.load(Ordering::Relaxed)
    }
}

impl NetDevice for BlackholeDev {
    fn tx(&self, pkt: PacketBuf) -> Result<(), NetError> {
        if !self.up.load(Ordering::Acquire) {
            self.tx_dropped.fetch_add(1, Ordering::Relaxed);
            return Err(NetError::NetworkUnreachable);
        }
        self.tx_packets.fetch_add(1, Ordering::Relaxed);
        self.tx_bytes.fetch_add(pkt.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn poll_rx(&self, _budget: usize, _pool: &'static PacketPool) -> KVec<PacketBuf> {
        KVec::new()
    }

    fn set_up(&self) {
        self.up.store(true, Ordering::Release);
    }

    fn set_down(&self) {
        self.up.store(false, Ordering::Release);
    }

    fn mtu(&self) -> u16 {
        1500
    }

    fn mac(&self) -> MacAddr {
        self.mac
    }

    fn stats(&self) -> NetDeviceStats {
        NetDeviceStats {
            tx_packets: self.tx_packets.load(Ordering::Relaxed),
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
            tx_dropped: self.tx_dropped.load(Ordering::Relaxed),
            ..NetDeviceStats::new()
        }
    }

    fn features(&self) -> NetDeviceFeatures {
        NetDeviceFeatures::empty()
    }

    fn kind(&self) -> IfaceKind {
        IfaceKind::Ethernet
    }

    fn carrier(&self) -> bool {
        self.up.load(Ordering::Acquire)
    }

    fn carrier_detect(&self) -> bool {
        true
    }
}
