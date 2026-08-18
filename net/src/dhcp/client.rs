//! The DHCP client state machine.
//!
//! I/O-free, in the same idiom as [`DnsResolver`](crate::dns): it is fed events
//! and returns actions, and it neither sends a packet nor reads a clock of its
//! own.
//!
//! `Renewing` unicasts to the granting server; `Rebinding` broadcasts, and is
//! entered only once that server has stopped answering.

use super::codec::{self, DHCP_FRAME_LEN, DhcpReply, MSG_ACK, MSG_NAK, MSG_OFFER};

/// First retransmission delay, and the base the backoff doubles from.
pub const RETRY_BASE_MS: u32 = 4_000;
/// Backoff ceiling. RFC 2131 §4.1 puts it at 64 s.
pub const RETRY_MAX_MS: u32 = 64_000;
/// Bound on the randomisation applied to every retransmission, ±1 s per
/// RFC 2131 §4.1. Without it a segment full of machines that lost the same
/// server retransmits in lockstep.
pub const RETRY_JITTER_MS: u32 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DhcpDest {
    /// 255.255.255.255 at L3, broadcast at L2.
    Broadcast,
    /// Unicast to the server that granted the lease.
    Server([u8; 4]),
}

/// Why a lease is being torn down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnbindReason {
    /// The server refused to confirm the address.
    Nak,
    /// The lease ran out without a renewal succeeding.
    Expired,
    /// The client was stopped administratively.
    Stopped,
}

/// A lease, as the caller must install it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DhcpBinding {
    pub addr: [u8; 4],
    pub mask: [u8; 4],
    pub router: [u8; 4],
    pub dns: [u8; 4],
    pub server_id: [u8; 4],
    pub lease_secs: u32,
    pub t1_secs: u32,
    pub t2_secs: u32,
}

