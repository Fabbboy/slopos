use super::shim;
use super::*;
use slopos_abi::syscall::{ControlFlags, InputFlags, LocalFlags, OutputFlags, UserTermios};

pub fn run_tty_tests() -> (u32, u32) {
    let mut pass = 0u32;
    let mut fail = 0u32;

    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond {
                pass += 1;
            } else {
                fail += 1;
            }
        };
    }

    check!("TCSANOW_eq_0", TCSANOW == 0);
    check!("TCSADRAIN_eq_1", TCSADRAIN == 1);
    check!("TCSAFLUSH_eq_2", TCSAFLUSH == 2);

    check!(
        "UserTermios_size_reasonable",
        core::mem::size_of::<UserTermios>() >= 40
    );

    check!("TCGETS_eq_0x5401", slopos_abi::syscall::TCGETS == 0x5401);
    check!("TCSETS_eq_0x5402", slopos_abi::syscall::TCSETS == 0x5402);
    check!("TCSETSW_eq_0x5403", slopos_abi::syscall::TCSETSW == 0x5403);
    check!("TCSETSF_eq_0x5404", slopos_abi::syscall::TCSETSF == 0x5404);

    check!("cfmakeraw_clears_flags", {
        let mut t = UserTermios {
            c_iflag: InputFlags::from_bits_retain(0xFFFF_FFFF),
            c_oflag: OutputFlags::from_bits_retain(0xFFFF_FFFF),
            c_cflag: ControlFlags::empty(),
            c_lflag: LocalFlags::from_bits_retain(0xFFFF_FFFF),
            c_line: 0,
            c_cc: [0; slopos_abi::syscall::NCCS],
            c_ispeed: 0,
            c_ospeed: 0,
        };
        shim::cfmakeraw(&mut t);
        let iflags_cleared = !t.c_iflag.intersects(InputFlags::ICRNL | InputFlags::IXON);
        let oflags_cleared = !t.c_oflag.contains(OutputFlags::OPOST);
        let lflags_cleared = !t
            .c_lflag
            .intersects(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG);
        let vmin_set = t.c_cc[slopos_abi::syscall::VMIN] == 1;
        let vtime_set = t.c_cc[slopos_abi::syscall::VTIME] == 0;
        iflags_cleared && oflags_cleared && lflags_cleared && vmin_set && vtime_set
    });

    check!("cfgetispeed_returns_value", {
        let t = UserTermios {
            c_iflag: InputFlags::empty(),
            c_oflag: OutputFlags::empty(),
            c_cflag: ControlFlags::empty(),
            c_lflag: LocalFlags::empty(),
            c_line: 0,
            c_cc: [0; slopos_abi::syscall::NCCS],
            c_ispeed: 9600,
            c_ospeed: 19200,
        };
        shim::cfgetispeed(&t) == 9600 && shim::cfgetospeed(&t) == 19200
    });

    check!("cfsetispeed_sets_value", {
        let mut t = UserTermios {
            c_iflag: InputFlags::empty(),
            c_oflag: OutputFlags::empty(),
            c_cflag: ControlFlags::empty(),
            c_lflag: LocalFlags::empty(),
            c_line: 0,
            c_cc: [0; slopos_abi::syscall::NCCS],
            c_ispeed: 0,
            c_ospeed: 0,
        };
        shim::cfsetispeed(&mut t, 115200) == 0 && t.c_ispeed == 115200
    });

    check!("cfsetospeed_sets_value", {
        let mut t = UserTermios {
            c_iflag: InputFlags::empty(),
            c_oflag: OutputFlags::empty(),
            c_cflag: ControlFlags::empty(),
            c_lflag: LocalFlags::empty(),
            c_line: 0,
            c_cc: [0; slopos_abi::syscall::NCCS],
            c_ispeed: 0,
            c_ospeed: 0,
        };
        shim::cfsetospeed(&mut t, 38400) == 0 && t.c_ospeed == 38400
    });

    check!("cfgetispeed_null", shim::cfgetispeed_null() == 0);

    check!("cfsetispeed_null", shim::cfsetispeed_null(9600) == -1);

    check!("tcgetattr_null_termios", shim::tcgetattr_null(0) == -1);

    check!(
        "tcsetattr_null_termios",
        shim::tcsetattr_null(0, TCSANOW) == -1
    );

    check!("tcsetattr_invalid_action", {
        let t = UserTermios {
            c_iflag: InputFlags::empty(),
            c_oflag: OutputFlags::empty(),
            c_cflag: ControlFlags::empty(),
            c_lflag: LocalFlags::empty(),
            c_line: 0,
            c_cc: [0; slopos_abi::syscall::NCCS],
            c_ispeed: 0,
            c_ospeed: 0,
        };
        shim::tcsetattr(0, 99, &t) == -1
    });

    (pass, fail)
}
