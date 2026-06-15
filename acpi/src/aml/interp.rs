//! A tightly-scoped AML method evaluator.
//!
//! This is *not* a general AML interpreter. It executes a single device's
//! `_INI` method against that device's local objects (its `Name`s and
//! `Create*Field` overlays) plus the global `SystemMemory` `Field`s the
//! method reads. Calls to methods outside the device (e.g. GPIO-library
//! helpers) are stubbed to `0`; their results only feed the GpioInt pin
//! number, which the polled path ignores.
//!
//! After `_INI` runs, the caller re-reads the device's resource-template
//! buffer (which `_INI` has patched with the I²C slave address) to obtain
//! the address — no per-machine constants.

use slopos_ostd::{KBTreeMap, KVec};

use super::AmlHost;
use super::object::{AmlVal, bytes_from_slice, nameseg_key};
use super::parse::*;

/// A `Create*Field` overlay: a named window of `width` bytes at
/// `byte_index` into the buffer named `source`.
#[derive(Clone, Copy)]
pub struct Overlay {
    pub source: u32,
    pub byte_index: u64,
    pub width: u8,
}

/// A `SystemMemory` field's physical location.
#[derive(Clone, Copy)]
pub struct FieldLoc {
    pub region_base: u64,
    pub region_space: u8,
    pub bit_offset: u32,
    pub bit_width: u32,
}

/// Evaluator over a single device's `_INI`.
pub struct Interp<'a> {
    aml: &'a [u8],
    /// Device-local named objects (buffers, integers, strings).
    pub locals: KBTreeMap<u32, AmlVal>,
    /// `Create*Field` overlays into local buffers.
    pub overlays: KBTreeMap<u32, Overlay>,
    /// Global `SystemMemory` fields (e.g. `TPTY`).
    fields: &'a KBTreeMap<u32, FieldLoc>,
    /// Method name → declared arg count (for parsing invocations).
    methods: &'a KBTreeMap<u32, u8>,
    host: &'a dyn AmlHost,
    returned: bool,
    ret_value: Option<AmlVal>,
    /// Recursion / iteration guard.
    budget: u32,
}

impl<'a> Interp<'a> {
    pub fn new(
        aml: &'a [u8],
        fields: &'a KBTreeMap<u32, FieldLoc>,
        methods: &'a KBTreeMap<u32, u8>,
        host: &'a dyn AmlHost,
    ) -> Self {
        Self {
            aml,
            locals: KBTreeMap::new(),
            overlays: KBTreeMap::new(),
            fields,
            methods,
            host,
            returned: false,
            ret_value: None,
            budget: 100_000,
        }
    }

    fn tick(&mut self) -> bool {
        if self.budget == 0 {
            return false;
        }
        self.budget -= 1;
        true
    }

    /// Run a method body term list `[start, end)`, returning the value it
    /// `Return`ed (if any). Resets the return latch so the same `Interp`
    /// (and its accumulated device-local state) can run several methods.
    pub fn run(&mut self, start: usize, end: usize) -> Option<AmlVal> {
        self.returned = false;
        self.ret_value = None;
        self.exec_list(start, end);
        self.ret_value.take()
    }

    fn exec_list(&mut self, start: usize, end: usize) {
        let mut p = start;
        while p < end && !self.returned {
            if !self.tick() {
                return;
            }
            match self.exec_stmt(p, end) {
                Some(next) if next > p => p = next,
                _ => return,
            }
        }
    }

