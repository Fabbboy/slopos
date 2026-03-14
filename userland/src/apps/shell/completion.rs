use std::fs;

use crate::program_registry;

use super::builtins::BUILTINS;
use super::parser::is_space;

pub struct CompletionResult {
    pub insertion: [u8; 128],
    pub insertion_len: usize,
    pub show_matches: bool,
    pub matches_buf: [u8; 512],
    pub matches_len: usize,
}

impl CompletionResult {
    fn empty() -> Self {
        Self {
            insertion: [0; 128],
            insertion_len: 0,
            show_matches: false,
            matches_buf: [0; 512],
            matches_len: 0,
        }
    }
}

pub fn try_complete(input: &[u8], len: usize, cursor_pos: usize, cwd: &[u8]) -> CompletionResult {
    let mut result = CompletionResult::empty();
    let effective_pos = cursor_pos.min(len);

    let mut word_start = effective_pos;
    while word_start > 0 && !is_space(input[word_start - 1]) {
        word_start -= 1;
    }

    let prefix = &input[word_start..effective_pos];
    let prefix_len = effective_pos - word_start;

    if prefix_len == 0 {
        return result;
    }

    let is_first_token = {
        let mut all_space = true;
        for i in 0..word_start {
            if !is_space(input[i]) {
                all_space = false;
                break;
            }
        }
        all_space
    };

    if is_first_token {
        complete_command(prefix, prefix_len, &mut result);
    } else {
        let dirs_only = command_wants_dirs_only(input, word_start);
        complete_path(prefix, prefix_len, cwd, dirs_only, &mut result);
    }

    result
}

fn complete_command(prefix: &[u8], prefix_len: usize, result: &mut CompletionResult) {
    let mut matches: [&[u8]; 64] = [&[]; 64];
    let mut match_count = 0;

    for entry in BUILTINS {
        let name = entry.name.as_bytes();
        if name.len() >= prefix_len && &name[..prefix_len] == prefix {
            push_command_match(name, &mut matches, &mut match_count);
        }
    }

    for spec in program_registry::user_programs() {
        let name = spec.name.as_bytes();
        if name.len() >= prefix_len && &name[..prefix_len] == prefix {
            push_command_match(name, &mut matches, &mut match_count);
        }
    }

    if match_count == 0 {
        return;
    }

    if match_count == 1 {
        let name = matches[0];
        let remaining = name.len() - prefix_len;
        let insert_len = remaining + 1;
        if insert_len <= result.insertion.len() {
            result.insertion[..remaining].copy_from_slice(&name[prefix_len..]);
            result.insertion[remaining] = b' ';
            result.insertion_len = insert_len;
        }
    } else {
        let first_name = matches[0];
        let mut common_len = first_name.len();
        for i in 1..match_count {
            let name = matches[i];
            let mut j = prefix_len;
            while j < common_len && j < name.len() && first_name[j] == name[j] {
                j += 1;
            }
            common_len = j;
        }

        if common_len > prefix_len {
            let remaining = common_len - prefix_len;
            result.insertion[..remaining].copy_from_slice(&first_name[prefix_len..common_len]);
            result.insertion_len = remaining;
        }

        result.show_matches = true;
        let mut pos = 0;
        for i in 0..match_count {
            let name = matches[i];
            if pos + name.len() + 2 < result.matches_buf.len() {
                result.matches_buf[pos..pos + name.len()].copy_from_slice(name);
                pos += name.len();
                result.matches_buf[pos] = b' ';
                pos += 1;
                result.matches_buf[pos] = b' ';
                pos += 1;
            }
        }
        if pos > 0 {
            pos -= 2;
        }
        result.matches_len = pos;
    }
}

fn push_command_match(name: &'static [u8], matches: &mut [&'static [u8]; 64], count: &mut usize) {
    if *count >= matches.len() {
        return;
    }

    let mut i = 0usize;
    while i < *count {
        if matches[i] == name {
            return;
        }
        i += 1;
    }

    matches[*count] = name;
    *count += 1;
}

fn command_wants_dirs_only(input: &[u8], word_start: usize) -> bool {
    let mut cmd_start = 0;
    while cmd_start < word_start && is_space(input[cmd_start]) {
        cmd_start += 1;
    }
    let mut cmd_end = cmd_start;
    while cmd_end < word_start && !is_space(input[cmd_end]) {
        cmd_end += 1;
    }
    let cmd = &input[cmd_start..cmd_end];
    cmd == b"cd" || cmd == b"mkdir"
}

