//! DHCP wire format: building the messages this client sends and decoding what
//! a server sends back.  Pure encode/decode with no state and no I/O; the state
//! machine that decides what any of it *means* is [`super::client`].

pub const UDP_PORT_SERVER: u16 = 67;
pub const UDP_PORT_CLIENT: u16 = 68;

const BOOTREQUEST: u8 = 1;
const BOOTREPLY: u8 = 2;
const FLAGS_BROADCAST: u16 = 0x8000;
const MAGIC_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];

const OPTION_PAD: u8 = 0;
const OPTION_SUBNET_MASK: u8 = 1;
const OPTION_ROUTER: u8 = 3;
const OPTION_DNS: u8 = 6;
const OPTION_REQUESTED_IP: u8 = 50;
const OPTION_MSG_TYPE: u8 = 53;
const OPTION_SERVER_ID: u8 = 54;
const OPTION_LEASE_TIME: u8 = 51;
const OPTION_PARAM_REQ_LIST: u8 = 55;
const OPTION_RENEWAL_T1: u8 = 58;
const OPTION_REBINDING_T2: u8 = 59;
/// Servers key their lease database on it, so omitting it makes a reboot look
/// like a different client and burn a fresh address — RFC 2131 §4.4.1's
/// "SHOULD".
const OPTION_CLIENT_ID: u8 = 61;
const OPTION_END: u8 = 255;

/// Hardware type 1, Ethernet — the first byte of a MAC-based client id.
const HTYPE_ETHERNET: u8 = 1;

pub const MSG_DISCOVER: u8 = 1;
pub const MSG_OFFER: u8 = 2;
pub const MSG_REQUEST: u8 = 3;
pub const MSG_DECLINE: u8 = 4;
pub const MSG_ACK: u8 = 5;
pub const MSG_NAK: u8 = 6;
pub const MSG_RELEASE: u8 = 7;

pub const BOOTP_HEADER_LEN: usize = 240;

/// Every message this client sends is exactly this long: a constant-size buffer
/// means no allocation and no length arithmetic on a path that runs before the
/// machine has an address, and RFC 2131 constrains only what a client accepts.
pub const DHCP_FRAME_LEN: usize = 320;

#[derive(Clone, Copy)]
pub struct DhcpLease {
    pub ipv4: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub router: [u8; 4],
    pub dns: [u8; 4],
}

impl DhcpLease {
    pub fn is_valid(&self) -> bool {
        self.ipv4 != [0; 4]
    }
}

// 240 of the 320 bytes are the BOOTP header, leaving 80 for options against a
// worst case of 32.

/// Bounds-checked option writer: an option that would not fit is silently
/// dropped, so a miscounted length is a missing option rather than a write past
/// the frame.
struct OptWriter<'a> {
    buf: &'a mut [u8; DHCP_FRAME_LEN],
    at: usize,
}

impl<'a> OptWriter<'a> {
    fn new(buf: &'a mut [u8; DHCP_FRAME_LEN], at: usize) -> Self {
        Self { buf, at }
    }

    /// Dropped whole if it would not fit, never truncated: a half-written
    /// option would desynchronise every parser downstream.
    fn put(&mut self, code: u8, data: &[u8]) {
        let need = 2 + data.len();
        if data.len() > u8::MAX as usize || self.at + need + 1 > DHCP_FRAME_LEN {
            return;
        }
        self.buf[self.at] = code;
        self.buf[self.at + 1] = data.len() as u8;
        self.buf[self.at + 2..self.at + need].copy_from_slice(data);
        self.at += need;
    }

    fn finish(self) -> usize {
        self.buf[self.at] = OPTION_END;
        self.at + 1
    }
}

/// `ciaddr` must be set exactly when the client already holds a lease it is
/// confirming — RENEWING, REBINDING and RELEASE.  `broadcast` asks the server
/// to broadcast its reply: unavoidable for a client with no configured address,
/// and must *not* be set by a renewing client, which can receive unicast.
fn write_bootp_header(
    out: &mut [u8; DHCP_FRAME_LEN],
    mac: [u8; 6],
    xid: u32,
    ciaddr: [u8; 4],
    broadcast: bool,
) -> usize {
    out.fill(0);
    out[0] = BOOTREQUEST;
    out[1] = HTYPE_ETHERNET;
    out[2] = 6; // hlen: 6-byte MAC
    out[4..8].copy_from_slice(&xid.to_be_bytes());
    if broadcast {
        out[10..12].copy_from_slice(&FLAGS_BROADCAST.to_be_bytes());
    }
    out[12..16].copy_from_slice(&ciaddr);
    out[28..34].copy_from_slice(&mac);
    out[236..240].copy_from_slice(&MAGIC_COOKIE);
    BOOTP_HEADER_LEN
}