/// What the caller must do next.
///
/// Frames are not carried in here — the caller reads [`DhcpClient::frame`] —
/// because a 320-byte payload inside an enum would be copied at every return,
/// against a 2 KiB kernel stack budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DhcpAction {
    /// Nothing to do.
    Idle,
    /// Transmit [`frame`](DhcpClient::frame) to `dest` and arm the
    /// retransmission timer for `retry_ms`.
    Send { dest: DhcpDest, retry_ms: u32 },
    /// Install this lease, then arm T1, T2 and expiry.
    Bind(DhcpBinding),
    /// Tear the lease down and cancel every timer.
    Unbind(UnbindReason),
    /// Tear the lease down, then start discovering again: a NAK means the
    /// address is not ours.
    UnbindThenSend {
        reason: UnbindReason,
        dest: DhcpDest,
        retry_ms: u32,
    },
    /// Transmit, then tear down: RELEASE must reach the wire while the address
    /// is still configured.
    SendThenUnbind {
        dest: DhcpDest,
        reason: UnbindReason,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum DhcpEvent<'a> {
    /// Begin, or restart after a stop.
    Start,
    /// Give the lease back and stop.
    Stop,
    /// A UDP payload arrived on port 68.
    Reply(&'a [u8]),
    Retransmit,
    /// T1: time to renew with the granting server.
    T1,
    /// T2: time to ask anybody.
    T2,
    Expire,
    /// The link went away; the address is kept, since the server holds the
    /// binding regardless.
    CarrierDown,
    /// The link came back; confirm the address we still hold.
    CarrierUp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DhcpState {
    Init,
    Selecting,
    Requesting,
    Bound,
    Renewing,
    Rebinding,
}

impl DhcpState {
    /// `NET_DHCP_*`, for `net_query` and the monitor's DHCP records.
    pub const fn to_abi(self) -> u8 {
        match self {
            Self::Init => slopos_abi::net::NET_DHCP_INIT,
            Self::Selecting => slopos_abi::net::NET_DHCP_SELECTING,
            Self::Requesting => slopos_abi::net::NET_DHCP_REQUESTING,
            Self::Bound => slopos_abi::net::NET_DHCP_BOUND,
            Self::Renewing => slopos_abi::net::NET_DHCP_RENEWING,
            Self::Rebinding => slopos_abi::net::NET_DHCP_REBINDING,
        }
    }
}

/// One interface's DHCP client.
pub struct DhcpClient {
    state: DhcpState,
    mac: [u8; 6],
    xid: u32,
    /// Transaction-id generator, seeded from the kernel RNG: a predictable xid
    /// lets an off-path attacker answer the transaction.
    rng: u32,

    addr: [u8; 4],
    mask: [u8; 4],
    router: [u8; 4],
    dns: [u8; 4],
    server_id: [u8; 4],
    lease_secs: u32,
    t1_secs: u32,
    t2_secs: u32,

    /// Backoff base, before jitter.
    retry_base_ms: u32,
    /// True while the link is down: timers are stopped but the lease is kept.
    link_down: bool,

    tx: [u8; DHCP_FRAME_LEN],
    tx_len: usize,
}

impl DhcpClient {
    /// A client that has not started. `seed` must come from the kernel RNG in
    /// production.
    pub const fn new(mac: [u8; 6], seed: u32) -> Self {
        Self {
            state: DhcpState::Init,
            mac,
            xid: 0,
            // Folded constant: a zero seed would make xorshift a fixed point.
            rng: seed ^ 0x9E37_79B9,
            addr: [0; 4],
            mask: [0; 4],
            router: [0; 4],
            dns: [0; 4],
            server_id: [0; 4],
            lease_secs: 0,
            t1_secs: 0,
            t2_secs: 0,
            retry_base_ms: RETRY_BASE_MS,
            link_down: false,
            tx: [0; DHCP_FRAME_LEN],
            tx_len: 0,
        }
    }

    /// Re-initialise in place for a fresh start.
    ///
    /// In place rather than by assigning `DhcpClient::new(..)`: the ~360-byte
    /// rvalue would land on the caller's stack against a 2 KiB frame budget.
    pub fn reset(&mut self, mac: [u8; 6], seed: u32) {
        self.state = DhcpState::Init;
        self.mac = mac;
        self.xid = 0;
        self.rng = seed ^ 0x9E37_79B9;
        self.clear_lease();
        self.retry_base_ms = RETRY_BASE_MS;
        self.link_down = false;
        self.tx_len = 0;
    }

    #[inline]
    pub const fn state(&self) -> DhcpState {
        self.state
    }

    /// The address currently held, or all-zero.
    #[inline]
    pub const fn address(&self) -> [u8; 4] {
        self.addr
    }

    #[inline]
    pub const fn server_id(&self) -> [u8; 4] {
        self.server_id
    }

    #[inline]
    pub const fn lease_secs(&self) -> u32 {
        self.lease_secs
    }

    #[inline]
    pub const fn t1_secs(&self) -> u32 {
        self.t1_secs
    }

    #[inline]
    pub const fn t2_secs(&self) -> u32 {
        self.t2_secs
    }

    /// The transaction id currently in flight.
    #[inline]
    pub const fn xid(&self) -> u32 {
        self.xid
    }

    /// The retransmission delay before jitter.
    #[inline]
    pub const fn retry_base_ms(&self) -> u32 {
        self.retry_base_ms
    }

    /// The frame the last [`DhcpAction::Send`] refers to.
    #[inline]
    pub fn frame(&self) -> &[u8] {
        &self.tx[..self.tx_len]
    }

    /// xorshift32.
    fn next_random(&mut self) -> u32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        x
    }

    fn new_transaction(&mut self) {
        self.xid = self.next_random();
        self.retry_base_ms = RETRY_BASE_MS;
    }

    /// The delay to arm, jittered; the base is left untouched so the doubling
    /// sequence stays exact.
    fn jittered_retry(&mut self) -> u32 {
        let spread = 2 * RETRY_JITTER_MS + 1;
        let offset = self.next_random() % spread;
        self.retry_base_ms
            .saturating_add(offset)
            .saturating_sub(RETRY_JITTER_MS)
            .max(1)
    }

    /// Double the backoff, capped; retransmission never gives up.
    fn back_off(&mut self) {
        self.retry_base_ms = self.retry_base_ms.saturating_mul(2).min(RETRY_MAX_MS);
    }

    fn send(&mut self, dest: DhcpDest) -> DhcpAction {
        let retry_ms = self.jittered_retry();
        DhcpAction::Send { dest, retry_ms }
    }

    fn discover(&mut self) -> DhcpAction {
        self.new_transaction();
        self.tx_len = codec::build_discover(self.mac, self.xid, &mut self.tx);
        self.state = DhcpState::Selecting;
        self.send(DhcpDest::Broadcast)
    }

    /// Apply a reply's lease times, filling in the RFC 2131 §4.4.5 defaults for
    /// whichever of T1 and T2 the server left out; treating the absent values
    /// as zero would renew continuously.
    fn adopt(&mut self, reply: &DhcpReply) -> DhcpBinding {
        self.addr = reply.yiaddr;
        self.mask = reply.subnet_mask;
        self.router = reply.router;
        self.dns = reply.dns;
        if reply.server_id != [0; 4] {
            self.server_id = reply.server_id;
        }
        self.lease_secs = reply.lease_secs.unwrap_or(0);
        self.t1_secs = reply.t1_secs.unwrap_or(self.lease_secs / 2);
        self.t2_secs = reply.t2_secs.unwrap_or(self.lease_secs / 8 * 7);
        DhcpBinding {
            addr: self.addr,
            mask: self.mask,
            router: self.router,
            dns: self.dns,
            server_id: self.server_id,
            lease_secs: self.lease_secs,
            t1_secs: self.t1_secs,
            t2_secs: self.t2_secs,
        }
    }

    fn clear_lease(&mut self) {
        self.addr = [0; 4];
        self.mask = [0; 4];
        self.router = [0; 4];
        self.dns = [0; 4];
        self.server_id = [0; 4];
        self.lease_secs = 0;
        self.t1_secs = 0;
        self.t2_secs = 0;
    }

    /// Feed one event and get the work it implies.
    ///
    /// `now_ms` is unused: transitions are driven by the timer events the
    /// caller arms, not by comparing clocks in here.
    pub fn step(&mut self, event: DhcpEvent<'_>, now_ms: u64) -> DhcpAction {
        let _ = now_ms;
        match event {
            DhcpEvent::Start => self.on_start(),
            DhcpEvent::Stop => self.on_stop(),
            DhcpEvent::Reply(payload) => self.on_reply(payload),
            DhcpEvent::Retransmit => self.on_retransmit(),
            DhcpEvent::T1 => self.on_t1(),
            DhcpEvent::T2 => self.on_t2(),
            DhcpEvent::Expire => self.on_expire(),
            DhcpEvent::CarrierDown => self.on_carrier_down(),
            DhcpEvent::CarrierUp => self.on_carrier_up(),
        }
    }

    fn on_start(&mut self) -> DhcpAction {
        if matches!(self.state, DhcpState::Bound) {
            // Starting again is not a reason to give up a working lease.
            return DhcpAction::Idle;
        }
        self.link_down = false;
        self.discover()
    }

    fn on_stop(&mut self) -> DhcpAction {
        match self.state {
            // Nothing is configured yet, so there is nothing to give back.
            DhcpState::Init | DhcpState::Selecting | DhcpState::Requesting => {
                self.state = DhcpState::Init;
                self.clear_lease();
                DhcpAction::Idle
            }
            DhcpState::Bound | DhcpState::Renewing | DhcpState::Rebinding => {
                self.new_transaction();
                self.tx_len = codec::build_release(
                    self.mac,
                    self.xid,
                    self.addr,
                    self.server_id,
                    &mut self.tx,
                );
                let dest = DhcpDest::Server(self.server_id);
                self.state = DhcpState::Init;
                DhcpAction::SendThenUnbind {
                    dest,
                    reason: UnbindReason::Stopped,
                }
            }
        }
    }

    fn on_retransmit(&mut self) -> DhcpAction {
        match self.state {
            DhcpState::Selecting => {
                self.back_off();
                self.tx_len = codec::build_discover(self.mac, self.xid, &mut self.tx);
                self.send(DhcpDest::Broadcast)
            }
            DhcpState::Requesting => {
                self.back_off();
                // The frame is unchanged from the first attempt.
                self.send(DhcpDest::Broadcast)
            }
            DhcpState::Renewing => {
                self.back_off();
                self.send(DhcpDest::Server(self.server_id))
            }
            DhcpState::Rebinding => {
                self.back_off();
                self.send(DhcpDest::Broadcast)
            }
            DhcpState::Init | DhcpState::Bound => DhcpAction::Idle,
        }
    }

    fn on_reply(&mut self, payload: &[u8]) -> DhcpAction {
        let Some(reply) = codec::parse_reply(payload, self.xid) else {
            return DhcpAction::Idle;
        };

        match (self.state, reply.msg_type) {
            (DhcpState::Selecting, MSG_OFFER) => {
                if reply.server_id == [0; 4] || reply.yiaddr == [0; 4] {
                    // An offer that names no server cannot be requested from.
                    return DhcpAction::Idle;
                }
                self.server_id = reply.server_id;
                self.retry_base_ms = RETRY_BASE_MS;
                self.tx_len = codec::build_request_selecting(
                    self.mac,
                    self.xid,
                    reply.yiaddr,
                    reply.server_id,
                    &mut self.tx,
                );
                self.state = DhcpState::Requesting;
                self.send(DhcpDest::Broadcast)
            }
            (DhcpState::Requesting, MSG_ACK)
            | (DhcpState::Renewing, MSG_ACK)
            | (DhcpState::Rebinding, MSG_ACK) => {
                let binding = self.adopt(&reply);
                self.state = DhcpState::Bound;
                self.retry_base_ms = RETRY_BASE_MS;
                DhcpAction::Bind(binding)
            }
            (DhcpState::Requesting, MSG_NAK) => {
                // Nothing was ever installed, so there is nothing to unbind.
                self.clear_lease();
                self.discover()
            }
            (DhcpState::Renewing, MSG_NAK) | (DhcpState::Rebinding, MSG_NAK) => {
                // Give the address up immediately: keeping a NAKed address
                // risks a second host on it.
                self.clear_lease();
                let action = self.discover();
                let DhcpAction::Send { dest, retry_ms } = action else {
                    return action;
                };
                DhcpAction::UnbindThenSend {
                    reason: UnbindReason::Nak,
                    dest,
                    retry_ms,
                }
            }
            _ => DhcpAction::Idle,
        }
    }

    fn on_t1(&mut self) -> DhcpAction {
        if !matches!(self.state, DhcpState::Bound) || self.link_down {
            return DhcpAction::Idle;
        }
        self.new_transaction();
        self.tx_len =
            codec::build_request_renew(self.mac, self.xid, self.addr, false, &mut self.tx);
        self.state = DhcpState::Renewing;
        self.send(DhcpDest::Server(self.server_id))
    }

    fn on_t2(&mut self) -> DhcpAction {
        if !matches!(self.state, DhcpState::Bound | DhcpState::Renewing) || self.link_down {
            return DhcpAction::Idle;
        }
        self.new_transaction();
        // Broadcast, with no server identifier: the granting server has stopped
        // answering.
        self.tx_len = codec::build_request_renew(self.mac, self.xid, self.addr, true, &mut self.tx);
        self.state = DhcpState::Rebinding;
        self.send(DhcpDest::Broadcast)
    }

    fn on_expire(&mut self) -> DhcpAction {
        match self.state {
            DhcpState::Bound | DhcpState::Renewing | DhcpState::Rebinding => {
                self.clear_lease();
                self.state = DhcpState::Init;
                DhcpAction::Unbind(UnbindReason::Expired)
            }
            _ => DhcpAction::Idle,
        }
    }

    fn on_carrier_down(&mut self) -> DhcpAction {
        self.link_down = true;
        DhcpAction::Idle
    }

    fn on_carrier_up(&mut self) -> DhcpAction {
        self.link_down = false;
        if self.addr == [0; 4] {
            return self.discover();
        }
        // INIT-REBOOT: broadcast a request for the address still held, with no
        // server identifier — the segment may not be the one the lease came
        // from, and any server there is entitled to NAK it.
        self.new_transaction();
        self.tx_len = codec::build_request_reboot(self.mac, self.xid, self.addr, &mut self.tx);
        self.state = DhcpState::Requesting;
        self.send(DhcpDest::Broadcast)
    }
}
