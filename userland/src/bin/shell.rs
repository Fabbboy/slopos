#![feature(restricted_std)]

fn main() {
    std::process::exit(slopos_userland::apps::shell::shell_user_main());
}
