use super::wait::*;

pub fn run_process_tests() -> (u32, u32) {
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

    // Normal exit: status is (exit_code << 8) | 0
    let normal_exit_42 = 42 << 8;
    check!("WIFEXITED normal", WIFEXITED(normal_exit_42));
    check!("WEXITSTATUS 42", WEXITSTATUS(normal_exit_42) == 42);
    check!("!WIFSIGNALED normal", !WIFSIGNALED(normal_exit_42));

    let normal_exit_0 = 0;
    check!("WIFEXITED 0", WIFEXITED(normal_exit_0));
    check!("WEXITSTATUS 0", WEXITSTATUS(normal_exit_0) == 0);

    let normal_exit_255 = 255 << 8;
    check!("WIFEXITED 255", WIFEXITED(normal_exit_255));
    check!("WEXITSTATUS 255", WEXITSTATUS(normal_exit_255) == 255);

    // Signal termination: status = signal_number (low 7 bits nonzero)
    let killed_by_9 = 9; // SIGKILL
    check!("!WIFEXITED signal", !WIFEXITED(killed_by_9));
    check!("WIFSIGNALED signal", WIFSIGNALED(killed_by_9));
    check!("WTERMSIG SIGKILL", WTERMSIG(killed_by_9) == 9);

    let killed_by_11 = 11; // SIGSEGV
    check!("WIFSIGNALED SIGSEGV", WIFSIGNALED(killed_by_11));
    check!("WTERMSIG SIGSEGV", WTERMSIG(killed_by_11) == 11);

    // Stopped: status = (stop_sig << 8) | 0x7F
    let stopped_sigstop = (19 << 8) | 0x7F;
    check!("!WIFEXITED stopped", !WIFEXITED(stopped_sigstop));
    check!("!WIFSIGNALED stopped", !WIFSIGNALED(stopped_sigstop));
    check!("WIFSTOPPED stopped", WIFSTOPPED(stopped_sigstop));
    check!("WSTOPSIG SIGSTOP", WSTOPSIG(stopped_sigstop) == 19);

    // WNOHANG constant
    check!("WNOHANG value", WNOHANG == 1);

    (pass, fail)
}