fn complete_path(
    prefix: &[u8],
    prefix_len: usize,
    cwd: &[u8],
    dirs_only: bool,
    result: &mut CompletionResult,
) {
    let mut last_slash = None;
    for i in 0..prefix_len {
        if prefix[i] == b'/' {
            last_slash = Some(i);
        }
    }

    let (file_prefix, file_prefix_len) = if let Some(slash_pos) = last_slash {
        (&prefix[slash_pos + 1..], prefix_len - slash_pos - 1)
    } else {
        (prefix, prefix_len)
    };

    let mut dir_buf = [0u8; 256];
    let dir_len = build_dir_path(prefix, prefix_len, last_slash, cwd, &mut dir_buf);
    if dir_len == 0 {
        return;
    }

    let dir_str = match core::str::from_utf8(&dir_buf[..dir_len]) {
        Ok(s) => s,
        Err(_) => return,
    };

    let read_dir = match fs::read_dir(dir_str) {
        Ok(read_dir) => read_dir,
        Err(_) => return,
    };

    let mut matches: std::vec::Vec<PathMatch> = std::vec::Vec::new();

    for dir_entry in read_dir {
        let dir_entry = match dir_entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };

        let name = match dir_entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };
        let name_bytes = name.as_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }

        let file_type = match dir_entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };

        if dirs_only && !file_type.is_dir() {
            continue;
        }

        if name_bytes.len() >= file_prefix_len && &name_bytes[..file_prefix_len] == file_prefix {
            matches.push(PathMatch {
                name: name.into_bytes(),
                is_directory: file_type.is_dir(),
            });
        }
    }

    if matches.is_empty() {
        return;
    }

    let match_count = matches.len();

    if match_count == 1 {
        let entry = &matches[0];
        let name_len = entry.name.len();
        let remaining = name_len - file_prefix_len;
        let suffix = if entry.is_directory { b'/' } else { b' ' };
        let insert_len = remaining + 1;
        if insert_len <= result.insertion.len() {
            result.insertion[..remaining].copy_from_slice(&entry.name[file_prefix_len..name_len]);
            result.insertion[remaining] = suffix;
            result.insertion_len = insert_len;
        }
    } else {
        let first = &matches[0];
        let first_len = first.name.len();
        let mut common_len = first_len;

        for i in 1..match_count {
            let entry = &matches[i];
            let name_len = entry.name.len();
            let mut j = file_prefix_len;
            while j < common_len && j < name_len && first.name[j] == entry.name[j] {
                j += 1;
            }
            common_len = j;
        }

        if common_len > file_prefix_len {
            let remaining = common_len - file_prefix_len;
            result.insertion[..remaining].copy_from_slice(&first.name[file_prefix_len..common_len]);
            result.insertion_len = remaining;
        }

        result.show_matches = true;
        let mut pos = 0;
        for i in 0..match_count {
            let entry = &matches[i];
            let name_len = entry.name.len();
            if pos + name_len + 3 < result.matches_buf.len() {
                result.matches_buf[pos..pos + name_len].copy_from_slice(&entry.name[..name_len]);
                pos += name_len;
                if entry.is_directory {
                    result.matches_buf[pos] = b'/';
                    pos += 1;
                }
                result.matches_buf[pos] = b' ';
                pos += 1;
                result.matches_buf[pos] = b' ';
                pos += 1;
            }
        }
        if pos > 0 {
            pos -= 2;
        }
        result.matches_len = pos;
    }
}

fn build_dir_path(
    prefix: &[u8],
    _prefix_len: usize,
    last_slash: Option<usize>,
    cwd: &[u8],
    dir_buf: &mut [u8; 256],
) -> usize {
    let cwd_len = cwd_strlen(cwd);

    if let Some(slash_pos) = last_slash {
        if prefix[0] == b'/' {
            let len = (slash_pos + 1).min(255);
            dir_buf[..len].copy_from_slice(&prefix[..len]);
            return len;
        }
        if cwd_len + slash_pos + 2 >= 255 {
            return 0;
        }
        dir_buf[..cwd_len].copy_from_slice(&cwd[..cwd_len]);
        let mut pos = cwd_len;
        if pos > 0 && dir_buf[pos - 1] != b'/' {
            dir_buf[pos] = b'/';
            pos += 1;
        }
        let path_part = slash_pos + 1;
        dir_buf[pos..pos + path_part].copy_from_slice(&prefix[..path_part]);
        return pos + path_part;
    }

    let len = cwd_len.min(255);
    dir_buf[..len].copy_from_slice(&cwd[..len]);
    len
}

struct PathMatch {
    name: std::vec::Vec<u8>,
    is_directory: bool,
}

fn cwd_strlen(cwd: &[u8]) -> usize {
    cwd.iter().position(|&b| b == 0).unwrap_or(cwd.len())
}
