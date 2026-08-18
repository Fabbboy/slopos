//! Pure, allocation-free shell input core.
//!
//! A shell shares its input descriptor with the commands it runs, so
//! [`ScriptReader`] frames lines out of a [`ByteSource`] one byte per call: it
//! consumes the line it returns and not one byte more.

#![no_std]
#![forbid(unsafe_code)]

pub mod script;

pub use script::{ByteSource, Line, ScriptReader, SourceError};
