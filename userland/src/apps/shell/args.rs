//! How the shell was invoked.
//!
//! `sh`, `sh -c STRING`, `sh FILE [args…]` and `sh -s [args…]` each decide
//! where commands come from and what `$0`/`$1` mean.  Parsed once at startup.

use std::sync::Mutex;

/// Where this shell reads its commands from.
pub enum Source {
    /// Standard input — a terminal (interactive) or a script on a pipe/file.
    Stdin,
    /// `-c STRING`: run the string and exit.
    CommandString(Vec<u8>),
    /// A script file operand.
    File(Vec<u8>),
}

pub struct Invocation {
    pub source: Source,
    /// `-i`: force interactive even without a terminal.
    pub force_interactive: bool,
}

pub struct UsageError(pub &'static str);

/// `$0` followed by `$1`..`$9`.
static POSITIONAL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// Parse an argument vector into an [`Invocation`].
///
/// Tolerates a completely empty `argv`: the terminal emulator spawns the shell
/// with no arguments at all, not even a program name.
pub fn parse(argv: &[&str]) -> Result<Invocation, UsageError> {
    let name = argv.first().copied().unwrap_or("sh");
    let mut rest = argv.iter().skip(1);

    let mut force_interactive = false;
    let mut command: Option<Vec<u8>> = None;
    let mut read_stdin = false;
    let mut operands: Vec<Vec<u8>> = Vec::new();

    while let Some(arg) = rest.next() {
        match *arg {
            "--" => break,
            "-" | "-s" => {
                read_stdin = true;
                break;
            }
            "-i" => force_interactive = true,
            "-c" => match rest.next() {
                Some(text) => {
                    command = Some(text.as_bytes().to_vec());
                    break;
                }
                None => return Err(UsageError("-c requires an argument")),
            },
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(UsageError("unknown option"));
            }
            other => {
                operands.push(other.as_bytes().to_vec());
                break;
            }
        }
    }

    operands.extend(rest.map(|a| a.as_bytes().to_vec()));

    let source = if let Some(text) = command {
        // POSIX: under `-c` the first remaining operand renames `$0`, the rest
        // are `$1`...
        let mut positional = vec![name.as_bytes().to_vec()];
        if !operands.is_empty() {
            positional[0] = operands.remove(0);
        }
        positional.extend(operands);
        set_positional(positional);
        Source::CommandString(text)
    } else if !read_stdin && !operands.is_empty() {
        let path = operands.remove(0);
        let mut positional = vec![path.clone()];
        positional.extend(operands);
        set_positional(positional);
        Source::File(path)
    } else {
        let mut positional = vec![name.as_bytes().to_vec()];
        positional.extend(operands);
        set_positional(positional);
        Source::Stdin
    };

    Ok(Invocation {
        source,
        force_interactive,
    })
}

fn set_positional(values: Vec<Vec<u8>>) {
    *POSITIONAL.lock().unwrap() = values;
}

/// `$0` for index 0, `$1`.. for the operands.  `None` when unset, which expands
/// to nothing.
pub fn positional(idx: usize) -> Option<Vec<u8>> {
    POSITIONAL.lock().unwrap().get(idx).cloned()
}

/// `$#` — operands only, so `$0` is not counted.
pub fn positional_count() -> usize {
    POSITIONAL.lock().unwrap().len().saturating_sub(1)
}
