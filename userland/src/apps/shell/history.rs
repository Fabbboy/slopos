use std::sync::Mutex;

const MAX_HISTORY: usize = 64;
const MAX_LINE_LEN: usize = 256;

struct HistoryInner {
    entries: [[u8; MAX_LINE_LEN]; MAX_HISTORY],
    lengths: [u16; MAX_HISTORY],
    count: usize,
    write_pos: usize,
    cursor: usize,
    browsing: bool,
    saved_input: [u8; MAX_LINE_LEN],
    saved_len: usize,
}

impl HistoryInner {
    const fn new() -> Self {
        Self {
            entries: [[0; MAX_LINE_LEN]; MAX_HISTORY],
            lengths: [0; MAX_HISTORY],
            count: 0,
            write_pos: 0,
            cursor: 0,
            browsing: false,
            saved_input: [0; MAX_LINE_LEN],
            saved_len: 0,
        }
    }
}

static HISTORY: Mutex<HistoryInner> = Mutex::new(HistoryInner::new());

fn with_history<R, F: FnOnce(&mut HistoryInner) -> R>(f: F) -> R {
    f(&mut HISTORY.lock().unwrap())
}

pub fn push(line: &[u8], len: usize) {
    if len == 0 {
        return;
    }
    with_history(|h| {
        let store_len = len.min(MAX_LINE_LEN);

        if h.count > 0 {
            let last_pos = if h.write_pos == 0 {
                MAX_HISTORY - 1
            } else {
                h.write_pos - 1
            };
            let last_len = h.lengths[last_pos] as usize;
            if last_len == store_len {
                let mut same = true;
                for i in 0..store_len {
                    if h.entries[last_pos][i] != line[i] {
                        same = false;
                        break;
                    }
                }
                if same {
                    return;
                }
            }
        }

        h.entries[h.write_pos][..store_len].copy_from_slice(&line[..store_len]);
        if store_len < MAX_LINE_LEN {
            h.entries[h.write_pos][store_len] = 0;
        }
        h.lengths[h.write_pos] = store_len as u16;
        h.write_pos = (h.write_pos + 1) % MAX_HISTORY;
        if h.count < MAX_HISTORY {
            h.count += 1;
        }
    });
}

pub fn navigate_up(current_input: &[u8], current_len: usize, out: &mut [u8]) -> Option<usize> {
    with_history(|h| {
        if h.count == 0 {
            return None;
        }

        if !h.browsing {
            let save_len = current_len.min(MAX_LINE_LEN);
            h.saved_input[..save_len].copy_from_slice(&current_input[..save_len]);
            h.saved_len = save_len;
            h.browsing = true;
            h.cursor = 0;
        } else if h.cursor + 1 >= h.count {
            return None;
        } else {
            h.cursor += 1;
        }

        let idx = if h.write_pos >= h.cursor + 1 {
            h.write_pos - h.cursor - 1
        } else {
            MAX_HISTORY + h.write_pos - h.cursor - 1
        } % MAX_HISTORY;

        let len = h.lengths[idx] as usize;
        let copy_len = len.min(out.len());
        out[..copy_len].copy_from_slice(&h.entries[idx][..copy_len]);
        Some(copy_len)
    })
}

pub fn navigate_down(out: &mut [u8]) -> Option<usize> {
    with_history(|h| {
        if !h.browsing {
            return None;
        }

        if h.cursor == 0 {
            h.browsing = false;
            let len = h.saved_len;
            let copy_len = len.min(out.len());
            out[..copy_len].copy_from_slice(&h.saved_input[..copy_len]);
            return Some(copy_len);
        }

        h.cursor -= 1;

        let idx = if h.write_pos >= h.cursor + 1 {
            h.write_pos - h.cursor - 1
        } else {
            MAX_HISTORY + h.write_pos - h.cursor - 1
        } % MAX_HISTORY;

        let len = h.lengths[idx] as usize;
        let copy_len = len.min(out.len());
        out[..copy_len].copy_from_slice(&h.entries[idx][..copy_len]);
        Some(copy_len)
    })
}

pub fn reset_cursor() {
    with_history(|h| {
        h.browsing = false;
        h.cursor = 0;
    });
}
