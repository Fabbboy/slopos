//! `DiagnosticSink` impl that writes raw lines to the platform console.
//!
//! Routes through the `platform::console_puts` facade rather than
//! `slopos-utils::klog` because `slopos-utils` depends on
//! `slopos-kernel-services`; pulling it in here would form a cycle.

use slopos_ostd::irq::idt::DiagnosticSink;

use crate::platform;

pub struct ConsoleDiagnosticSink;

pub static CONSOLE_SINK: ConsoleDiagnosticSink = ConsoleDiagnosticSink;

impl DiagnosticSink for ConsoleDiagnosticSink {
    fn emit(&self, line: &str) {
        platform::console_puts(line.as_bytes());
        platform::console_puts(b"\n");
    }
}