/// The client identifier: hardware type then MAC.
fn put_client_id(w: &mut OptWriter<'_>, mac: [u8; 6]) {
    let id = [
        HTYPE_ETHERNET,
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5],
    ];
    w.put(OPTION_CLIENT_ID, &id);
}

/// The options this client knows how to apply; anything more would be bytes a
/// server spends on something that gets parsed and dropped.
fn put_param_request_list(w: &mut OptWriter<'_>) {
    w.put(
        OPTION_PARAM_REQ_LIST,
        &[
            OPTION_SUBNET_MASK,
            OPTION_ROUTER,
            OPTION_DNS,
            OPTION_LEASE_TIME,
            OPTION_RENEWAL_T1,
            OPTION_REBINDING_T2,
        ],
    );
}

/// DISCOVER: no address yet, so no `ciaddr` and the broadcast flag set.
pub fn build_discover(mac: [u8; 6], xid: u32, out: &mut [u8; DHCP_FRAME_LEN]) -> usize {
    let at = write_bootp_header(out, mac, xid, [0; 4], true);
    let mut w = OptWriter::new(out, at);
    w.put(OPTION_MSG_TYPE, &[MSG_DISCOVER]);
    put_client_id(&mut w, mac);
    put_param_request_list(&mut w);
    w.finish()
}

/// REQUEST in SELECTING: names both the address being accepted (option 50) and
/// the server whose offer it is (option 54), so the servers that lost the race
/// know to release their own offers.
pub fn build_request_selecting(
    mac: [u8; 6],
    xid: u32,
    requested_ip: [u8; 4],
    server_id: [u8; 4],
    out: &mut [u8; DHCP_FRAME_LEN],
) -> usize {
    let at = write_bootp_header(out, mac, xid, [0; 4], true);
    let mut w = OptWriter::new(out, at);
    w.put(OPTION_MSG_TYPE, &[MSG_REQUEST]);
    w.put(OPTION_REQUESTED_IP, &requested_ip);
    w.put(OPTION_SERVER_ID, &server_id);
    put_client_id(&mut w, mac);
    put_param_request_list(&mut w);
    w.finish()
}

/// REQUEST in INIT-REBOOT: the client believes it still holds `requested_ip`
/// and wants it confirmed. No server identifier — it does not know which server
/// to ask — so this is broadcast and any server may answer or NAK.
pub fn build_request_reboot(
    mac: [u8; 6],
    xid: u32,
    requested_ip: [u8; 4],
    out: &mut [u8; DHCP_FRAME_LEN],
) -> usize {
    let at = write_bootp_header(out, mac, xid, [0; 4], true);
    let mut w = OptWriter::new(out, at);
    w.put(OPTION_MSG_TYPE, &[MSG_REQUEST]);
    w.put(OPTION_REQUESTED_IP, &requested_ip);
    put_client_id(&mut w, mac);
    put_param_request_list(&mut w);
    w.finish()
}

/// REQUEST in RENEWING or REBINDING: the address is already configured, so it
/// travels in `ciaddr` rather than option 50, and neither option 50 nor 54 is
/// present. RFC 2131 §4.3.2 is explicit that including them here is an error.
pub fn build_request_renew(
    mac: [u8; 6],
    xid: u32,
    ciaddr: [u8; 4],
    broadcast: bool,
    out: &mut [u8; DHCP_FRAME_LEN],
) -> usize {
    let at = write_bootp_header(out, mac, xid, ciaddr, broadcast);
    let mut w = OptWriter::new(out, at);
    w.put(OPTION_MSG_TYPE, &[MSG_REQUEST]);
    put_client_id(&mut w, mac);
    put_param_request_list(&mut w);
    w.finish()
}

