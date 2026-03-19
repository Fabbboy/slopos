//! DHCP client packet construction and parsing.

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
const OPTION_PARAM_REQ_LIST: u8 = 55;
const OPTION_END: u8 = 255;

pub const MSG_DISCOVER: u8 = 1;
pub const MSG_OFFER: u8 = 2;
pub const MSG_REQUEST: u8 = 3;
pub const MSG_ACK: u8 = 5;

pub const BOOTP_HEADER_LEN: usize = 240;

#[derive(Clone, Copy, Default)]
struct DhcpOptions {
    message_type: u8,
    server_id: [u8; 4],
    subnet_mask: [u8; 4],
    router: [u8; 4],
    dns: [u8; 4],
}

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

#[derive(Clone, Copy)]
pub struct DhcpOffer {
    pub yiaddr: [u8; 4],
    pub server_id: [u8; 4],
    pub subnet_mask: [u8; 4],
    pub router: [u8; 4],
    pub dns: [u8; 4],
}

// =============================================================================
// Packet construction
// =============================================================================

/// Write the common BOOTP header fields shared by DISCOVER and REQUEST.
/// Returns the byte offset where DHCP options should begin (`BOOTP_HEADER_LEN`).
fn write_bootp_header(out: &mut [u8; 320], mac: [u8; 6], xid: u32) -> usize {
    out.fill(0);
    out[0] = BOOTREQUEST;
    out[1] = 1; // htype: Ethernet
    out[2] = 6; // hlen:  6-byte MAC
    out[4..8].copy_from_slice(&xid.to_be_bytes());
    out[10..12].copy_from_slice(&FLAGS_BROADCAST.to_be_bytes());
    out[28..34].copy_from_slice(&mac);
    out[236..240].copy_from_slice(&MAGIC_COOKIE);
    BOOTP_HEADER_LEN
}

/// Append the standard parameter-request-list option and the END marker.
/// Returns the final packet length.
fn finish_options(out: &mut [u8; 320], mut i: usize) -> usize {
    out[i] = OPTION_PARAM_REQ_LIST;
    out[i + 1] = 3;
    out[i + 2] = OPTION_SUBNET_MASK;
    out[i + 3] = OPTION_ROUTER;
    out[i + 4] = OPTION_DNS;
    i += 5;

    out[i] = OPTION_END;
    i + 1
}

pub fn build_discover(mac: [u8; 6], xid: u32, out: &mut [u8; 320]) -> usize {
    let mut i = write_bootp_header(out, mac, xid);

    out[i] = OPTION_MSG_TYPE;
    out[i + 1] = 1;
    out[i + 2] = MSG_DISCOVER;
    i += 3;

    finish_options(out, i)
}

pub fn build_request(mac: [u8; 6], xid: u32, offer: DhcpOffer, out: &mut [u8; 320]) -> usize {
    let mut i = write_bootp_header(out, mac, xid);

    out[i] = OPTION_MSG_TYPE;
    out[i + 1] = 1;
    out[i + 2] = MSG_REQUEST;
    i += 3;

    out[i] = OPTION_REQUESTED_IP;
    out[i + 1] = 4;
    out[i + 2..i + 6].copy_from_slice(&offer.yiaddr);
    i += 6;

    out[i] = OPTION_SERVER_ID;
    out[i + 1] = 4;
    out[i + 2..i + 6].copy_from_slice(&offer.server_id);
    i += 6;

    finish_options(out, i)
}

// =============================================================================
// Parsing
// =============================================================================

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
            _ => {}
        }

        i += 2 + len;
    }

    opts
}

pub fn parse_bootp_reply(payload: &[u8], xid: u32, expected_type: u8) -> Option<DhcpOffer> {
    if payload.len() < BOOTP_HEADER_LEN {
        return None;
    }
    if payload[0] != BOOTREPLY {
        return None;
    }
    if u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]) != xid {
        return None;
    }
    if payload[236..240] != MAGIC_COOKIE {
        return None;
    }

    let options = parse_options(&payload[BOOTP_HEADER_LEN..]);

    if options.message_type != expected_type {
        return None;
    }
    if expected_type == MSG_OFFER && options.server_id == [0; 4] {
        return None;
    }

    Some(DhcpOffer {
        yiaddr: [payload[16], payload[17], payload[18], payload[19]],
        server_id: options.server_id,
        subnet_mask: options.subnet_mask,
        router: options.router,
        dns: options.dns,
    })
}
