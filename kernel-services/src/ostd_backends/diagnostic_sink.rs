//! `DiagnosticSink` writing raw lines to the platform console. Routed through
//! `platform::console_puts` rather than `slopos-utils::klog` because
//! `slopos-utils` depends on this crate, so klog here would form a cycle.

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
