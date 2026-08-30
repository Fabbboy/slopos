#![feature(restricted_std)]

//! SlopRing multishot (ABI v2) end-to-end test.
//!
//! Driven by `poll_add_multishot` over a pipe: each readiness transition
//! (not-ready → ready) drives one `MultishotStream` item, with edge-tracking
//! suppressing the level flood (SLOPRING §1.2).
//!
//! `accept_multishot` is driven over a real loopback TCP connection.

use slopos_abi::net::{AF_INET, SOCK_STREAM, SockAddrIn};
use slopos_abi::syscall::POLLIN;
use slopos_userland as _;
use slopos_userland::ring::{Ring, slopfut};
use slopos_userland::syscall::{fs, net};

/// write → ready, drain → not-ready, write → ready: two writes yield exactly
/// two items, both carrying POLLIN.
fn test_poll_multishot_two_edges() -> bool {
    let Ok((rd, wr)) = fs::pipe() else {
        return false;
    };
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    let rd_fd = rd.raw();
    let wr_fd = wr.raw();

    let ok = slopfut::block_on(ring, async move {
        let mut stream = slopfut::poll_add_multishot(rd_fd, POLLIN);

        if fs::write_slice(wr_fd, b"a").is_err() {
            return false;
        }
        let first = stream.next().await;
        if !matches!(first, Some(r) if (r as u16) & POLLIN != 0) {
            return false;
        }

        // The nop park forces a harvest pass, so the kernel observes the
        // not-ready level and resets the edge cache; without it no second
        // transition fires.
        let mut buf = [0u8; 8];
        let _ = fs::read_slice(rd_fd, &mut buf);
        let _ = slopfut::nop().await;

        if fs::write_slice(wr_fd, b"b").is_err() {
            return false;
        }
        let second = stream.next().await;
        matches!(second, Some(r) if (r as u16) & POLLIN != 0)
    });

    drop(wr);
    drop(rd);
    ok
}

fn test_oneshot_equivalence() -> bool {
    let Ok((rd1, wr1)) = fs::pipe() else {
        return false;
    };
    if fs::write_slice(wr1.raw(), b"x").is_err() {
        return false;
    }
    let Ok(r1) = Ring::setup(8) else {
        return false;
    };
    let rd1_fd = rd1.raw();
    let oneshot = slopfut::block_on(r1, async move { slopfut::poll_add(rd1_fd, POLLIN).await });
    drop(wr1);
    drop(rd1);

    let Ok((rd2, wr2)) = fs::pipe() else {
        return false;
    };
    if fs::write_slice(wr2.raw(), b"x").is_err() {
        return false;
    }
    let Ok(r2) = Ring::setup(8) else {
        return false;
    };
    let rd2_fd = rd2.raw();
    let multishot = slopfut::block_on(r2, async move {
        let mut s = slopfut::poll_add_multishot(rd2_fd, POLLIN);
        s.next().await
    });
    drop(wr2);
    drop(rd2);

    oneshot > 0
        && (oneshot as u16) & POLLIN != 0
        && matches!(multishot, Some(m) if (m as u16) & POLLIN != 0 && m == oneshot)
}

/// Raced against a short timer so the empty-pipe poll never fires and the
/// stream is dropped while still armed.
fn test_drop_cancels() -> bool {
    let Ok((rd, _wr)) = fs::pipe() else {
        return false;
    };
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    let rd_fd = rd.raw();

    let timer_won = slopfut::block_on(ring, async move {
        // Both `Next` and the `OP_TIMEOUT` leaf are `Unpin`, as `select2`
        // needs.
        let mut stream = slopfut::poll_add_multishot(rd_fd, POLLIN);
        match slopfut::select2(stream.next(), slopfut::timeout(50_000_000)).await {
            slopfut::Either2::A(_) => false,
            slopfut::Either2::B(_) => true,
        }
    });

    drop(_wr);
    drop(rd);
    timer_won
}

/// A multishot accept yields each connection as a stream item: two clients
/// dial one listener and both arrive on the same armed `OP_ACCEPT`.
fn test_accept_multishot_yields_connections() -> bool {
    const PORT: u16 = 18200;

    let Ok(listener) = net::socket(AF_INET, SOCK_STREAM, 0) else {
        return false;
    };
    if net::bind_any(listener.raw(), PORT).is_err() || net::listen(listener.raw(), 4).is_err() {
        return false;
    }
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    let listen_fd = listener.raw();

    let addr = SockAddrIn {
        family: AF_INET,
        port: PORT.to_be(),
        addr: [127, 0, 0, 1],
        _pad: [0; 8],
    };

    slopfut::block_on(ring, async move {
        let mut stream = slopfut::accept_multishot(listen_fd);

        for _ in 0..2 {
            let Ok(client) = net::socket(AF_INET, SOCK_STREAM, 0) else {
                return false;
            };
            if net::connect(client.raw(), &addr).is_err() {
                return false;
            }
            match stream.next().await {
                Some(fd) if fd >= 0 => {
                    let _ = fs::close_fd_raw(fd);
                }
                _ => return false,
            }
            drop(client);
        }
        true
    })
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("poll_multishot_two_edges", test_poll_multishot_two_edges),
    ("oneshot_equivalence", test_oneshot_equivalence),
    ("drop_cancels", test_drop_cancels),
    (
        "accept_multishot_yields_connections",
        test_accept_multishot_yields_connections,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
