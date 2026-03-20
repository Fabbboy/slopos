use super::*;

pub fn test_ringbuf_new_is_empty() -> TestResult {
    let rb = RingBuffer::<u8, 8>::new_zeroed();
    if !rb.is_empty() || rb.count() != 0 {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_push_pop() -> TestResult {
    let mut rb = RingBuffer::<u8, 8>::new_zeroed();
    if !rb.try_push(b'a') || !rb.try_push(b'b') || !rb.try_push(b'c') {
        return TestResult::Fail;
    }
    if rb.try_pop() != Some(b'a') || rb.try_pop() != Some(b'b') || rb.try_pop() != Some(b'c') {
        return TestResult::Fail;
    }
    if rb.try_pop().is_some() {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_full_returns_false() -> TestResult {
    let mut rb = RingBuffer::<u8, 4>::new_zeroed();
    for b in b"abcd" {
        if !rb.try_push(*b) {
            return TestResult::Fail;
        }
    }
    if rb.try_push(b'e') {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_peek_does_not_consume() -> TestResult {
    let mut rb = RingBuffer::<u8, 8>::new_zeroed();
    rb.try_push(b'x');
    rb.try_push(b'y');
    if rb.peek().copied() != Some(b'x') || rb.count() != 2 {
        return TestResult::Fail;
    }
    if rb.try_pop() != Some(b'x') || rb.try_pop() != Some(b'y') {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_peek_at_offset() -> TestResult {
    let mut rb = RingBuffer::<u8, 8>::new_zeroed();
    for b in b"wxyz" {
        rb.try_push(*b);
    }
    if rb.peek_at_one(0).copied() != Some(b'w')
        || rb.peek_at_one(1).copied() != Some(b'x')
        || rb.peek_at_one(3).copied() != Some(b'z')
        || rb.peek_at_one(4).is_some()
    {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_read_bulk() -> TestResult {
    let mut rb = RingBuffer::<u8, 8>::new_zeroed();
    for b in b"hello" {
        rb.try_push(*b);
    }
    let mut out = [0u8; 8];
    let n = rb.read(&mut out);
    if n != 5 || &out[..5] != b"hello" || !rb.is_empty() {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_read_partial() -> TestResult {
    let mut rb = RingBuffer::<u8, 8>::new_zeroed();
    for b in b"hello" {
        rb.try_push(*b);
    }
    let mut out = [0u8; 3];
    let n = rb.read(&mut out);
    if n != 3 || &out != b"hel" {
        return TestResult::Fail;
    }
    if rb.count() != 2 || rb.peek().copied() != Some(b'l') {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_flush_resets() -> TestResult {
    let mut rb = RingBuffer::<u8, 8>::new_zeroed();
    for b in b"abcd" {
        rb.try_push(*b);
    }
    rb.flush();
    if !rb.is_empty() || rb.count() != 0 || rb.peek().copied().is_some() {
        return TestResult::Fail;
    }
    if !rb.try_push(b'z') || rb.try_pop() != Some(b'z') {
        return TestResult::Fail;
    }
    TestResult::Pass
}

pub fn test_ringbuf_wraparound() -> TestResult {
    let mut rb = RingBuffer::<u8, 4>::new_zeroed();
    for b in b"abcd" {
        rb.try_push(*b);
    }
    if rb.try_pop() != Some(b'a') || rb.try_pop() != Some(b'b') {
        return TestResult::Fail;
    }
    if !rb.try_push(b'e') || !rb.try_push(b'f') {
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
    let mut rb = RingBuffer::<u8, 8>::new_zeroed();
    if rb.capacity() != 8 || rb.free() != 8 {
        return TestResult::Fail;
    }
    rb.try_push(b'a');
    rb.try_push(b'b');
    if rb.count() != 2 || rb.free() != 6 {
        return TestResult::Fail;
    }
    TestResult::Pass
}
