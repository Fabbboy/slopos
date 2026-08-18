//! Fixed-width column layout for `-br` output.

use core::fmt::Write;

/// Column width for an interface name in brief mode.
pub const BRIEF_NAME: usize = 16;
/// Column width for an operational state in brief mode.
pub const BRIEF_STATE: usize = 15;
/// Column width for a hardware address in brief mode.
pub const BRIEF_MAC: usize = 18;

/// Writes `text` padded to `width`, followed by at least one space.
///
/// The `max(1, ...)` is what keeps a row parseable: a plain `width - len` pad
/// emits nothing for a field that exactly fills its column, and the next field
/// runs straight into it (`eth0LOWERLAYERDOWN`).
pub fn field<W: Write + ?Sized>(out: &mut W, text: &str, width: usize) -> core::fmt::Result {
    out.write_str(text)?;
    let pad = width.saturating_sub(text.chars().count()).max(1);
    for _ in 0..pad {
        out.write_char(' ')?;
    }
    Ok(())
}

/// A fixed-capacity [`Write`] sink, so the tests in this crate can assert on
/// rendered output without `alloc`.
#[cfg(test)]
pub(crate) struct TestBuf {
    bytes: [u8; 512],
    len: usize,
}

#[cfg(test)]
impl TestBuf {
    pub(crate) fn new() -> Self {
        Self {
            bytes: [0; 512],
            len: 0,
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).expect("rendered output is UTF-8")
    }

    pub(crate) fn push_fmt(&mut self, args: core::fmt::Arguments<'_>) {
        self.write_fmt(args).expect("TestBuf overflow");
    }
}

#[cfg(test)]
impl Write for TestBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.len + s.len();
        if end > self.bytes.len() {
            return Err(core::fmt::Error);
        }
        self.bytes[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(text: &str, width: usize) -> TestBuf {
        let mut buf = TestBuf::new();
        field(&mut buf, text, width).unwrap();
        buf
    }

    #[test]
    fn short_field_pads_to_width() {
        assert_eq!(rendered("eth0", 8).as_str(), "eth0    ");
        assert_eq!(rendered("UP", BRIEF_STATE).as_str(), "UP             ");
        assert_eq!(rendered("", 4).as_str(), "    ");
    }

    #[test]
    fn field_exactly_at_width_still_emits_one_separator() {
        assert_eq!(rendered("abcdefgh", 8).as_str(), "abcdefgh ");
        assert_eq!(
            rendered("LOWERLAYERDOWN", BRIEF_STATE).as_str(),
            "LOWERLAYERDOWN "
        );
    }

    #[test]
    fn over_width_field_emits_exactly_one_space() {
        assert_eq!(rendered("abcdefghij", 8).as_str(), "abcdefghij ");
        assert_eq!(
            rendered("a-very-long-interface-name", BRIEF_NAME).as_str(),
            "a-very-long-interface-name "
        );
    }

    #[test]
    fn width_zero_still_separates() {
        assert_eq!(rendered("x", 0).as_str(), "x ");
    }

    #[test]
    fn padding_counts_characters_not_bytes() {
        // A Latin-1 character is two UTF-8 bytes; the column counts characters.
        assert_eq!(rendered("é", 4).as_str(), "é   ");
    }

    #[test]
    fn brief_row_columns_line_up() {
        let mut buf = TestBuf::new();
        field(&mut buf, "eth0", BRIEF_NAME).unwrap();
        field(&mut buf, "UP", BRIEF_STATE).unwrap();
        field(&mut buf, "52:54:00:12:34:ab", BRIEF_MAC).unwrap();
        assert_eq!(
            buf.as_str(),
            "eth0            UP             52:54:00:12:34:ab "
        );
    }
}
