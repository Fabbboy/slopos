//! Argument-token resolution shared by the network CLIs.

/// Why a token did not name exactly one table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    /// No entry begins with the token.
    Unknown,
    /// More than one entry begins with the token and none equals it.
    Ambiguous,
}

/// Resolves an abbreviated token against a table of full names.
///
/// An exact match wins outright, even when the token is also a prefix of other
/// entries: `addr` against `["addr", "addrlabel"]` is `addr`, not ambiguous.
/// Without that rule a table can never grow a longer name beside a shorter
/// one, because doing so would break every existing invocation of the shorter.
///
/// Otherwise a token that prefixes exactly one entry resolves to it, and a
/// token that prefixes several is [`TokenError::Ambiguous`] — never silently
/// the first, which is the failure mode where a new table entry quietly
/// changes what an existing script does.
///
/// An empty token is [`TokenError::Unknown`] rather than ambiguous: it
/// prefixes everything, but it is a missing argument rather than an
/// under-specified one, and reporting it as ambiguous would print the entire
/// table at someone who typed nothing.
pub fn resolve_token(input: &[u8], table: &[&'static str]) -> Result<&'static str, TokenError> {
    if input.is_empty() {
        return Err(TokenError::Unknown);
    }
    for &candidate in table {
        if candidate.as_bytes() == input {
            return Ok(candidate);
        }
    }
    let mut found: Option<&'static str> = None;
    for &candidate in table {
        if candidate.as_bytes().starts_with(input) {
            if found.is_some() {
                return Err(TokenError::Ambiguous);
            }
            found = Some(candidate);
        }
    }
    found.ok_or(TokenError::Unknown)
}

/// The entries a token prefixes, in table order — what an ambiguity message
/// lists so the reader can pick one.
pub fn matches<'t>(
    input: &'t [u8],
    table: &'t [&'static str],
) -> impl Iterator<Item = &'static str> + 't {
    table
        .iter()
        .copied()
        .filter(move |c| c.as_bytes().starts_with(input))
}

