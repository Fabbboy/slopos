use super::shim;

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

    let mut buf = [0u8; 256];

    let n = shim::snprintf_d(&mut buf, b"%d\0", 42i32);
    check!(n == 2 && buf_eq(&buf, b"42\0"));

    let n = shim::snprintf_d(&mut buf, b"%d\0", -7i32);
    check!(n == 2 && buf_eq(&buf, b"-7\0"));

    let n = shim::snprintf_d(&mut buf, b"%d\0", 0i32);
    check!(n == 1 && buf_eq(&buf, b"0\0"));

    let n = shim::snprintf_u(&mut buf, b"%u\0", 65535u32);
    check!(n == 5 && buf_eq(&buf, b"65535\0"));

    let n = shim::snprintf_u(&mut buf, b"%x\0", 0xABCDu32);
    check!(n == 4 && buf_eq(&buf, b"abcd\0"));

    let n = shim::snprintf_u(&mut buf, b"%X\0", 0xABCDu32);
    check!(n == 4 && buf_eq(&buf, b"ABCD\0"));

    let n = shim::snprintf_u(&mut buf, b"%o\0", 255u32);
    check!(n == 3 && buf_eq(&buf, b"377\0"));

    let n = shim::snprintf_s(&mut buf, b"%s\0", b"hello\0".as_ptr());
    check!(n == 5 && buf_eq(&buf, b"hello\0"));

    let n = shim::snprintf_s(&mut buf, b"%s\0", core::ptr::null::<u8>());
    check!(n == 6 && buf_eq(&buf, b"(null)\0"));

    let n = shim::snprintf_d(&mut buf, b"%c\0", b'Z' as i32);
    check!(n == 1 && buf_eq(&buf, b"Z\0"));

    let n = shim::snprintf_fmt_only(&mut buf, b"%%\0");
    check!(n == 1 && buf_eq(&buf, b"%\0"));

    let n = shim::snprintf_sd(&mut buf, b"%s=%d\0", b"x\0".as_ptr(), 99i32);
    check!(n == 4 && buf_eq(&buf, b"x=99\0"));

    let n = shim::snprintf_d(&mut buf, b"%5d\0", 42i32);
    check!(n == 5 && buf_eq(&buf, b"   42\0"));

    let n = shim::snprintf_d(&mut buf, b"%05d\0", 42i32);
    check!(n == 5 && buf_eq(&buf, b"00042\0"));

    let n = shim::snprintf_d(&mut buf, b"%-5d!\0", 42i32);
    check!(n == 6 && buf_eq(&buf, b"42   !\0"));

    let n = shim::snprintf_s(&mut buf, b"%.3s\0", b"hello\0".as_ptr());
    check!(n == 3 && buf_eq(&buf, b"hel\0"));

    let n = shim::snprintf_d_with_cap(&mut buf, 4, b"%d\0", 12345i32);
    check!(n == 5 && buf_eq(&buf, b"123\0"));

    let n = shim::snprintf_u(&mut buf, b"%#x\0", 255u32);
    check!(n == 4 && buf_eq(&buf, b"0xff\0"));

    let n = shim::snprintf_d(&mut buf, b"%+d\0", 42i32);
    check!(n == 3 && buf_eq(&buf, b"+42\0"));

    let mut val: i32 = 0;
    let n = shim::sscanf_d(b"123\0", b"%d\0", &mut val);
    check!(n == 1 && val == 123);

    let mut sbuf = [0u8; 64];
    let n = shim::sscanf_s(b"hello world\0", b"%s\0", &mut sbuf);
    check!(n == 1 && buf_eq(&sbuf, b"hello\0"));

    let mut a: i32 = 0;
    let mut b: i32 = 0;
    let n = shim::sscanf_dd(b"10 20\0", b"%d %d\0", &mut a, &mut b);
    check!(n == 2 && a == 10 && b == 20);

    (pass, fail)
}
