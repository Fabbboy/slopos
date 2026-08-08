//! Pure network formatting, parsing and command-grammar core.
//!
//! Two problems put this logic in a crate of its own.
//!
//! **The userland crate cannot be tested.** `userland/Cargo.toml` sets
//! `test = false` on its `[lib]` target and `just test-host` does not name
//! `slopos-userland`, so every `#[cfg(test)]` inside it is dead code that never
//! compiles and never runs. Address parsing and argument parsing are exactly
//! the kind of logic where a silent wrong answer is indistinguishable from a
//! right one, so they live here, where `cargo test -p slopos-net-core` runs
//! them on the host.
//!
//! **The CLI and the compositor must agree.** `ip` renders a link state and the
//! status indicator renders the same state; if each spells it itself, the two
//! drift and a person reading both sees a contradiction. [`render`] is the one
//! place a `NET_*` constant becomes a string, so there is nothing to drift.
//!
//! Everything here is free of `alloc`, `unsafe` and any syscall surface: the
//! parsers work on `&[u8]` and the renderers write into a
//! [`core::fmt::Write`], so a caller supplies its own buffer. Interface names
//! are bytes in the ABI and stay bytes all the way through the `ip` grammar.
//!
//! Rendered strings are constrained by the console font: `font/src/lib.rs`
//! covers ASCII `0x20..=0x7E`, Latin-1 `0xA0..=0xFF`, and exactly `€ ˚ ˇ`.
//! Anything else draws as the replacement glyph, so an em dash in a
//! human-facing string is a rendering bug. [`render::is_renderable`] encodes
//! that range and a test holds every string this crate can produce to it.

#![no_std]
#![forbid(unsafe_code)]

pub mod addr;
pub mod argv;
pub mod cidr;
pub mod columns;
pub mod ip_plan;
pub mod mac;
pub mod render;
pub mod ss_filter;

pub use addr::Ipv4;
pub use argv::{TokenError, matches, resolve_token, scan_bundled};
pub use cidr::{Cidr, mask_to_prefix_len, prefix_len_to_mask};
pub use columns::field;
pub use ip_plan::{Invocation, IpError, Object, Options, Plan, RouteDest, parse};
pub use mac::Mac;
pub use render::{
    CONNECTIVITY_DISABLED, IF_FLAG_NAMES, addr_origin, addr_scope, connectivity,
    connectivity_label, dhcp_reason, dhcp_state, iface_kind, is_renderable, neigh_state,
    oper_state, route_origin, write_if_flags,
};
pub use ss_filter::{SS_ALL, SS_LISTEN, SS_TCP, SS_UDP, is_connected, ss_row_selected};
