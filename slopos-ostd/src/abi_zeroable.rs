//! `Zeroable` impls for `slopos-abi` types.
//!
//! `Zeroable` is OSTD's trait and `slopos-abi` is the upstream ABI
//! crate; the orphan rule permits these impls to live here even though
//! the types themselves are defined in `slopos-abi`. Centralising the
//! impls in OSTD keeps the unsafe surface inside the trusted core and
//! lets downstream kernel crates derive `Zeroable` on aggregates that
//! embed these ABI types without needing their own unsafe blocks.
//!
//! Every impl below carries a SAFETY comment naming the bit-pattern
//! justification: the all-zero byte pattern must be a well-formed
//! value of the type. See `slopos_ostd::Zeroable` for the trait
//! contract.

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_abi::input::{InputEvent, InputEventData, InputEventType};
use slopos_abi::syscall::UserPollFd;
use slopos_abi::syscall::termios::{
    ControlFlags, InputFlags, LocalFlags, OutputFlags, UserTermios,
};

use crate::Zeroable;

// SAFETY: `bitflags::bitflags!` 2.4 emits `#[repr(transparent)]` over
// the declared backing integer. For all four termios bitflag types the
// backing integer is `u32`; the all-zero `u32` represents the empty
// flag set, which is a valid value (`Flags::empty()` is the canonical
// zero constructor).
unsafe impl Zeroable for InputFlags {}
unsafe impl Zeroable for OutputFlags {}
unsafe impl Zeroable for LocalFlags {}
unsafe impl Zeroable for ControlFlags {}

// SAFETY: `UserTermios` is `#[repr(C)]` over the four bitflag types
// (each Zeroable above), a `u8`, a `[u8; NCCS]`, and two `u32`s. Every
// component accepts the all-zero pattern: the bitflags yield an empty
// set, the integer fields are zero, and the byte array is all-zero.
unsafe impl Zeroable for UserTermios {}

// SAFETY: `UserPollFd` is `#[repr(C)]` over `i32 + u16 + u16`. All
// three components are primitive integers whose all-zero pattern is a
// valid value (`fd = 0`, `events = 0`, `revents = 0`).
unsafe impl Zeroable for UserPollFd {}

// SAFETY: `InputEventData` is `#[repr(C)]` over two `u32` fields; the
// all-zero pattern is the canonical "no payload" value.
unsafe impl Zeroable for InputEventData {}

// SAFETY: `InputEventType` is `#[repr(u8)]` with `KeyPress = 0` as the
// `#[default]` variant (see `slopos-abi/src/input.rs`); discriminant 0
// is therefore a valid representation.
unsafe impl Zeroable for InputEventType {}

// SAFETY: `InputEvent` is `#[repr(C)]` over `InputEventType`,
// `[u8; 3]`, `u64`, and `InputEventData`. Each component is Zeroable
// per the impls above, so the all-zero aggregate is well-formed (it
// represents a `KeyPress` event with empty payload and zero timestamp,
// which is also `InputEvent::default()`).
unsafe impl Zeroable for InputEvent {}

// SAFETY: `PhysAddr` and `VirtAddr` are `#[repr(transparent)]` over
// `u64`. The all-zero pattern is the canonical NULL physical / virtual
// address (`PhysAddr::NULL` is `PhysAddr(0)`).
unsafe impl Zeroable for PhysAddr {}
unsafe impl Zeroable for VirtAddr {}
