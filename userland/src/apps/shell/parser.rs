//! Command line parsing and path normalization.

use super::buffers;

/// Words one command may take.  The token arena itself is unbounded; this only
/// sizes the fixed `argv` index array each parsed command carries.
pub const SHELL_MAX_ARGS: usize = 64;

#[inline(always)]
pub fn is_space(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
}

/// Trim a `#` comment from `line`.
///
/// `#` only starts a comment at the beginning of a word and outside quotes.
/// Run before expansion, so a variable's value can never be spliced out of a
/// comment and back into the command.
pub fn strip_comment(line: &[u8]) -> &[u8] {
    let mut in_single = false;
    let mut in_double = false;
    let mut at_word_start = true;

    for (i, &c) in line.iter().enumerate() {
        match c {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'#' if at_word_start && !in_single && !in_double => return &line[..i],
            _ => {}
        }
        at_word_start = is_space(c);
    }
    line
}

pub fn normalize_path(input: &[u8], buffer: &mut [u8]) -> i32 {
    let cwd = super::cwd_bytes();
    normalize_path_with_cwd(input, buffer, &cwd)
}

fn collapse_absolute_path(buffer: &mut [u8], len: usize) -> usize {
    if buffer.is_empty() {
        return 0;
    }
    if len == 0 || buffer[0] != b'/' {
        buffer[0] = b'/';
        return 1;
    }

    let mut write = 1usize;
    let mut read = 1usize;

    while read < len {
        while read < len && buffer[read] == b'/' {
            read += 1;
        }
        if read >= len {
            break;
        }

        let seg_start = read;
        while read < len && buffer[read] != b'/' {
            read += 1;
        }
        let seg_len = read - seg_start;

        if seg_len == 1 && buffer[seg_start] == b'.' {
            continue;
        }
        if seg_len == 2 && buffer[seg_start] == b'.' && buffer[seg_start + 1] == b'.' {
            if write > 1 {
                write -= 1;
                while write > 0 && buffer[write] != b'/' {
                    write -= 1;
                }
                if write == 0 {
                    write = 1;
                }
            }
            continue;
        }

        if write > 1 {
            buffer[write] = b'/';
            write += 1;
        }
        for j in 0..seg_len {
            buffer[write + j] = buffer[seg_start + j];
        }
        write += seg_len;
    }

    if write == 0 { 1 } else { write }
}

pub fn normalize_path_with_cwd(input: &[u8], buffer: &mut [u8], cwd: &[u8]) -> i32 {
    if buffer.is_empty() {
        return -1;
    }
    if input.is_empty() {
        buffer[0] = b'/';
        if buffer.len() > 1 {
            buffer[1] = 0;
        }
        return 0;
    }

    if input[0] == b'/' {
        let len = input.len().min(buffer.len().saturating_sub(1));
        if len >= buffer.len() {
            return -1;
        }
        buffer[..len].copy_from_slice(&input[..len]);
        let collapsed_len = collapse_absolute_path(buffer, len);
        buffer[collapsed_len] = 0;
        return 0;
    }

    let cwd_len = cwd.iter().position(|&b| b == 0).unwrap_or(cwd.len());
    let input_len = input.len().min(buffer.len());

    let needs_sep = cwd_len > 0 && cwd[cwd_len - 1] != b'/';
    let sep_len = if needs_sep { 1 } else { 0 };
    let total = cwd_len + sep_len + input_len;

    if total >= buffer.len() {
        return -1;
    }

    buffer[..cwd_len].copy_from_slice(&cwd[..cwd_len]);
    if needs_sep {
        buffer[cwd_len] = b'/';
    }
    buffer[cwd_len + sep_len..cwd_len + sep_len + input_len].copy_from_slice(&input[..input_len]);
    let collapsed_len = collapse_absolute_path(buffer, total);
    buffer[collapsed_len] = 0;
    0
}

