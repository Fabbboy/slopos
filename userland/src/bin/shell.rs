#![feature(restricted_std)]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    std::process::exit(slopos_userland::apps::shell::shell_user_main(&argv));
}
