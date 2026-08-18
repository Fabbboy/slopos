//! Pure network formatting, parsing and command-grammar core, shared so that
//! [`render`] is the one place a `NET_*` constant becomes a string. Free of
//! `alloc`, `unsafe` and syscalls: parsers take `&[u8]`, renderers write into
//! a [`core::fmt::Write`] the caller supplies.
//!
//! Rendered strings are constrained by the console font — ASCII `0x20..=0x7E`,
//! Latin-1 `0xA0..=0xFF`, and exactly `€ ˚ ˇ`; anything else draws as the
//! replacement glyph. [`render::is_renderable`] encodes that range.

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
