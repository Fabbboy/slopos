//! Address-with-prefix notation and netmask conversion.

use core::fmt;

use crate::addr::Ipv4;

/// An address and its prefix length, as `ip addr` writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cidr {
    pub addr: Ipv4,
    /// `0..=32`. Every constructor here enforces that, so no consumer has to
    /// re-check before shifting by it.
    pub prefix_len: u8,
}

impl Cidr {
    /// `None` if `prefix_len > 32`.
    #[inline]
    pub const fn new(addr: Ipv4, prefix_len: u8) -> Option<Cidr> {
        if prefix_len > 32 {
            None
        } else {
            Some(Cidr { addr, prefix_len })
        }
    }

    /// Parses `10.0.2.15/24`.
    ///
    /// A bare address parses as `/32`, with no classful inference: reading
    /// `10.0.2.15` as `/8` fails invisibly, the command succeeding while the
    /// routing table is subtly not what was asked for.
    ///
    /// Rejected: an empty address, an empty prefix (`10.0.2.15/`), a prefix
    /// above 32, and anything the [`Ipv4`] parser rejects.
    pub fn from_str_bytes(text: &[u8]) -> Option<Cidr> {
        let (addr_part, prefix_part) = match text.iter().position(|&b| b == b'/') {
            Some(slash) => (&text[..slash], Some(&text[slash + 1..])),
            None => (text, None),
        };

        let addr = Ipv4::from_str_bytes(addr_part)?;
        let prefix_len = match prefix_part {
            None => 32,
            Some(digits) => {
                if digits.is_empty() {
                    return None;
                }
                let mut val: u8 = 0;
                for &b in digits {
                    if !b.is_ascii_digit() {
                        return None;
                    }
                    val = val.checked_mul(10)?.checked_add(b - b'0')?;
                    if val > 32 {
                        return None;
                    }
                }
                val
            }
        };
        Some(Cidr { addr, prefix_len })
    }

    /// The netmask this prefix length denotes.
    #[inline]
    pub const fn mask(self) -> [u8; 4] {
        prefix_len_to_mask(self.prefix_len)
    }

    /// The address with every host bit cleared.
    pub const fn network(self) -> Ipv4 {
        let a = self.addr.0;
        let m = self.mask();
        Ipv4([a[0] & m[0], a[1] & m[1], a[2] & m[2], a[3] & m[3]])
    }

    /// The address with every host bit set. For a `/32` that is the address
    /// itself, and for a `/31` it is the peer.
    pub const fn broadcast(self) -> Ipv4 {
        let a = self.addr.0;
        let m = self.mask();
        Ipv4([a[0] | !m[0], a[1] | !m[1], a[2] | !m[2], a[3] | !m[3]])
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix_len)
    }
}

/// Netmask for a prefix length. Lengths above 32 saturate to `/32` rather than
/// wrapping the shift, so a caller that skipped validation gets the narrowest
/// mask instead of the widest.
pub const fn prefix_len_to_mask(prefix_len: u8) -> [u8; 4] {
    let bits = if prefix_len > 32 { 32 } else { prefix_len };
    let mask: u32 = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    mask.to_be_bytes()
}

