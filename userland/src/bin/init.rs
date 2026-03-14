#![feature(restricted_std)]
#![no_main]
slopos_userland::entry!(slopos_userland::apps::init_process::init_user_main);
