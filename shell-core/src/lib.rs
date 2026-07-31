//! Pure, allocation-free shell input core.
//!
//! A shell reading commands from a pipe shares that pipe with the commands it
//! runs: `cmd | shell` hands the same descriptor to every child, and
//! `{ read x; cat; } < file` expects `cat` to see whatever `read` did not
//! consume. So the reader may consume the line it is about to execute and
//! **not one byte more** — a block read that keeps the remainder of its buffer
//! either loses those bytes or steals them from a child.
//!
//! [`ScriptReader`] is that guarantee expressed as code rather than as a
//! comment: it frames lines out of a [`ByteSource`] one byte per call, so
//! over-reading is not a mistake the implementation is capable of making. The
//! host tests drive it through a source that counts bytes consumed and assert
//! the count exactly.
//!
//! Being free of `alloc`, `unsafe`, and any syscall surface makes the whole
//! core host-testable: `cargo test -p slopos-shell-core` runs natively (the
//! `terminal-core` / `keymap-core` pattern), while the shell links it and
//! supplies a descriptor-backed [`ByteSource`].

#![no_std]
#![forbid(unsafe_code)]

pub mod script;

pub use script::{ByteSource, Line, ScriptReader, SourceError};