/// Prefix length for a netmask, or `None` if the mask has a zero bit above a
/// one bit.
///
/// There are exactly 33 valid masks. `255.255.0.255` counts 24 set bits, so a
/// popcount-based conversion reports `/24` and every later comparison against
/// that prefix silently disagrees with the mask the operator wrote.
pub const fn mask_to_prefix_len(mask: [u8; 4]) -> Option<u8> {
    let value = u32::from_be_bytes(mask);
    let ones = value.leading_ones();
    // Contiguous exactly when the leading run of ones is the whole set of ones.
    if value.count_ones() == ones {
        Some(ones as u8)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::columns::TestBuf;

    #[test]
    fn parse_accepts_prefixes() {
        assert_eq!(
            Cidr::from_str_bytes(b"10.0.2.15/24"),
            Some(Cidr {
                addr: Ipv4([10, 0, 2, 15]),
                prefix_len: 24
            })
        );
        assert_eq!(
            Cidr::from_str_bytes(b"0.0.0.0/0"),
            Some(Cidr {
                addr: Ipv4::UNSPECIFIED,
                prefix_len: 0
            })
        );
        assert_eq!(
            Cidr::from_str_bytes(b"10.0.2.15/32"),
            Some(Cidr {
                addr: Ipv4([10, 0, 2, 15]),
                prefix_len: 32
            })
        );
    }

    #[test]
    fn bare_address_is_a_host_route() {
        assert_eq!(
            Cidr::from_str_bytes(b"10.0.2.15"),
            Some(Cidr {
                addr: Ipv4([10, 0, 2, 15]),
                prefix_len: 32
            })
        );
        // Class A, B and C leading octets all give /32, not /8, /16 or /24.
        assert_eq!(Cidr::from_str_bytes(b"10.1.1.1").unwrap().prefix_len, 32);
        assert_eq!(Cidr::from_str_bytes(b"172.16.1.1").unwrap().prefix_len, 32);
        assert_eq!(Cidr::from_str_bytes(b"192.168.1.1").unwrap().prefix_len, 32);
    }

    #[test]
    fn parse_rejects_malformed() {
        assert_eq!(Cidr::from_str_bytes(b"10.0.2.15/33"), None);
        assert_eq!(Cidr::from_str_bytes(b"10.0.2.15/"), None);
        assert_eq!(Cidr::from_str_bytes(b"/24"), None);
        assert_eq!(Cidr::from_str_bytes(b"10.0.2.256/24"), None);
        assert_eq!(Cidr::from_str_bytes(b""), None);
        assert_eq!(Cidr::from_str_bytes(b"10.0.2.15/24/8"), None);
        assert_eq!(Cidr::from_str_bytes(b"10.0.2.15/2a"), None);
        assert_eq!(Cidr::from_str_bytes(b"10.0.2.15/999"), None);
    }

    #[test]
    fn parse_reads_leading_zeros_as_decimal() {
        assert_eq!(
            Cidr::from_str_bytes(b"010.0.2.15/024"),
            Some(Cidr {
                addr: Ipv4([10, 0, 2, 15]),
                prefix_len: 24
            })
        );
    }

    #[test]
    fn new_rejects_over_long_prefix() {
        assert!(Cidr::new(Ipv4::UNSPECIFIED, 32).is_some());
        assert!(Cidr::new(Ipv4::UNSPECIFIED, 33).is_none());
        assert!(Cidr::new(Ipv4::UNSPECIFIED, 255).is_none());
    }

    #[test]
    fn network_and_broadcast() {
        let net = Cidr::from_str_bytes(b"10.0.2.15/24").unwrap();
        assert_eq!(net.network(), Ipv4([10, 0, 2, 0]));
        assert_eq!(net.broadcast(), Ipv4([10, 0, 2, 255]));

        let host = Cidr::from_str_bytes(b"10.0.2.15/32").unwrap();
        assert_eq!(host.network(), Ipv4([10, 0, 2, 15]));
        assert_eq!(host.broadcast(), Ipv4([10, 0, 2, 15]));

        let all = Cidr::from_str_bytes(b"10.0.2.15/0").unwrap();
        assert_eq!(all.network(), Ipv4::UNSPECIFIED);
        assert_eq!(all.broadcast(), Ipv4([255, 255, 255, 255]));

        let odd = Cidr::from_str_bytes(b"192.168.1.130/26").unwrap();
        assert_eq!(odd.network(), Ipv4([192, 168, 1, 128]));
        assert_eq!(odd.broadcast(), Ipv4([192, 168, 1, 191]));
    }

    #[test]
    fn display_round_trips() {
        for text in [
            "10.0.2.15/24",
            "0.0.0.0/0",
            "255.255.255.255/32",
            "172.16.0.1/12",
        ] {
            let parsed = Cidr::from_str_bytes(text.as_bytes()).unwrap();
            let mut buf = TestBuf::new();
            buf.push_fmt(format_args!("{}", parsed));
            assert_eq!(buf.as_str(), text);
        }
    }

    #[test]
    fn prefix_and_mask_round_trip_over_every_valid_mask() {
        for prefix_len in 0u8..=32 {
            let mask = prefix_len_to_mask(prefix_len);
            assert_eq!(
                mask_to_prefix_len(mask),
                Some(prefix_len),
                "prefix {prefix_len} did not round-trip"
            );
        }
        assert_eq!(prefix_len_to_mask(0), [0, 0, 0, 0]);
        assert_eq!(prefix_len_to_mask(8), [255, 0, 0, 0]);
        assert_eq!(prefix_len_to_mask(24), [255, 255, 255, 0]);
        assert_eq!(prefix_len_to_mask(32), [255, 255, 255, 255]);
    }

    #[test]
    fn prefix_len_to_mask_saturates() {
        assert_eq!(prefix_len_to_mask(33), [255, 255, 255, 255]);
        assert_eq!(prefix_len_to_mask(255), [255, 255, 255, 255]);
    }

    #[test]
    fn mask_to_prefix_len_rejects_non_contiguous() {
        assert_eq!(mask_to_prefix_len([255, 255, 0, 255]), None);
        assert_eq!(mask_to_prefix_len([255, 0, 255, 0]), None);
        assert_eq!(mask_to_prefix_len([0, 255, 255, 255]), None);
        assert_eq!(mask_to_prefix_len([255, 255, 255, 1]), None);
        // Same popcount as /24, which a popcount implementation gets wrong.
        assert_eq!(mask_to_prefix_len([255, 255, 0, 255]), None);
        assert_eq!(mask_to_prefix_len([255, 255, 255, 0]), Some(24));
    }

    /// Exhaustive over the whole 32-bit mask space would be four billion
    /// checks; the octet-granular sweep still catches a popcount-based
    /// implementation.
    #[test]
    fn mask_to_prefix_len_accepts_only_the_33() {
        let mut accepted = 0usize;
        for a in [0u8, 128, 192, 224, 240, 248, 252, 254, 255] {
            for b in [0u8, 128, 192, 224, 240, 248, 252, 254, 255] {
                for c in [0u8, 128, 192, 224, 240, 248, 252, 254, 255] {
                    for d in [0u8, 128, 192, 224, 240, 248, 252, 254, 255] {
                        if mask_to_prefix_len([a, b, c, d]).is_some() {
                            accepted += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(accepted, 33);
    }
}
