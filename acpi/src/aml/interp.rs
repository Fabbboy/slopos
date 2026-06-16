//! A tightly-scoped AML method evaluator.
//!
//! Not a general AML interpreter. It runs a single device's `_STA`/`_INI`
//! against that device's local objects (its `Name`s and `Create*Field`
//! overlays) and the globals they reach: `SystemMemory` fields, `Name` tables
//! (`Package`s), and helper methods. It evaluates the integer arithmetic and
//! package indexing those helpers use (method calls, `Arg`/`Local`, `Add`/
//! `And`/shifts, `Index`/`DerefOf`).
//!
//! After `_INI` runs, the caller re-reads the device's resource-template
//! buffer — which `_INI` has patched with the I²C slave address and the
//! GpioInt pin — with no per-machine constants.

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

/// A method-call activation: its arguments and locals (integers only — the
/// resource methods on this path do integer arithmetic and package indexing).
#[derive(Clone, Copy)]
struct Frame {
    args: [u64; 7],
    locals: [u64; 8],
}

/// Maximum method-call nesting.
const MAX_CALL_DEPTH: usize = 16;

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
    /// Method name → body range (for evaluating calls).
    method_bodies: &'a KBTreeMap<u32, Range>,
    /// Global `Name` → its data-object position (for `Package` tables).
    names: &'a KBTreeMap<u32, usize>,
    host: &'a dyn AmlHost,
    returned: bool,
    ret_value: Option<AmlVal>,
    /// Active call frames (Arg/Local storage).
    frames: KVec<Frame>,
    /// Recursion / iteration guard.
    budget: u32,
}

impl<'a> Interp<'a> {
    pub fn new(
        aml: &'a [u8],
        fields: &'a KBTreeMap<u32, FieldLoc>,
        methods: &'a KBTreeMap<u32, u8>,
        method_bodies: &'a KBTreeMap<u32, Range>,
        names: &'a KBTreeMap<u32, usize>,
        host: &'a dyn AmlHost,
    ) -> Self {
        Self {
            aml,
            locals: KBTreeMap::new(),
            overlays: KBTreeMap::new(),
            fields,
            methods,
            method_bodies,
            names,
            host,
            returned: false,
            ret_value: None,
            frames: KVec::new(),
            budget: 100_000,
        }
    }

    /// Test-only: resolve a global `Name` and, if it's a `Package`, return its
    /// element count (else `None`). Confirms package-table resolution.
    #[doc(hidden)]
    pub fn resolve_pkg_len_for_test(&mut self, name: &[u8; 4]) -> Option<usize> {
        let &pos = self.names.get(&nameseg_key(name))?;
        match self.eval(pos)?.0 {
            AmlVal::Package(elems) => Some(elems.len()),
            _ => None,
        }
    }

    /// Test-only: invoke a method by name with integer args, returning the
    /// integer it returns.
    #[doc(hidden)]
    pub fn invoke_for_test(&mut self, method: &[u8; 4], args: &[u64]) -> Option<u64> {
        let &body = self.method_bodies.get(&nameseg_key(method))?;
        let mut a = [0u64; 7];
        for (slot, &v) in a.iter_mut().zip(args.iter()).take(7) {
            *slot = v;
        }
        Some(self.call(body, a).as_int())
    }

    fn arg(&self, i: usize) -> u64 {
        match self.frames.last() {
            Some(f) if i < 7 => f.args[i],
            _ => 0,
        }
    }

    fn local(&self, i: usize) -> u64 {
        match self.frames.last() {
            Some(f) if i < 8 => f.locals[i],
            _ => 0,
        }
    }

    fn set_local(&mut self, i: usize, v: u64) {
        if let Some(f) = self.frames.last_mut() {
            if i < 8 {
                f.locals[i] = v;
            }
        }
    }

    fn set_arg(&mut self, i: usize, v: u64) {
        if let Some(f) = self.frames.last_mut() {
            if i < 7 {
                f.args[i] = v;
            }
        }
    }

