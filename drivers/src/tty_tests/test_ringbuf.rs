use super::*;

pub fn test_ringbuf_new_is_empty() -> TestResult {
    let rb = RingBuf::<8>::new();
    if !rb.is_empty() || rb.count() != 0 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_push_pop() -> TestResult {
    let mut rb = RingBuf::<8>::new();
    if !rb.push(b'a') || !rb.push(b'b') || !rb.push(b'c') {
        return TestResult::Fail;
    }
    if rb.pop() != Some(b'a') || rb.pop() != Some(b'b') || rb.pop() != Some(b'c') {
        return TestResult::Fail;
    }
    if rb.pop().is_some() {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_full_returns_false() -> TestResult {
    let mut rb = RingBuf::<4>::new();
    for b in b"abcd" {
        if !rb.push(*b) {
            return TestResult::Fail;
        }
    }
    if rb.push(b'e') {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_peek_does_not_consume() -> TestResult {
    let mut rb = RingBuf::<8>::new();
    rb.push(b'x');
    rb.push(b'y');
    if rb.peek() != Some(b'x') || rb.count() != 2 {
        return TestResult::Fail;
    }
    if rb.pop() != Some(b'x') || rb.pop() != Some(b'y') {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_peek_at_offset() -> TestResult {
    let mut rb = RingBuf::<8>::new();
    for b in b"wxyz" {
        rb.push(*b);
    }
    if rb.peek_at(0) != Some(b'w')
        || rb.peek_at(1) != Some(b'x')
        || rb.peek_at(3) != Some(b'z')
        || rb.peek_at(4).is_some()
    {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_read_bulk() -> TestResult {
    let mut rb = RingBuf::<8>::new();
    for b in b"hello" {
        rb.push(*b);
    }
    let mut out = [0u8; 8];
    let n = rb.read(&mut out);
    if n != 5 || &out[..5] != b"hello" || !rb.is_empty() {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_read_partial() -> TestResult {
    let mut rb = RingBuf::<8>::new();
    for b in b"hello" {
        rb.push(*b);
    }
    let mut out = [0u8; 3];
    let n = rb.read(&mut out);
    if n != 3 || &out != b"hel" {
        return TestResult::Fail;
    }
    if rb.count() != 2 || rb.peek() != Some(b'l') {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_flush_resets() -> TestResult {
    let mut rb = RingBuf::<8>::new();
    for b in b"abcd" {
        rb.push(*b);
    }
    rb.flush();
    if !rb.is_empty() || rb.count() != 0 || rb.peek().is_some() {
        return TestResult::Fail;
    }
    if !rb.push(b'z') || rb.pop() != Some(b'z') {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_wraparound() -> TestResult {
    let mut rb = RingBuf::<4>::new();
    for b in b"abcd" {
        rb.push(*b);
    }
    if rb.pop() != Some(b'a') || rb.pop() != Some(b'b') {
        return TestResult::Fail;
    }
    if !rb.push(b'e') || !rb.push(b'f') {
        return TestResult::Fail;
    }
    let mut out = [0u8; 4];
    let n = rb.read(&mut out);
    if n != 4 || &out != b"cdef" {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_capacity_and_free() -> TestResult {
    let mut rb = RingBuf::<8>::new();
    if rb.capacity() != 8 || rb.free() != 8 {
        return TestResult::Fail;
    }
    rb.push(b'a');
    rb.push(b'b');
    if rb.count() != 2 || rb.free() != 6 {
        return TestResult::Fail;
    }
    TestResult::Pass
}