/// RELEASE: unicast to the server that granted the lease, with the address in
/// `ciaddr`.
pub fn build_release(
    mac: [u8; 6],
    xid: u32,
    ciaddr: [u8; 4],
    server_id: [u8; 4],
    out: &mut [u8; DHCP_FRAME_LEN],
) -> usize {
    let at = write_bootp_header(out, mac, xid, ciaddr, false);
    let mut w = OptWriter::new(out, at);
    w.put(OPTION_MSG_TYPE, &[MSG_RELEASE]);
    w.put(OPTION_SERVER_ID, &server_id);
    put_client_id(&mut w, mac);
    w.finish()
}

/// Lease times are `Option` because their absence is meaningful: RFC 2131
/// §4.4.5 gives defaults for T1 and T2 derived from the lease, and a client
/// that silently substituted zero would renew immediately and forever.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DhcpReply {
    /// `MSG_*`.
    pub msg_type: u8,
    /// The address being offered or confirmed.
    pub yiaddr: [u8; 4],
    /// Next-server address from the BOOTP header.
    pub siaddr: [u8; 4],
    pub server_id: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub router: [u8; 4],
    pub dns: [u8; 4],
    pub lease_secs: Option<u32>,
    pub t1_secs: Option<u32>,
    pub t2_secs: Option<u32>,
}

#[derive(Clone, Copy, Default)]
struct DhcpOptions {
    message_type: u8,
    server_id: [u8; 4],
    subnet_mask: [u8; 4],
    router: [u8; 4],
    dns: [u8; 4],
    lease_secs: Option<u32>,
    t1_secs: Option<u32>,
    t2_secs: Option<u32>,
}

fn be32(data: &[u8]) -> u32 {
    u32::from_be_bytes([data[0], data[1], data[2], data[3]])
}

/// Every step is bounded by the slice length, so a truncated or malformed
/// packet stops the walk rather than reading past the end: the input is an
/// unauthenticated broadcast from an unverified server.
fn parse_options(options: &[u8]) -> DhcpOptions {
    let mut opts = DhcpOptions::default();
    let mut i = 0usize;
    while i < options.len() {
        let code = options[i];
        if code == OPTION_END {
            break;
        }
        if code == OPTION_PAD {
            i += 1;
            continue;
        }
        if i + 1 >= options.len() {
            break;
        }
        let len = options[i + 1] as usize;
        if i + 2 + len > options.len() {
            break;
        }

        let data = &options[i + 2..i + 2 + len];
        match code {
            OPTION_MSG_TYPE if len >= 1 => opts.message_type = data[0],
            OPTION_SERVER_ID if len >= 4 => opts.server_id.copy_from_slice(&data[..4]),
            OPTION_SUBNET_MASK if len >= 4 => opts.subnet_mask.copy_from_slice(&data[..4]),
            OPTION_ROUTER if len >= 4 => opts.router.copy_from_slice(&data[..4]),
            OPTION_DNS if len >= 4 => opts.dns.copy_from_slice(&data[..4]),
            OPTION_LEASE_TIME if len >= 4 => opts.lease_secs = Some(be32(data)),
            OPTION_RENEWAL_T1 if len >= 4 => opts.t1_secs = Some(be32(data)),
            OPTION_REBINDING_T2 if len >= 4 => opts.t2_secs = Some(be32(data)),
            _ => {}
        }

        i += 2 + len;
    }

    opts
}

/// Checks only what identifies the reply as ours: a BOOTP reply, our
/// transaction, the magic cookie, and a message type.  Whether that type is one
/// the client wants *right now* is the state machine's decision, which is why
/// an unexpected type comes back decoded rather than as `None`.
pub fn parse_reply(payload: &[u8], xid: u32) -> Option<DhcpReply> {
    if payload.len() < BOOTP_HEADER_LEN {
        return None;
    }
    if payload[0] != BOOTREPLY {
        return None;
    }
    if be32(&payload[4..8]) != xid {
        return None;
    }
    if payload[236..240] != MAGIC_COOKIE {
        return None;
    }

    let opts = parse_options(&payload[BOOTP_HEADER_LEN..]);
    if opts.message_type == 0 {
        return None;
    }

    Some(DhcpReply {
        msg_type: opts.message_type,
        yiaddr: [payload[16], payload[17], payload[18], payload[19]],
        siaddr: [payload[20], payload[21], payload[22], payload[23]],
        server_id: opts.server_id,
        subnet_mask: opts.subnet_mask,
        router: opts.router,
        dns: opts.dns,
        lease_secs: opts.lease_secs,
        t1_secs: opts.t1_secs,
        t2_secs: opts.t2_secs,
    })
}