    /// Execute one statement, returning the position after it.
    fn exec_stmt(&mut self, p: usize, end: usize) -> Option<usize> {
        let op = *self.aml.get(p)?;
        match op {
            0x70 => {
                // Store(Source, Target)
                let (val, q) = self.eval(p + 1)?;
                let (target, q2) = name_string(self.aml, q)?;
                if let Some(seg) = target {
                    self.store(nameseg_key(&seg), val);
                }
                Some(q2)
            }
            0xa0 => {
                // If(Predicate) { ... }
                let (total, after_len) = pkg_length(self.aml, p + 1)?;
                let node_end = (p + 1) + total;
                let (pred, body_start) = self.eval(after_len)?;
                if pred.as_int() != 0 {
                    self.exec_list(body_start, node_end.min(end));
                }
                // Optional trailing Else.
                let mut next = node_end;
                if self.aml.get(node_end).copied() == Some(0xa1) {
                    let (etotal, _eafter) = pkg_length(self.aml, node_end + 1)?;
                    let else_end = (node_end + 1) + etotal;
                    if pred.as_int() == 0 {
                        let (_etot, ebody) = pkg_length(self.aml, node_end + 1)?;
                        self.exec_list(ebody, else_end.min(end));
                    }
                    next = else_end;
                }
                Some(next)
            }
            0xa1 => {
                // Stray Else (already consumed by If); skip it.
                let (total, _) = pkg_length(self.aml, p + 1)?;
                Some((p + 1) + total)
            }
            0xa2 => {
                // While — not used on the enumeration path; skip its body.
                let (total, _) = pkg_length(self.aml, p + 1)?;
                Some((p + 1) + total)
            }
            0xa4 => {
                // Return(Arg)
                let (v, q) = self.eval(p + 1)?;
                self.ret_value = Some(v);
                self.returned = true;
                Some(q)
            }
            0xa3 => Some(p + 1), // Noop
            b if is_lead_name_char_pub(b)
                || b == OP_ROOT_CHAR
                || b == OP_PARENT_CHAR
                || b == OP_DUAL_NAME
                || b == OP_MULTI_NAME =>
            {
                // A bare method-invocation statement (result discarded).
                let (_v, q) = self.eval(p)?;
                Some(q)
            }
            _ => None,
        }
    }

    /// Evaluate a TermArg starting at `p`, returning `(value, next_pos)`.
    fn eval(&mut self, p: usize) -> Option<(AmlVal, usize)> {
        if !self.tick() {
            return None;
        }
        let op = *self.aml.get(p)?;
        match op {
            OP_ZERO => Some((AmlVal::Int(0), p + 1)),
            OP_ONE => Some((AmlVal::Int(1), p + 1)),
            OP_ONES => Some((AmlVal::Int(u64::MAX), p + 1)),
            OP_BYTE_PREFIX => Some((AmlVal::Int(*self.aml.get(p + 1)? as u64), p + 2)),
            OP_WORD_PREFIX => {
                let (v, q) = self.read_le(p + 1, 2)?;
                Some((AmlVal::Int(v), q))
            }
            OP_DWORD_PREFIX => {
                let (v, q) = self.read_le(p + 1, 4)?;
                Some((AmlVal::Int(v), q))
            }
            OP_QWORD_PREFIX => {
                let (v, q) = self.read_le(p + 1, 8)?;
                Some((AmlVal::Int(v), q))
            }
            OP_STRING_PREFIX => {
                let mut q = p + 1;
                let mut s = KVec::new();
                while *self.aml.get(q)? != 0 {
                    let _ = s.push(self.aml[q]);
                    q += 1;
                }
                Some((AmlVal::Str(s), q + 1))
            }
            OP_BUFFER => {
                let (total, after_len) = pkg_length(self.aml, p + 1)?;
                let buf_end = (p + 1) + total;
                let (_size, data_start) = self.eval(after_len)?;
                let bytes = self.aml.get(data_start..buf_end)?;
                Some((AmlVal::Buf(bytes_from_slice(bytes)), buf_end))
            }
            // Logical operators (predicates).
            0x93 => self.binary(p, |a, b| (a == b) as u64), // LEqual
            0x94 => self.binary(p, |a, b| (a > b) as u64),  // LGreater
            0x95 => self.binary(p, |a, b| (a < b) as u64),  // LLess
            0x90 => self.binary(p, |a, b| ((a != 0) && (b != 0)) as u64), // LAnd
            0x91 => self.binary(p, |a, b| ((a != 0) || (b != 0)) as u64), // LOr
            0x92 => {
                // LNot(Operand)
                let (v, q) = self.eval(p + 1)?;
                Some((AmlVal::Int((v.as_int() == 0) as u64), q))
            }
            0x7b => self.binary(p, |a, b| a & b), // And
            0x7d => self.binary(p, |a, b| a | b), // Or
            0x79 => self.binary(p, |a, b| a.wrapping_shl(b as u32)), // ShiftLeft
            0x7a => self.binary(p, |a, b| a.wrapping_shr(b as u32)), // ShiftRight
            0x99 => self.eval(p + 1),             // ToInteger(x) — pass-through for our ints
            // Arg0..6 / Local0..7 — stubbed to 0 (we don't run arg-taking methods).
            0x68..=0x6e => Some((AmlVal::Int(0), p + 1)),
            0x60..=0x67 => Some((AmlVal::Int(0), p + 1)),
            OP_ROOT_CHAR | OP_PARENT_CHAR | OP_DUAL_NAME | OP_MULTI_NAME => self.eval_name(p),
            b if is_lead_name_char_pub(b) => self.eval_name(p),
            _ => None,
        }
    }

