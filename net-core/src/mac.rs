//! Ethernet hardware addresses.

use core::fmt;

/// A six-octet Ethernet address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(transparent)]
pub struct Mac(pub [u8; 6]);

impl Mac {
    /// All zeroes — what the ABI reports for an interface with no hardware
    /// address and for a neighbour entry still `INCOMPLETE`.
    pub const ZERO: Mac = Mac([0; 6]);

    #[inline]
    pub const fn octets(self) -> [u8; 6] {
        self.0
    }

    #[inline]
    pub const fn is_zero(self) -> bool {
        let o = self.0;
        o[0] == 0 && o[1] == 0 && o[2] == 0 && o[3] == 0 && o[4] == 0 && o[5] == 0
    }

    /// Parses `aa:bb:cc:dd:ee:ff`.
    ///
    /// Exactly six groups of one or two hex digits, separated by `:` or `-`;
    /// case is ignored and the two separators may be mixed. A separator is
    /// required: a bare twelve-hex run is not accepted, so a mistyped address
    /// cannot silently reinterpret its digits.
    pub fn from_str_bytes(text: &[u8]) -> Option<Mac> {
        let mut out = [0u8; 6];
        let mut iter = text.split(|&b| b == b':' || b == b'-');
        for slot in out.iter_mut() {
            let piece = iter.next()?;
            if piece.is_empty() || piece.len() > 2 {
                return None;
            }
            let mut val: u16 = 0;
            for &b in piece {
                let digit = hex_digit(b)?;
                val = (val << 4) | u16::from(digit);
            }
            *slot = val as u8;
        }
        if iter.next().is_some() {
            return None;
        }
        Some(Mac(out))
    }
}

const fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

impl From<[u8; 6]> for Mac {
    #[inline]
    fn from(octets: [u8; 6]) -> Self {
        Self(octets)
    }
}

impl From<Mac> for [u8; 6] {
    #[inline]
    fn from(mac: Mac) -> Self {
        mac.0
    }
}

/// Always lowercase, always colon-separated, always two digits per octet: one
/// canonical spelling, so two rendered addresses compare as strings.
impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let o = &self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            o[0], o[1], o[2], o[3], o[4], o[5]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::columns::TestBuf;

    const SAMPLE: Mac = Mac([0x52, 0x54, 0x00, 0x12, 0x34, 0xab]);

    #[test]
    fn parse_accepts_colons() {
        assert_eq!(Mac::from_str_bytes(b"52:54:00:12:34:ab"), Some(SAMPLE));
    }

    #[test]
    fn parse_accepts_dashes_and_mixed_separators() {
        assert_eq!(Mac::from_str_bytes(b"52-54-00-12-34-ab"), Some(SAMPLE));
        assert_eq!(Mac::from_str_bytes(b"52:54-00:12-34:ab"), Some(SAMPLE));
    }

    #[test]
    fn parse_ignores_case() {
        assert_eq!(Mac::from_str_bytes(b"52:54:00:12:34:AB"), Some(SAMPLE));
        assert_eq!(Mac::from_str_bytes(b"52:54:00:12:34:Ab"), Some(SAMPLE));
    }

    #[test]
    fn parse_accepts_single_digit_groups() {
        assert_eq!(
            Mac::from_str_bytes(b"2:4:0:12:34:ab"),
            Some(Mac([0x02, 0x04, 0x00, 0x12, 0x34, 0xab]))
        );
    }

    #[test]
    fn parse_rejects_wrong_length() {
        assert_eq!(Mac::from_str_bytes(b""), None);
        assert_eq!(Mac::from_str_bytes(b"52:54:00:12:34"), None);
        assert_eq!(Mac::from_str_bytes(b"52:54:00:12:34:ab:cd"), None);
        assert_eq!(Mac::from_str_bytes(b"525:54:00:12:34:ab"), None);
        assert_eq!(Mac::from_str_bytes(b"52::00:12:34:ab"), None);
        // A separator is mandatory; a bare hex run is not an address here.
        assert_eq!(Mac::from_str_bytes(b"5254001234ab"), None);
    }

    #[test]
    fn parse_rejects_non_hex() {
        assert_eq!(Mac::from_str_bytes(b"52:54:00:12:34:gg"), None);
        assert_eq!(Mac::from_str_bytes(b"52:54:00:12:34:a "), None);
        assert_eq!(Mac::from_str_bytes(b"0x:54:00:12:34:ab"), None);
    }

    #[test]
    fn display_is_lowercase_colon_hex() {
        let mut buf = TestBuf::new();
        buf.push_fmt(format_args!("{}", SAMPLE));
        assert_eq!(buf.as_str(), "52:54:00:12:34:ab");

        let mut buf = TestBuf::new();
        buf.push_fmt(format_args!(
            "{}",
            Mac::from_str_bytes(b"AA-BB-CC-DD-EE-FF").unwrap()
        ));
        assert_eq!(buf.as_str(), "aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn zero_predicate() {
        assert!(Mac::ZERO.is_zero());
        assert!(Mac::default().is_zero());
        assert!(!SAMPLE.is_zero());
        assert!(!Mac([0, 0, 0, 0, 0, 1]).is_zero());
    }

    #[test]
    fn octet_conversions_round_trip() {
        assert_eq!(Mac::from([0x52, 0x54, 0x00, 0x12, 0x34, 0xab]), SAMPLE);
        assert_eq!(
            <[u8; 6]>::from(SAMPLE),
            [0x52, 0x54, 0x00, 0x12, 0x34, 0xab]
        );
    }
}