    /// Invoke a method body with `args` bound, returning its `Return` value
    /// (or `Int(0)`). Saves/restores the caller's return latch so nested calls
    /// don't clobber it.
    fn call(&mut self, body: Range, args: [u64; 7]) -> AmlVal {
        if self.frames.len() >= MAX_CALL_DEPTH
            || self
                .frames
                .push(Frame {
                    args,
                    locals: [0; 8],
                })
                .is_err()
        {
            return AmlVal::Int(0);
        }
        let saved_returned = core::mem::replace(&mut self.returned, false);
        let saved_ret = self.ret_value.take();
        self.exec_list(body.start, body.end);
        let result = self.ret_value.take().unwrap_or(AmlVal::Int(0));
        self.returned = saved_returned;
        self.ret_value = saved_ret;
        self.frames.pop();
        result
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
        // Base frame so any top-level Local/Arg references resolve.
        let pushed = self
            .frames
            .push(Frame {
                args: [0; 7],
                locals: [0; 8],
            })
            .is_ok();
        self.exec_list(start, end);
        if pushed {
            self.frames.pop();
        }
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

    /// Execute one statement (an AML `TermObj` in statement position),
    /// returning the position after it.
    ///
    /// A statement is one of three `TermObj` kinds, all handled here:
    /// * **Type1** — control flow with no value (`If`/`Else`/`While`/`Return`/
    ///   `Noop`).
    /// * **Type2** — an expression legal as a statement, run for its side
    ///   effect with the value discarded: a `Store`, a method call, or an
    ///   arithmetic/bitwise op writing its `Target` operand (ASL `Local = expr`
    ///   compiles to `Op(.., Target)`).
    /// * **NamedObj** — a declaration (`OperationRegion`/`Field`/`Create*Field`/
    ///   `Name`). Dynamic regions are beyond this evaluator, so the declaration
    ///   is structurally skipped and execution continues.
    fn exec_stmt(&mut self, p: usize, end: usize) -> Option<usize> {
        let op = *self.aml.get(p)?;
        match op {
            0x70 => {
                // Store(Source, Target) — Target may be a Local/Arg or a Name.
                let (val, q) = self.eval(p + 1)?;
                let tb = *self.aml.get(q)?;
                match tb {
                    0x60..=0x67 => {
                        self.set_local((tb - 0x60) as usize, val.as_int());
                        Some(q + 1)
                    }
                    0x68..=0x6e => {
                        self.set_arg((tb - 0x68) as usize, val.as_int());
                        Some(q + 1)
                    }
                    _ => {
                        let (target, q2) = name_string(self.aml, q)?;
                        if let Some(seg) = target {
                            self.store(nameseg_key(&seg), val);
                        }
                        Some(q2)
                    }
                }
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
            // Type2 expression (arithmetic/bitwise op writing its Target, or a
            // method call): evaluate for the side effect, discard the value.
            // A declaration this evaluator can't model is structurally skipped.
            _ => match self.eval(p) {
                Some((_v, q)) if q > p => Some(q),
                _ => skip_statement(self.aml, p),
            },
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
            // Bitwise / arithmetic ops carry a Target operand after the two
            // sources; `binary_t` evaluates it and applies the result.
            0x7b => self.binary_t(p, |a, b| a & b), // And
            0x7d => self.binary_t(p, |a, b| a | b), // Or
            0x79 => self.binary_t(p, |a, b| a.wrapping_shl(b as u32)), // ShiftLeft
            0x7a => self.binary_t(p, |a, b| a.wrapping_shr(b as u32)), // ShiftRight
            0x72 => self.binary_t(p, |a, b| a.wrapping_add(b)), // Add
            0x74 => self.binary_t(p, |a, b| a.wrapping_sub(b)), // Subtract
            0x77 => self.binary_t(p, |a, b| a.wrapping_mul(b)), // Multiply
            0x99 => self.eval(p + 1),               // ToInteger(x) — pass-through for our ints
            0x83 => self.eval(p + 1),               // DerefOf(x) — Index already yields the value
            0x88 => self.eval_index(p),             // Index(source, index, target)
            OP_PACKAGE | OP_VAR_PACKAGE => self.eval_package(p),
            0x68..=0x6e => Some((AmlVal::Int(self.arg((op - 0x68) as usize)), p + 1)),
            0x60..=0x67 => Some((AmlVal::Int(self.local((op - 0x60) as usize)), p + 1)),
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

    /// Like [`binary`](Self::binary) but for ops that carry a Target operand
    /// (And/Or/Add/…): evaluate it and apply the result.
    fn binary_t(&mut self, p: usize, f: impl Fn(u64, u64) -> u64) -> Option<(AmlVal, usize)> {
        let (a, q) = self.eval(p + 1)?;
        let (b, q2) = self.eval(q)?;
        let res = f(a.as_int(), b.as_int());
        let q3 = self.consume_target(q2, res)?;
        Some((AmlVal::Int(res), q3))
    }

    /// Parse a Target operand and store `res` into it (null/Local/Arg/Name).
    fn consume_target(&mut self, q: usize, res: u64) -> Option<usize> {
        let tb = *self.aml.get(q)?;
        match tb {
            0x00 => Some(q + 1), // NullName
            0x60..=0x67 => {
                self.set_local((tb - 0x60) as usize, res);
                Some(q + 1)
            }
            0x68..=0x6e => {
                self.set_arg((tb - 0x68) as usize, res);
                Some(q + 1)
            }
            b if is_lead_name_char_pub(b)
                || b == OP_ROOT_CHAR
                || b == OP_PARENT_CHAR
                || b == OP_DUAL_NAME
                || b == OP_MULTI_NAME =>
            {
                let (target, q2) = name_string(self.aml, q)?;
                if let Some(seg) = target {
                    self.store(nameseg_key(&seg), AmlVal::Int(res));
                }
                Some(q2)
            }
            _ => None,
        }
    }

    /// Skip a Target operand without applying a value (the `Index`
    /// ObjectReference destination, which this evaluator doesn't model).
    fn skip_target(&self, q: usize) -> Option<usize> {
        let tb = *self.aml.get(q)?;
        match tb {
            0x00 | 0x60..=0x6e => Some(q + 1),
            b if is_lead_name_char_pub(b)
                || b == OP_ROOT_CHAR
                || b == OP_PARENT_CHAR
                || b == OP_DUAL_NAME
                || b == OP_MULTI_NAME =>
            {
                let (_t, q2) = name_string(self.aml, q)?;
                Some(q2)
            }
            _ => None,
        }
    }

    /// `Index(Source, Index, Target)` → `Source[Index]` by value.
    fn eval_index(&mut self, p: usize) -> Option<(AmlVal, usize)> {
        let (src, q) = self.eval(p + 1)?;
        let (idx, q2) = self.eval(q)?;
        let q3 = self.skip_target(q2)?;
        let i = idx.as_int() as usize;
        let elem = match &src {
            AmlVal::Package(elems) => elems
                .get(i)
                .map(|e| e.clone_val())
                .unwrap_or(AmlVal::Int(0)),
            AmlVal::Buf(b) => AmlVal::Int(*b.get(i).unwrap_or(&0) as u64),
            _ => AmlVal::Int(0),
        };
        Some((elem, q3))
    }

    /// `Package`/`VarPackage` → an ordered list of evaluated elements.
    fn eval_package(&mut self, p: usize) -> Option<(AmlVal, usize)> {
        let (total, after_len) = pkg_length(self.aml, p + 1)?;
        let pkg_end = (p + 1) + total;
        // NumElements: a ByteData for `Package` (0x12), a TermArg for
        // `VarPackage` (0x13).
        let mut q = if *self.aml.get(p)? == OP_PACKAGE {
            after_len + 1
        } else {
            self.eval(after_len)?.1
        };
        let mut elems = KVec::new();
        while q < pkg_end {
            if !self.tick() {
                break;
            }
            let (v, nq) = self.eval(q)?;
            if nq <= q {
                break;
            }
            let _ = elems.push(v);
            q = nq;
        }
        Some((AmlVal::Package(elems), pkg_end))
    }

    /// Resolve a NameString in value position: a device-local value, a field
    /// read, a global `Name`, or a method call. Resolution is by object kind
    /// (the namespace here is flat, so it can't scope-resolve a name that two
    /// scopes define differently).
    fn eval_name(&mut self, p: usize) -> Option<(AmlVal, usize)> {
        let (seg, mut q) = name_string(self.aml, p)?;
        let key = match seg {
            Some(s) => nameseg_key(&s),
            None => return Some((AmlVal::Int(0), q)),
        };
        // Device-local objects (the resource buffers `_INI` patches) win.
        if let Some(v) = self.locals.get(&key) {
            return Some((v.clone_val(), q));
        }
        if let Some(&floc) = self.fields.get(&key) {
            return Some((AmlVal::Int(self.read_field(&floc)), q));
        }
        // A `Package`/`Buffer` global `Name` preempts a same-named method: a
        // value-position reference (e.g. `Index` into a pad-info table) wants
        // the data object, not a method invocation.
        if let Some(&pos) = self.names.get(&key) {
            if matches!(
                self.aml.get(pos),
                Some(&OP_PACKAGE) | Some(&OP_VAR_PACKAGE) | Some(&OP_BUFFER)
            ) {
                if let Some((v, _)) = self.eval(pos) {
                    return Some((v, q));
                }
            }
        }
        if let Some(&argc) = self.methods.get(&key) {
            // Method invocation: evaluate `argc` arguments, then run the body
            // (if indexed; an `External` method with no body returns 0).
            let mut args = [0u64; 7];
            for (i, slot) in args.iter_mut().enumerate().take(argc as usize) {
                let (a, nq) = self.eval(q)?;
                q = nq;
                if i < 7 {
                    *slot = a.as_int();
                }
            }
            let ret = match self.method_bodies.get(&key) {
                Some(&body) => self.call(body, args),
                None => AmlVal::Int(0),
            };
            return Some((ret, q));
        }
        // A scalar global `Name`, or an untracked name (e.g. `OSYS`): its data
        // object, else `0`.
        if let Some(&pos) = self.names.get(&key) {
            if let Some((v, _)) = self.eval(pos) {
                return Some((v, q));
            }
        }
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
