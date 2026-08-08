//! IPv4 addresses.

use core::fmt;

/// A four-octet IPv4 address, in network order.
///
/// Named `Ipv4` rather than `Ipv4Addr` so that host-side code and tests can
/// name it alongside `std::net::Ipv4Addr` without either shadowing the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct Ipv4(pub [u8; 4]);

impl Ipv4 {
    /// `0.0.0.0`.
    pub const UNSPECIFIED: Ipv4 = Ipv4([0, 0, 0, 0]);

    #[inline]
    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self([a, b, c, d])
    }

    #[inline]
    pub const fn octets(self) -> [u8; 4] {
        self.0
    }

    /// `0.0.0.0` — the wildcard, and what a route uses for "directly
    /// connected" rather than a gateway.
    #[inline]
    pub const fn is_unspecified(self) -> bool {
        let o = self.0;
        o[0] == 0 && o[1] == 0 && o[2] == 0 && o[3] == 0
    }

    /// Anything in `127.0.0.0/8`, per RFC 1122 — not just `127.0.0.1`.
    #[inline]
    pub const fn is_loopback(self) -> bool {
        self.0[0] == 127
    }

    /// Parses a dotted-quad literal.
    ///
    /// Strictly four decimal parts, each one to three digits and each at most
    /// 255. There is no shorthand form: `10.1` is not `10.0.0.1` and
    /// `0x0a000001` is not an address, because `inet_aton`'s octal and
    /// short-form rules are a documented source of SSRF filter bypasses. A
    /// leading zero is read as decimal (`010` is 10, not 8) and is accepted
    /// rather than rejected, which is the one leniency kept.
    pub fn from_str_bytes(text: &[u8]) -> Option<Ipv4> {
        let mut out = [0u8; 4];
        let mut iter = text.split(|&b| b == b'.');
        for slot in out.iter_mut() {
            let piece = iter.next()?;
            if piece.is_empty() || piece.len() > 3 {
                return None;
            }
            let mut val: u32 = 0;
            for &b in piece {
                if !b.is_ascii_digit() {
                    return None;
                }
                val = val * 10 + u32::from(b - b'0');
                if val > 255 {
                    return None;
                }
            }
            *slot = val as u8;
        }
        if iter.next().is_some() {
            return None;
        }
        Some(Ipv4(out))
    }
}

impl From<[u8; 4]> for Ipv4 {
    #[inline]
    fn from(octets: [u8; 4]) -> Self {
        Self(octets)
    }
}

impl From<Ipv4> for [u8; 4] {
    #[inline]
    fn from(addr: Ipv4) -> Self {
        addr.0
    }
}

impl fmt::Display for Ipv4 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let o = &self.0;
        write!(f, "{}.{}.{}.{}", o[0], o[1], o[2], o[3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::columns::TestBuf;

    #[test]
    fn parse_accepts_standard() {
        assert_eq!(Ipv4::from_str_bytes(b"10.0.0.1"), Some(Ipv4([10, 0, 0, 1])));
        assert_eq!(Ipv4::from_str_bytes(b"0.0.0.0"), Some(Ipv4([0, 0, 0, 0])));
        assert_eq!(
            Ipv4::from_str_bytes(b"255.255.255.255"),
            Some(Ipv4([255; 4]))
        );
    }

    #[test]
    fn parse_rejects_malformed() {
        assert_eq!(Ipv4::from_str_bytes(b""), None);
        assert_eq!(Ipv4::from_str_bytes(b"10.0.0"), None);
        assert_eq!(Ipv4::from_str_bytes(b"10.0.0.1.2"), None);
        assert_eq!(Ipv4::from_str_bytes(b"10.0.0.256"), None);
        assert_eq!(Ipv4::from_str_bytes(b"abc.def.ghi.jkl"), None);
        assert_eq!(Ipv4::from_str_bytes(b"10..0.1"), None);
        assert_eq!(Ipv4::from_str_bytes(b"10.0.0.1 "), None);
    }

    /// Pinned rather than incidental: a leading zero is decimal here, so
    /// `010.0.2.15` is `10.0.2.15` and not the `8.0.2.15` an octal-aware
    /// `inet_aton` would return.
    #[test]
    fn parse_reads_leading_zeros_as_decimal() {
        assert_eq!(
            Ipv4::from_str_bytes(b"010.0.2.15"),
            Some(Ipv4([10, 0, 2, 15]))
        );
        assert_eq!(Ipv4::from_str_bytes(b"0000.0.0.0"), None);
    }

    #[test]
    fn display_is_dotted_quad() {
        let mut buf = TestBuf::new();
        buf.push_fmt(format_args!("{}", Ipv4([192, 168, 1, 255])));
        assert_eq!(buf.as_str(), "192.168.1.255");
    }

    #[test]
    fn classification_predicates() {
        assert!(Ipv4::UNSPECIFIED.is_unspecified());
        assert!(!Ipv4([0, 0, 0, 1]).is_unspecified());
        assert!(Ipv4([127, 0, 0, 1]).is_loopback());
        assert!(Ipv4([127, 255, 255, 254]).is_loopback());
        assert!(!Ipv4([128, 0, 0, 1]).is_loopback());
    }

    #[test]
    fn octet_conversions_round_trip() {
        let addr = Ipv4::new(10, 0, 2, 15);
        assert_eq!(addr.octets(), [10, 0, 2, 15]);
        assert_eq!(Ipv4::from([10, 0, 2, 15]), addr);
        assert_eq!(<[u8; 4]>::from(addr), [10, 0, 2, 15]);
    }
}