/// Reads the flag bytes of a bundled short-option argument, returning the OR
/// of every matched bit.
///
/// `accept` maps a flag byte to the bit it sets, so `-tuln` and `-t -u -l -n`
/// produce the same value and neither the caller nor the parser has to care
/// which form was typed. A single leading `-` is skipped when present, so an
/// argument may be passed whole.
///
/// A bare `-` carries no flag bytes and yields `Ok(0)`: it is the conventional
/// stdin/stdout placeholder, and rejecting it here would push a special case
/// into every caller.
///
/// The first byte with no entry in `accept` stops the scan and is returned as
/// `Err`, so the caller can name the offending flag rather than the whole
/// argument. Bits set by earlier bytes of a rejected argument are discarded,
/// because a partially applied option bundle is worse than none.
pub fn scan_bundled(arg: &[u8], accept: &[(u8, u32)]) -> Result<u32, u8> {
    let flags = match arg.split_first() {
        Some((b'-', rest)) => rest,
        _ => arg,
    };
    let mut bits = 0u32;
    for &byte in flags {
        match accept.iter().find(|(flag, _)| *flag == byte) {
            Some((_, bit)) => bits |= bit,
            None => return Err(byte),
        }
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBJECTS: [&str; 4] = ["addr", "dhcp", "dns", "link"];
    const COMMANDS: [&str; 3] = ["help", "set", "show"];

    #[test]
    fn exact_match_beats_prefix() {
        assert_eq!(resolve_token(b"addr", &["addr", "addrlabel"]), Ok("addr"));
        // Order in the table must not matter.
        assert_eq!(resolve_token(b"addr", &["addrlabel", "addr"]), Ok("addr"));
    }

    #[test]
    fn unique_prefix_resolves() {
        assert_eq!(resolve_token(b"li", &OBJECTS), Ok("link"));
        assert_eq!(resolve_token(b"l", &OBJECTS), Ok("link"));
        assert_eq!(resolve_token(b"a", &OBJECTS), Ok("addr"));
        assert_eq!(resolve_token(b"sh", &COMMANDS), Ok("show"));
        assert_eq!(resolve_token(b"se", &COMMANDS), Ok("set"));
    }

    #[test]
    fn ambiguous_prefix_is_an_error() {
        assert_eq!(resolve_token(b"d", &OBJECTS), Err(TokenError::Ambiguous));
        assert_eq!(resolve_token(b"s", &COMMANDS), Err(TokenError::Ambiguous));
    }

    #[test]
    fn unknown_token_is_an_error() {
        assert_eq!(resolve_token(b"zebra", &OBJECTS), Err(TokenError::Unknown));
        assert_eq!(resolve_token(b"x", &OBJECTS), Err(TokenError::Unknown));
        // Longer than any entry, and a prefix of none.
        assert_eq!(
            resolve_token(b"linkage", &OBJECTS),
            Err(TokenError::Unknown)
        );
    }

    #[test]
    fn empty_token_is_unknown_not_ambiguous() {
        assert_eq!(resolve_token(b"", &OBJECTS), Err(TokenError::Unknown));
        assert_eq!(resolve_token(b"", &[]), Err(TokenError::Unknown));
    }

    #[test]
    fn empty_table_resolves_nothing() {
        assert_eq!(resolve_token(b"link", &[]), Err(TokenError::Unknown));
    }

    #[test]
    fn matches_lists_the_candidates() {
        let found: [&str; 2] = {
            let mut it = matches(b"d", &OBJECTS);
            [it.next().unwrap(), it.next().unwrap()]
        };
        assert_eq!(found, ["dhcp", "dns"]);
        assert_eq!(matches(b"d", &OBJECTS).count(), 2);
        assert_eq!(matches(b"z", &OBJECTS).count(), 0);
        assert_eq!(matches(b"", &OBJECTS).count(), 4);
    }

    const NC_FLAGS: [(u8, u32); 4] = [(b't', 1), (b'u', 2), (b'l', 4), (b'n', 8)];

    #[test]
    fn bundled_flags_set_every_bit() {
        assert_eq!(scan_bundled(b"-tuln", &NC_FLAGS), Ok(1 | 2 | 4 | 8));
    }

    #[test]
    fn bundled_and_separate_flags_agree() {
        let separate =
            scan_bundled(b"-t", &NC_FLAGS).unwrap() | scan_bundled(b"-u", &NC_FLAGS).unwrap();
        assert_eq!(scan_bundled(b"-tu", &NC_FLAGS), Ok(separate));
    }

    #[test]
    fn unknown_flag_names_the_byte() {
        assert_eq!(scan_bundled(b"-tx", &NC_FLAGS), Err(b'x'));
        assert_eq!(scan_bundled(b"-x", &NC_FLAGS), Err(b'x'));
        // The first bad byte wins, not the last.
        assert_eq!(scan_bundled(b"-txy", &NC_FLAGS), Err(b'x'));
    }

    #[test]
    fn repeated_flags_are_idempotent() {
        assert_eq!(scan_bundled(b"-tttt", &NC_FLAGS), Ok(1));
    }

    /// Pinned: a bare `-` is the stdin placeholder and sets nothing.
    #[test]
    fn bare_dash_sets_no_bits() {
        assert_eq!(scan_bundled(b"-", &NC_FLAGS), Ok(0));
        assert_eq!(scan_bundled(b"", &NC_FLAGS), Ok(0));
    }

    #[test]
    fn leading_dash_is_optional() {
        assert_eq!(scan_bundled(b"tuln", &NC_FLAGS), Ok(1 | 2 | 4 | 8));
        // Only one dash is skipped, so `--t` reports the second dash.
        assert_eq!(scan_bundled(b"--t", &NC_FLAGS), Err(b'-'));
    }
}