    fn binary(&mut self, p: usize, f: impl Fn(u64, u64) -> u64) -> Option<(AmlVal, usize)> {
        let (a, q) = self.eval(p + 1)?;
        let (b, q2) = self.eval(q)?;
        Some((AmlVal::Int(f(a.as_int(), b.as_int())), q2))
    }

    /// Resolve a NameString in value position: a method call, a field
    /// read, or a local value.
    fn eval_name(&mut self, p: usize) -> Option<(AmlVal, usize)> {
        let (seg, mut q) = name_string(self.aml, p)?;
        let key = match seg {
            Some(s) => nameseg_key(&s),
            None => return Some((AmlVal::Int(0), q)),
        };
        if let Some(&argc) = self.methods.get(&key) {
            // Method invocation: consume `argc` arguments, return 0.
            for _ in 0..argc {
                let (_a, nq) = self.eval(q)?;
                q = nq;
            }
            return Some((AmlVal::Int(0), q));
        }
        if let Some(&floc) = self.fields.get(&key) {
            return Some((AmlVal::Int(self.read_field(&floc)), q));
        }
        if let Some(v) = self.locals.get(&key) {
            return Some((v.clone_val(), q));
        }
        // Unknown name (e.g. a global integer like OSYS we don't track):
        // treat as 0. No args consumed.
        Some((AmlVal::Int(0), q))
    }

    fn read_le(&self, p: usize, n: usize) -> Option<(u64, usize)> {
        let bytes = self.aml.get(p..p + n)?;
        let mut v = 0u64;
        for (i, &b) in bytes.iter().enumerate() {
            v |= (b as u64) << (8 * i);
        }
        Some((v, p + n))
    }

    fn read_field(&self, loc: &FieldLoc) -> u64 {
        if loc.region_space != REGION_SYSTEM_MEMORY || loc.bit_width == 0 || loc.bit_width > 64 {
            return 0;
        }
        // Read the byte span covering the field (bit-granular within bytes).
        let start_byte = loc.region_base + (loc.bit_offset / 8) as u64;
        let bit_in_byte = loc.bit_offset % 8;
        let total_bits = bit_in_byte + loc.bit_width;
        let nbytes = ((total_bits + 7) / 8) as usize;
        let mut raw = [0u8; 16];
        if nbytes > raw.len() {
            return 0;
        }
        self.host.read_phys(start_byte, &mut raw[..nbytes]);
        let mut acc: u128 = 0;
        for i in 0..nbytes {
            acc |= (raw[i] as u128) << (8 * i);
        }
        acc >>= bit_in_byte;
        let mask: u128 = if loc.bit_width == 64 {
            u64::MAX as u128
        } else {
            (1u128 << loc.bit_width) - 1
        };
        (acc & mask) as u64
    }

    fn store(&mut self, target: u32, val: AmlVal) {
        if let Some(&ov) = self.overlays.get(&target) {
            // Write an integer into the source buffer at the overlay window.
            let v = val.as_int();
            if let Some(AmlVal::Buf(buf)) = self.locals.get_mut(&ov.source) {
                let base = ov.byte_index as usize;
                let width = if ov.width == 0 { 1 } else { ov.width as usize };
                for i in 0..width {
                    if let Some(slot) = buf.get_mut(base + i) {
                        *slot = (v >> (8 * i)) as u8;
                    }
                }
            }
            return;
        }
        self.locals.insert(target, val);
    }
}

#[inline]
fn is_lead_name_char_pub(b: u8) -> bool {
    b == b'_' || b.is_ascii_uppercase()
}