fn is_var_char(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

fn format_int_into(dst: &mut [u8], val: impl core::fmt::Display) -> usize {
    use std::io::Write;
    let len = dst.len();
    let mut cursor: &mut [u8] = dst;
    let _ = write!(cursor, "{val}");
    len - cursor.len()
}

fn emit(dst: &mut [u8], pos: &mut usize, b: u8) {
    if *pos < dst.len() - 1 {
        dst[*pos] = b;
        *pos += 1;
    }
}

fn emit_slice(dst: &mut [u8], pos: &mut usize, src: &[u8], src_len: usize) {
    let avail = dst.len().saturating_sub(*pos + 1);
    let n = src_len.min(avail);
    dst[*pos..*pos + n].copy_from_slice(&src[..n]);
    *pos += n;
}

pub fn expand_variables(input: &[u8], input_len: usize, output: &mut [u8]) -> usize {
    let mut out = 0usize;
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    while i < input_len && input[i] != 0 {
        let c = input[i];

        if c == b'\'' && !in_double {
            in_single = !in_single;
            emit(output, &mut out, c);
            i += 1;
            continue;
        }
        if c == b'"' && !in_single {
            in_double = !in_double;
            emit(output, &mut out, c);
            i += 1;
            continue;
        }

        if in_single {
            emit(output, &mut out, c);
            i += 1;
            continue;
        }

        if c == b'\\' && i + 1 < input_len {
            let next = input[i + 1];
            if next == b'$' {
                emit(output, &mut out, b'$');
                i += 2;
                continue;
            }
            if in_double && (next == b'"' || next == b'\\') {
                emit(output, &mut out, next);
                i += 2;
                continue;
            }
        }

        if c == b'$' && i + 1 < input_len {
            let next = input[i + 1];

            if next == b'?' {
                let val = super::last_exit_code();
                out += format_int_into(&mut output[out..], val);
                i += 2;
                continue;
            }
            if next == b'$' {
                let val = super::shell_pid();
                out += format_int_into(&mut output[out..], val);
                i += 2;
                continue;
            }
            if next == b'!' {
                let val = super::last_bg_pid();
                out += format_int_into(&mut output[out..], val);
                i += 2;
                continue;
            }
            if next == b'#' {
                let val = super::args::positional_count() as i32;
                out += format_int_into(&mut output[out..], val);
                i += 2;
                continue;
            }
            // Positional parameters are not environment variables, so they
            // resolve here rather than through the name lookup below.
            if next.is_ascii_digit() {
                let idx = (next - b'0') as usize;
                if let Some(value) = super::args::positional(idx) {
                    let len = value.len();
                    emit_slice(output, &mut out, value.as_slice(), len);
                }
                i += 2;
                continue;
            }
            if next == b'{' {
                i += 2;
                let var_start = i;
                while i < input_len && input[i] != b'}' && input[i] != 0 {
                    i += 1;
                }
                let var_name = &input[var_start..i];
                if i < input_len && input[i] == b'}' {
                    i += 1;
                }
                if let Some((val, val_len)) = super::env::get(var_name) {
                    emit_slice(output, &mut out, &val, val_len);
                }
                continue;
            }
            if is_var_char(next) {
                i += 1;
                let var_start = i;
                while i < input_len && is_var_char(input[i]) {
                    i += 1;
                }
                let var_name = &input[var_start..i];
                if let Some((val, val_len)) = super::env::get(var_name) {
                    emit_slice(output, &mut out, &val, val_len);
                }
                continue;
            }
        }

        emit(output, &mut out, c);
        i += 1;
    }

    if out < output.len() {
        output[out] = 0;
    }
    out
}

fn is_operator(b: u8) -> bool {
    b == b'|' || b == b'<' || b == b'>' || b == b'&' || b == b';'
}

/// Length of the redirection operator at the head of `s`, if there is one.
///
/// POSIX IO_NUMBER: leading digits count as the redirected descriptor only when
/// they touch the `<` or `>`, so `echo 2 > out` passes `2` as an argument.
fn scan_redirect(s: &[u8]) -> Option<usize> {
    let mut i = 0usize;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let arrow = *s.get(i)?;
    if arrow != b'<' && arrow != b'>' {
        return None;
    }
    Some(match s.get(i + 1) {
        // `>>` appends; `>&`/`<&` name another descriptor instead of a path.
        Some(b'>') if arrow == b'>' => i + 2,
        Some(b'&') => i + 2,
        _ => i + 1,
    })
}

pub fn shell_parse_line(line: &[u8], tokens: &mut buffers::ParsedTokens) -> i32 {
    if line.is_empty() {
        return 0;
    }
    let mut cursor = 0usize;
    let mut tok: Vec<u8> = Vec::new();

    while cursor < line.len() {
        while cursor < line.len() && is_space(line[cursor]) {
            cursor += 1;
        }
        if cursor >= line.len() || line[cursor] == 0 {
            break;
        }

        // Sitting after the whitespace skip and before the operator and word
        // branches gives `#` the POSIX word-boundary rule without a special
        // case.
        if line[cursor] == b'#' {
            break;
        }

        // Longest match first, so `&&` is never read as two background
        // operators nor `||` as two pipes.
        let rest = &line[cursor..];
        let control: Option<usize> = match (rest[0], rest.get(1)) {
            (b'&', Some(b'&')) | (b'|', Some(b'|')) => Some(2),
            (b'&', Some(b'>')) => None, // a redirection, handled below
            (b';' | b'|' | b'&', _) => Some(1),
            _ => None,
        };
        if let Some(n) = control {
            tokens.push_token(&rest[..n]);
            cursor += n;
            continue;
        }

        let redirect = if rest[0] == b'&' && rest.get(1) == Some(&b'>') {
            Some(2)
        } else {
            scan_redirect(rest)
        };
        if let Some(n) = redirect {
            tokens.push_token(&rest[..n]);
            cursor += n;
            continue;
        }

        tok.clear();
        let mut in_single = false;
        let mut in_double = false;
        let mut quoted = false;

        while cursor < line.len() && line[cursor] != 0 {
            let c = line[cursor];

            if c == b'\'' && !in_double {
                in_single = !in_single;
                quoted = true;
                cursor += 1;
                continue;
            }
            if c == b'"' && !in_single {
                in_double = !in_double;
                quoted = true;
                cursor += 1;
                continue;
            }

            if in_single || in_double {
                tok.push(c);
                cursor += 1;
                continue;
            }

            if is_space(c) || is_operator(c) {
                break;
            }

            if c == b'\\' && cursor + 1 < line.len() {
                cursor += 1;
                tok.push(line[cursor]);
                quoted = true;
                cursor += 1;
                continue;
            }

            tok.push(c);
            cursor += 1;
        }

        // An explicitly quoted empty word is a real, empty argument.
        if !tok.is_empty() || quoted {
            tokens.push_token(&tok);
        }
    }

    tokens.count() as i32
}
