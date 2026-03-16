#![feature(restricted_std)]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    slopos_userland::apps::ping::ping_main(args);
}
