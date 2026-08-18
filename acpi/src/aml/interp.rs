//! A tightly-scoped AML method evaluator — not a general interpreter. It runs
//! one device's `_STA`/`_INI` against that device's local objects and the
//! globals they reach, with only the integer arithmetic and package indexing
//! those helpers need.

use slopos_ostd::{KBTreeMap, KVec};

use super::AmlHost;
use super::object::{AmlVal, bytes_from_slice, nameseg_key};
use super::parse::*;

/// A `Create*Field` window of `width` bytes at `byte_index` into `source`.
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

/// Integers only — the methods on this path need nothing wider.
#[derive(Clone, Copy)]
struct Frame {
    args: [u64; 7],
    locals: [u64; 8],
}

const MAX_CALL_DEPTH: usize = 16;

pub struct Interp<'a> {
    aml: &'a [u8],
    /// Device-local named objects.
    pub locals: KBTreeMap<u32, AmlVal>,
    pub overlays: KBTreeMap<u32, Overlay>,
    fields: &'a KBTreeMap<u32, FieldLoc>,
    /// Name key → declared arg count.
    methods: &'a KBTreeMap<u32, u8>,
    method_bodies: &'a KBTreeMap<u32, Range>,
    /// Name key → its data-object position.
    names: &'a KBTreeMap<u32, usize>,
    host: &'a dyn AmlHost,
    returned: bool,
    ret_value: Option<AmlVal>,
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

    /// Test-only: element count if the global `Name` resolves to a `Package`.
    #[doc(hidden)]
    pub fn resolve_pkg_len_for_test(&mut self, name: &[u8; 4]) -> Option<usize> {
        let &pos = self.names.get(&nameseg_key(name))?;
        match self.eval(pos)?.0 {
            AmlVal::Package(elems) => Some(elems.len()),
            _ => None,
        }
    }

    /// Test-only.
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

    /// Saves and restores the caller's return latch so a nested call cannot
    /// clobber it.
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

    /// Resets the return latch, so one `Interp` — and its accumulated
    /// device-local state — can run several method bodies in sequence.
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

    /// A statement is a Type1 control-flow op, a Type2 expression run for its
    /// side effect (ASL `Local = expr` compiles to `Op(.., Target)`), or a
    /// NamedObj declaration, which this evaluator structurally skips.
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

    /// For ops carrying a Target operand after the two sources: evaluates it
    /// and applies the result.
    fn binary_t(&mut self, p: usize, f: impl Fn(u64, u64) -> u64) -> Option<(AmlVal, usize)> {
        let (a, q) = self.eval(p + 1)?;
        let (b, q2) = self.eval(q)?;
        let res = f(a.as_int(), b.as_int());
        let q3 = self.consume_target(q2, res)?;
        Some((AmlVal::Int(res), q3))
    }

    /// Target may be NullName, a Local, an Arg, or a Name.
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

    /// The `Index` ObjectReference destination, which this evaluator does not
    /// model, so the operand is skipped without applying a value.
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

    /// Resolution is by object kind: the namespace here is flat, so it cannot
    /// distinguish a name that two scopes define differently.
    fn eval_name(&mut self, p: usize) -> Option<(AmlVal, usize)> {
        let (seg, mut q) = name_string(self.aml, p)?;
        let key = match seg {
            Some(s) => nameseg_key(&s),
            None => return Some((AmlVal::Int(0), q)),
        };
        // Device-local objects win: they are the buffers `_INI` patches.
        if let Some(v) = self.locals.get(&key) {
            return Some((v.clone_val(), q));
        }
        if let Some(&floc) = self.fields.get(&key) {
            return Some((AmlVal::Int(self.read_field(&floc)), q));
        }
        // A `Package`/`Buffer` global `Name` preempts a same-named method: a
        // value-position reference wants the data object, not an invocation.
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
            // An `External` method whose body was never indexed returns 0.
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
        // Fallback: a scalar global `Name`'s data object, else 0.
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
