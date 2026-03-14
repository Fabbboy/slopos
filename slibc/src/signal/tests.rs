use super::*;

pub fn run_signal_tests() -> (u32, u32) {
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

    check!("SIGHUP", SIGHUP == 1);
    check!("SIGINT", SIGINT == 2);
    check!("SIGQUIT", SIGQUIT == 3);
    check!("SIGILL", SIGILL == 4);
    check!("SIGTRAP", SIGTRAP == 5);
    check!("SIGABRT", SIGABRT == 6);
    check!("SIGBUS", SIGBUS == 7);
    check!("SIGFPE", SIGFPE == 8);
    check!("SIGKILL", SIGKILL == 9);
    check!("SIGUSR1", SIGUSR1 == 10);
    check!("SIGSEGV", SIGSEGV == 11);
    check!("SIGUSR2", SIGUSR2 == 12);
    check!("SIGPIPE", SIGPIPE == 13);
    check!("SIGALRM", SIGALRM == 14);
    check!("SIGTERM", SIGTERM == 15);
    check!("SIGCHLD", SIGCHLD == 17);
    check!("SIGCONT", SIGCONT == 18);
    check!("SIGSTOP", SIGSTOP == 19);
    check!("SIGTSTP", SIGTSTP == 20);
    check!("SIGTTIN", SIGTTIN == 21);
    check!("SIGTTOU", SIGTTOU == 22);
    check!("SIGWINCH", SIGWINCH == 28);

    check!("SIG_DFL", SIG_DFL == 0);
    check!("SIG_IGN", SIG_IGN == 1);

    (pass, fail)
}
