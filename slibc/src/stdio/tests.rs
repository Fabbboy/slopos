unsafe extern "C" {
    fn snprintf(buf: *mut u8, n: usize, fmt: *const u8, ...) -> i32;
    fn sscanf(buf: *const u8, fmt: *const u8, ...) -> i32;
}

fn buf_eq(buf: &[u8], expected: &[u8]) -> bool {
    for (i, &e) in expected.iter().enumerate() {
        if buf[i] != e {
            return false;
        }
    }
    true
}

pub fn run_stdio_tests() -> (u32, u32) {
    let mut pass = 0u32;
    let mut fail = 0u32;

    macro_rules! check {
        ($cond:expr) => {
            if $cond {
                pass += 1;
            } else {
                fail += 1;
            }
        };
    }

    unsafe {
        let mut buf = [0u8; 256];

        // %d basic
        let n = snprintf(buf.as_mut_ptr(), 256, b"%d\0".as_ptr(), 42i32);
        check!(n == 2 && buf_eq(&buf, b"42\0"));

        // %d negative
        let n = snprintf(buf.as_mut_ptr(), 256, b"%d\0".as_ptr(), -7i32);
        check!(n == 2 && buf_eq(&buf, b"-7\0"));

        // %d zero
        let n = snprintf(buf.as_mut_ptr(), 256, b"%d\0".as_ptr(), 0i32);
        check!(n == 1 && buf_eq(&buf, b"0\0"));

        // %u unsigned
        let n = snprintf(buf.as_mut_ptr(), 256, b"%u\0".as_ptr(), 65535u32);
        check!(n == 5 && buf_eq(&buf, b"65535\0"));

        // %x lowercase hex
        let n = snprintf(buf.as_mut_ptr(), 256, b"%x\0".as_ptr(), 0xABCDu32);
        check!(n == 4 && buf_eq(&buf, b"abcd\0"));

        // %X uppercase hex
        let n = snprintf(buf.as_mut_ptr(), 256, b"%X\0".as_ptr(), 0xABCDu32);
        check!(n == 4 && buf_eq(&buf, b"ABCD\0"));

        // %o octal
        let n = snprintf(buf.as_mut_ptr(), 256, b"%o\0".as_ptr(), 255u32);
        check!(n == 3 && buf_eq(&buf, b"377\0"));

        // %s string
        let n = snprintf(buf.as_mut_ptr(), 256, b"%s\0".as_ptr(), b"hello\0".as_ptr());
        check!(n == 5 && buf_eq(&buf, b"hello\0"));

        // %s null pointer -> "(null)"
        let n = snprintf(
            buf.as_mut_ptr(),
            256,
            b"%s\0".as_ptr(),
            core::ptr::null::<u8>(),
        );
        check!(n == 6 && buf_eq(&buf, b"(null)\0"));

        // %c character
        let n = snprintf(buf.as_mut_ptr(), 256, b"%c\0".as_ptr(), b'Z' as i32);
        check!(n == 1 && buf_eq(&buf, b"Z\0"));

        // %% literal percent
        let n = snprintf(buf.as_mut_ptr(), 256, b"%%\0".as_ptr());
        check!(n == 1 && buf_eq(&buf, b"%\0"));

        // mixed format
        let n = snprintf(
            buf.as_mut_ptr(),
            256,
            b"%s=%d\0".as_ptr(),
            b"x\0".as_ptr(),
            99i32,
        );
        check!(n == 4 && buf_eq(&buf, b"x=99\0"));

        // width padding (right-aligned)
        let n = snprintf(buf.as_mut_ptr(), 256, b"%5d\0".as_ptr(), 42i32);
        check!(n == 5 && buf_eq(&buf, b"   42\0"));

        // zero-padded
        let n = snprintf(buf.as_mut_ptr(), 256, b"%05d\0".as_ptr(), 42i32);
        check!(n == 5 && buf_eq(&buf, b"00042\0"));

        // left-aligned
        let n = snprintf(buf.as_mut_ptr(), 256, b"%-5d!\0".as_ptr(), 42i32);
        check!(n == 6 && buf_eq(&buf, b"42   !\0"));

        // precision on string (truncation)
        let n = snprintf(
            buf.as_mut_ptr(),
            256,
            b"%.3s\0".as_ptr(),
            b"hello\0".as_ptr(),
        );
        check!(n == 3 && buf_eq(&buf, b"hel\0"));

        // snprintf truncation
        let n = snprintf(buf.as_mut_ptr(), 4, b"%d\0".as_ptr(), 12345i32);
        check!(n == 5 && buf_eq(&buf, b"123\0"));

        // #x alternate form
        let n = snprintf(buf.as_mut_ptr(), 256, b"%#x\0".as_ptr(), 255u32);
        check!(n == 4 && buf_eq(&buf, b"0xff\0"));

        // + sign flag
        let n = snprintf(buf.as_mut_ptr(), 256, b"%+d\0".as_ptr(), 42i32);
        check!(n == 3 && buf_eq(&buf, b"+42\0"));

        // sscanf %d
        let mut val: i32 = 0;
        let n = sscanf(b"123\0".as_ptr(), b"%d\0".as_ptr(), &mut val as *mut i32);
        check!(n == 1 && val == 123);

        // sscanf %s
        let mut sbuf = [0u8; 64];
        let n = sscanf(
            b"hello world\0".as_ptr(),
            b"%s\0".as_ptr(),
            sbuf.as_mut_ptr(),
        );
        check!(n == 1 && buf_eq(&sbuf, b"hello\0"));

        // sscanf mixed
        let mut a: i32 = 0;
        let mut b: i32 = 0;
        let n = sscanf(
            b"10 20\0".as_ptr(),
            b"%d %d\0".as_ptr(),
            &mut a as *mut i32,
            &mut b as *mut i32,
        );
        check!(n == 2 && a == 10 && b == 20);
    }

    (pass, fail)
}
