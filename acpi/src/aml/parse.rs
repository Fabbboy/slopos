//! Low-level AML byte parsing primitives plus a tolerant structural walker.
//! Container bounds come from `PkgLength`, so bailing out of one term list
//! never corrupts a sibling subtree; every function returns `Option`, and a
//! malformed DSDT degrades to "found less" rather than to a crash.

/// Half-open byte range `[start, end)` within an AML blob.
#[derive(Clone, Copy)]
pub struct Range {
    pub start: usize,
    pub end: usize,
}

pub const OP_ZERO: u8 = 0x00;
pub const OP_ONE: u8 = 0x01;
pub const OP_ALIAS: u8 = 0x06;
pub const OP_NAME: u8 = 0x08;
pub const OP_BYTE_PREFIX: u8 = 0x0a;
pub const OP_WORD_PREFIX: u8 = 0x0b;
pub const OP_DWORD_PREFIX: u8 = 0x0c;
pub const OP_STRING_PREFIX: u8 = 0x0d;
pub const OP_QWORD_PREFIX: u8 = 0x0e;
pub const OP_SCOPE: u8 = 0x10;
pub const OP_BUFFER: u8 = 0x11;
pub const OP_PACKAGE: u8 = 0x12;
pub const OP_VAR_PACKAGE: u8 = 0x13;
pub const OP_METHOD: u8 = 0x14;
pub const OP_EXTERNAL: u8 = 0x15;
pub const OP_DUAL_NAME: u8 = 0x2e;
pub const OP_MULTI_NAME: u8 = 0x2f;
pub const OP_EXT_PREFIX: u8 = 0x5b;
pub const OP_ROOT_CHAR: u8 = 0x5c;
pub const OP_PARENT_CHAR: u8 = 0x5e;
pub const OP_CREATE_DWORD_FIELD: u8 = 0x8a;
pub const OP_CREATE_WORD_FIELD: u8 = 0x8b;
pub const OP_CREATE_BYTE_FIELD: u8 = 0x8c;
pub const OP_CREATE_BIT_FIELD: u8 = 0x8d;
pub const OP_CREATE_QWORD_FIELD: u8 = 0x8f;
pub const OP_IF: u8 = 0xa0;
pub const OP_ELSE: u8 = 0xa1;
pub const OP_WHILE: u8 = 0xa2;
pub const OP_NOOP: u8 = 0xa3;
pub const OP_RETURN: u8 = 0xa4;
pub const OP_ONES: u8 = 0xff;

// Extended opcodes (preceded by 0x5b).
pub const EXT_MUTEX: u8 = 0x01;
pub const EXT_EVENT: u8 = 0x02;
pub const EXT_COND_REF_OF: u8 = 0x12;
pub const EXT_CREATE_FIELD: u8 = 0x13;
pub const EXT_OP_REGION: u8 = 0x80;
pub const EXT_FIELD: u8 = 0x81;
pub const EXT_DEVICE: u8 = 0x82;
pub const EXT_PROCESSOR: u8 = 0x83;
pub const EXT_POWER_RES: u8 = 0x84;
pub const EXT_THERMAL_ZONE: u8 = 0x85;
pub const EXT_INDEX_FIELD: u8 = 0x86;
pub const EXT_BANK_FIELD: u8 = 0x87;

pub const REGION_SYSTEM_MEMORY: u8 = 0x00;

#[inline]
fn is_lead_name_char(b: u8) -> bool {
    b == b'_' || b.is_ascii_uppercase()
}

#[inline]
fn is_name_char(b: u8) -> bool {
    is_lead_name_char(b) || b.is_ascii_digit()
}

/// Returns `(total_len, after_len_pos)`. `total_len` counts the PkgLength
/// field bytes themselves, so the enclosing package ends at `p + total_len`.
pub fn pkg_length(aml: &[u8], p: usize) -> Option<(usize, usize)> {
    let lead = *aml.get(p)?;
    let extra = (lead >> 6) as usize;
    if extra == 0 {
        return Some(((lead & 0x3f) as usize, p + 1));
    }
    let mut len = (lead & 0x0f) as usize;
    for i in 0..extra {
        let b = *aml.get(p + 1 + i)? as usize;
        len |= b << (4 + 8 * i);
    }
    Some((len, p + 1 + extra))
}

/// Yields the final 4-byte NameSeg, or `None` for a NullName.
pub fn name_string(aml: &[u8], mut p: usize) -> Option<(Option<[u8; 4]>, usize)> {
    if aml.get(p).copied() == Some(OP_ROOT_CHAR) {
        p += 1;
    } else {
        while aml.get(p).copied() == Some(OP_PARENT_CHAR) {
            p += 1;
        }
    }
    match aml.get(p).copied()? {
        OP_ZERO => Some((None, p + 1)), // NullName
        OP_DUAL_NAME => {
            let s1 = read_seg(aml, p + 1)?;
            let s2 = read_seg(aml, p + 5)?;
            let _ = s1;
            Some((Some(s2), p + 9))
        }
        OP_MULTI_NAME => {
            let count = *aml.get(p + 1)? as usize;
            if count == 0 {
                return Some((None, p + 2));
            }
            let mut last = [0u8; 4];
            for i in 0..count {
                last = read_seg(aml, p + 2 + i * 4)?;
            }
            Some((Some(last), p + 2 + count * 4))
        }
        b if is_lead_name_char(b) => {
            let seg = read_seg(aml, p)?;
            Some((Some(seg), p + 4))
        }
        _ => None,
    }
}

fn read_seg(aml: &[u8], p: usize) -> Option<[u8; 4]> {
    let s = aml.get(p..p + 4)?;
    let seg = [s[0], s[1], s[2], s[3]];
    if !is_lead_name_char(seg[0]) || !seg[1..].iter().all(|&c| is_name_char(c)) {
        return None;
    }
    Some(seg)
}

/// Little-endian.
fn read_uint(aml: &[u8], p: usize, n: usize) -> Option<(u64, usize)> {
    let bytes = aml.get(p..p + n)?;
    let mut v = 0u64;
    for (i, &b) in bytes.iter().enumerate() {
        v |= (b as u64) << (8 * i);
    }
    Some((v, p + n))
}

/// `None` unless the TermArg is a plain integer literal.
pub fn const_integer(aml: &[u8], p: usize) -> Option<(u64, usize)> {
    match *aml.get(p)? {
        OP_ZERO => Some((0, p + 1)),
        OP_ONE => Some((1, p + 1)),
        OP_ONES => Some((u64::MAX, p + 1)),
        OP_BYTE_PREFIX => read_uint(aml, p + 1, 1),
        OP_WORD_PREFIX => read_uint(aml, p + 1, 2),
        OP_DWORD_PREFIX => read_uint(aml, p + 1, 4),
        OP_QWORD_PREFIX => read_uint(aml, p + 1, 8),
        _ => None,
    }
}

/// Handles the forms that appear as `Name` values and as `OperationRegion` /
/// `Create*Field` operands; anything else yields `None` and the caller bails.
pub fn skip_term_arg(aml: &[u8], p: usize) -> Option<usize> {
    let op = *aml.get(p)?;
    match op {
        OP_ZERO | OP_ONE | OP_ONES => Some(p + 1),
        // LocalN (0x60..=0x67) / ArgN (0x68..=0x6e): a single opcode byte.
        0x60..=0x6e => Some(p + 1),
        OP_BYTE_PREFIX => Some(p + 2),
        OP_WORD_PREFIX => Some(p + 3),
        OP_DWORD_PREFIX => Some(p + 5),
        OP_QWORD_PREFIX => Some(p + 9),
        OP_STRING_PREFIX => {
            let mut q = p + 1;
            while *aml.get(q)? != 0 {
                q += 1;
            }
            Some(q + 1)
        }
        OP_BUFFER | OP_PACKAGE | OP_VAR_PACKAGE => {
            let (total, _) = pkg_length(aml, p + 1)?;
            Some(p + 1 + total)
        }
        // Logical operators: two operands, no target.
        0x90 | 0x91 | 0x93 | 0x94 | 0x95 => {
            let q = skip_term_arg(aml, p + 1)?;
            skip_term_arg(aml, q)
        }
        // Arithmetic / bitwise: two operands plus a Target, often NullName.
        0x72 | 0x74 | 0x77 | 0x79 | 0x7a | 0x7b | 0x7c | 0x7d | 0x7e | 0x7f => {
            let q = skip_term_arg(aml, p + 1)?;
            let q = skip_term_arg(aml, q)?;
            skip_term_arg(aml, q)
        }
        0x92 | 0x83 => skip_term_arg(aml, p + 1), // LNot / DerefOf
        // Unary operators with a Target.
        0x80..=0x82 | 0x99 => {
            let q = skip_term_arg(aml, p + 1)?;
            skip_term_arg(aml, q)
        }
        OP_EXT_PREFIX if aml.get(p + 1).copied() == Some(EXT_COND_REF_OF) => {
            // CondRefOf(Source, optional Target)
            let (_, q) = name_string(aml, p + 2)?;
            if aml.get(q).copied() == Some(OP_ZERO) {
                Some(q + 1)
            } else {
                Some(q)
            }
        }
        OP_ROOT_CHAR | OP_PARENT_CHAR | OP_DUAL_NAME | OP_MULTI_NAME => {
            skip_name_or_known_call(aml, p)
        }
        b if is_lead_name_char(b) => skip_name_or_known_call(aml, p),
        _ => None,
    }
}

fn method_arg_hint(seg: [u8; 4]) -> u8 {
    if seg == *b"PC2M" {
        1
    } else if seg == *b"GMIO" {
        2
    } else if seg == *b"_OSI" {
        1
    } else {
        0
    }
}

fn skip_name_or_known_call(aml: &[u8], p: usize) -> Option<usize> {
    let (seg, mut q) = name_string(aml, p)?;
    if let Some(seg) = seg {
        for _ in 0..method_arg_hint(seg) {
            q = skip_term_arg(aml, q)?;
        }
    }
    Some(q)
}

fn integer_prefix(aml: &[u8], p: usize) -> bool {
    matches!(
        aml.get(p).copied(),
        Some(
            OP_ZERO
                | OP_ONE
                | OP_ONES
                | OP_BYTE_PREFIX
                | OP_WORD_PREFIX
                | OP_DWORD_PREFIX
                | OP_QWORD_PREFIX
        )
    )
}

/// Some firmware writes the base as a method call such as `PC2M(_ADR())`, and
/// `skip_term_arg` has no arity index so it stops at the bare NameString; this
/// consumes one extra argument when that makes the next token the length.
fn skip_op_region_base(aml: &[u8], p: usize) -> Option<usize> {
    let q = skip_term_arg(aml, p)?;
    if integer_prefix(aml, q) {
        return Some(q);
    }
    let q2 = skip_term_arg(aml, q)?;
    if integer_prefix(aml, q2) {
        return Some(q2);
    }
    Some(q)
}

pub struct FieldElem {
    pub seg: [u8; 4],
    pub bit_offset: u32,
    pub bit_width: u32,
}

/// `f` receives each named field with its running bit offset.
pub fn walk_field_list(aml: &[u8], start: usize, end: usize, mut f: impl FnMut(FieldElem)) {
    let mut p = start;
    let mut bit_off: u32 = 0;
    while p < end {
        match aml.get(p).copied() {
            Some(0x00) => {
                // ReservedField: 0x00 PkgLength(width)
                let Some((w, q)) = pkg_length(aml, p + 1) else {
                    return;
                };
                bit_off = bit_off.wrapping_add(w as u32);
                p = q;
            }
            Some(0x01) => {
                // AccessField: 0x01 AccessType AccessAttrib
                p += 3;
            }
            Some(0x02) => {
                // ConnectField: 0x02 (NameString | BufferData) — bail rather
                // than decode; no field of interest sits past a connection.
                return;
            }
            Some(0x03) => {
                // ExtendedAccessField: 0x03 AccessType AccessAttrib AccessLength
                p += 4;
            }
            Some(b) if is_lead_name_char(b) => {
                let Some(seg) = read_seg(aml, p) else {
                    return;
                };
                let Some((w, q)) = pkg_length(aml, p + 4) else {
                    return;
                };
                f(FieldElem {
                    seg,
                    bit_offset: bit_off,
                    bit_width: w as u32,
                });
                bit_off = bit_off.wrapping_add(w as u32);
                p = q;
            }
            _ => return,
        }
    }
}

/// Invoked by [`walk_terms`] for each declaration of interest.
pub trait Visitor {
    fn method(&mut self, _seg: [u8; 4], _argc: u8, _body: Range) {}
    /// Only for `External(..., MethodObj, argc)`.
    fn external_method(&mut self, _seg: [u8; 4], _argc: u8) {}
    /// `_value` spans the data object.
    fn name(&mut self, _seg: [u8; 4], _value: Range) {}
    fn op_region(&mut self, _seg: [u8; 4], _space: u8, _base: u64, _len: u64) {}
    /// A field declared inside `Field(region, …)`.
    fn field(&mut self, _region: [u8; 4], _elem: &FieldElem) {}
    /// `_width_bytes` is 1/2/4/8, or 0 for a `CreateBitField`.
    fn create_field(
        &mut self,
        _source: [u8; 4],
        _byte_index: u64,
        _width_bytes: u8,
        _name: [u8; 4],
    ) {
    }
    /// Return `true` to descend into the device body.
    fn enter_device(&mut self, _seg: [u8; 4], _body: Range) -> bool {
        true
    }
    fn exit_device(&mut self) {}
}

#[inline(never)]
fn walk_predicated_pkg<V: Visitor>(
    aml: &[u8],
    p: usize,
    end: usize,
    v: &mut V,
    consume_else: bool,
) -> Option<usize> {
    let (total, after_len) = pkg_length(aml, p + 1)?;
    let node_end = (p + 1) + total;
    let body_start = skip_term_arg(aml, after_len)?;
    if body_start <= node_end {
        walk_terms(aml, body_start, node_end.min(end), v);
    }
    let mut next = node_end;

    // Walk the Else body too: firmware wraps top-level namespace declarations
    // in conditionals, and this walker is not an evaluator.
    if consume_else && aml.get(next).copied() == Some(OP_ELSE) {
        let (else_total, else_body) = pkg_length(aml, next + 1)?;
        let else_end = (next + 1) + else_total;
        walk_terms(aml, else_body, else_end.min(end), v);
        next = else_end;
    }
    Some(next)
}

#[inline(never)]
fn walk_plain_pkg<V: Visitor>(aml: &[u8], p: usize, end: usize, v: &mut V) -> Option<usize> {
    let (total, body_start) = pkg_length(aml, p + 1)?;
    let node_end = (p + 1) + total;
    walk_terms(aml, body_start, node_end.min(end), v);
    Some(node_end)
}

#[inline(never)]
fn walk_scope<V: Visitor>(aml: &[u8], p: usize, end: usize, v: &mut V) -> Option<usize> {
    let (total, after_len) = pkg_length(aml, p + 1)?;
    let node_end = (p + 1) + total;
    let (_, after_name) = name_string(aml, after_len)?;
    walk_terms(aml, after_name, node_end.min(end), v);
    Some(node_end)
}

#[inline(never)]
fn walk_name<V: Visitor>(aml: &[u8], p: usize, v: &mut V) -> Option<usize> {
    let (seg, after_name) = name_string(aml, p + 1)?;
    let val_start = after_name;
    let val_end = skip_term_arg(aml, val_start)?;
    if let Some(seg) = seg {
        v.name(
            seg,
            Range {
                start: val_start,
                end: val_end,
            },
        );
    }
    Some(val_end)
}

#[inline(never)]
fn walk_method<V: Visitor>(aml: &[u8], p: usize, end: usize, v: &mut V) -> Option<usize> {
    let (total, after_len) = pkg_length(aml, p + 1)?;
    let node_end = (p + 1) + total;
    let (seg, after_name) = name_string(aml, after_len)?;
    let flags = *aml.get(after_name).unwrap_or(&0);
    if let Some(seg) = seg {
        v.method(
            seg,
            flags & 0x07,
            Range {
                start: after_name + 1,
                end: node_end.min(end),
            },
        );
    }
    Some(node_end)
}

#[inline(never)]
fn walk_external<V: Visitor>(aml: &[u8], p: usize, v: &mut V) -> Option<usize> {
    let (seg, after_name) = name_string(aml, p + 1)?;
    let obj_type = *aml.get(after_name).unwrap_or(&0);
    let argc = *aml.get(after_name + 1).unwrap_or(&0);
    if obj_type == 0x08 {
        if let Some(seg) = seg {
            v.external_method(seg, argc);
        }
    }
    Some(after_name + 2)
}

#[inline(never)]
fn skip_alias(aml: &[u8], p: usize) -> Option<usize> {
    let (_, q) = name_string(aml, p + 1)?;
    let (_, q2) = name_string(aml, q)?;
    Some(q2)
}

#[inline(never)]
fn walk_create_field<V: Visitor>(aml: &[u8], p: usize, op: u8, v: &mut V) -> Option<usize> {
    let width = match op {
        OP_CREATE_BIT_FIELD => 0,
        OP_CREATE_BYTE_FIELD => 1,
        OP_CREATE_WORD_FIELD => 2,
        OP_CREATE_DWORD_FIELD => 4,
        _ => 8,
    };
    let (src, after_src) = name_string(aml, p + 1)?;
    let (idx, after_idx) = if let Some((idx, after_idx)) = const_integer(aml, after_src) {
        (Some(idx), after_idx)
    } else if let Some(after_idx) = skip_term_arg(aml, after_src) {
        (None, after_idx)
    } else {
        return None;
    };
    let (name, after_name) = name_string(aml, after_idx)?;
    if let (Some(src), Some(name), Some(idx)) = (src, name, idx) {
        v.create_field(src, idx, width, name);
    }
    Some(after_name)
}

#[inline(never)]
fn walk_ext<V: Visitor>(aml: &[u8], p: usize, end: usize, v: &mut V) -> Option<usize> {
    let ext = *aml.get(p + 1)?;
    match ext {
        EXT_DEVICE => {
            let (total, after_len) = pkg_length(aml, p + 2)?;
            let node_end = (p + 2) + total;
            let (seg, after_name) = name_string(aml, after_len)?;
            if let Some(seg) = seg {
                let body = Range {
                    start: after_name,
                    end: node_end.min(end),
                };
                if v.enter_device(seg, body) {
                    walk_terms(aml, after_name, node_end.min(end), v);
                    v.exit_device();
                }
            }
            Some(node_end)
        }
        EXT_OP_REGION => {
            let (seg, after_name) = name_string(aml, p + 2)?;
            let space = *aml.get(after_name).unwrap_or(&0xff);
            let base_p = after_name + 1;
            let const_base = const_integer(aml, base_p);
            let after_base = const_base
                .map(|(_, q)| q)
                .or_else(|| skip_op_region_base(aml, base_p))?;
            let const_len = const_integer(aml, after_base);
            let after_len = const_len
                .map(|(_, q)| q)
                .or_else(|| skip_term_arg(aml, after_base))?;
            if let (Some(seg), Some((base, _)), Some((len, _))) = (seg, const_base, const_len) {
                v.op_region(seg, space, base, len);
            }
            Some(after_len)
        }
        EXT_FIELD => {
            let (total, after_len) = pkg_length(aml, p + 2)?;
            let node_end = (p + 2) + total;
            let (region, after_name) = name_string(aml, after_len)?;
            let list_start = after_name + 1;
            if let Some(region) = region {
                walk_field_list(aml, list_start, node_end.min(end), |e| {
                    v.field(region, &e);
                });
            }
            Some(node_end)
        }
        EXT_PROCESSOR | EXT_POWER_RES | EXT_THERMAL_ZONE | EXT_INDEX_FIELD | EXT_BANK_FIELD => {
            let (total, _) = pkg_length(aml, p + 2)?;
            Some((p + 2) + total)
        }
        EXT_MUTEX => {
            let (_, q) = name_string(aml, p + 2)?;
            Some(q + 1)
        }
        EXT_EVENT => {
            let (_, q) = name_string(aml, p + 2)?;
            Some(q)
        }
        EXT_CREATE_FIELD => {
            let q1 = skip_term_arg(aml, p + 2)?;
            let q2 = skip_term_arg(aml, q1)?;
            let q3 = skip_term_arg(aml, q2)?;
            let (_, q4) = name_string(aml, q3)?;
            Some(q4)
        }
        _ => None,
    }
}

/// Real DSDTs nest ~15–25 deep; the cap bounds recursive-descent stack use
/// against buggy firmware so the walk cannot overflow the boot stack.
const MAX_WALK_DEPTH: u32 = 96;
static WALK_DEPTH: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

struct WalkDepthGuard;
impl Drop for WalkDepthGuard {
    fn drop(&mut self) {
        WALK_DEPTH.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
    }
}

/// Reuses the structural walkers purely for their end-position arithmetic.
struct NopVisitor;
impl Visitor for NopVisitor {
    fn enter_device(&mut self, _seg: [u8; 4], _body: Range) -> bool {
        false
    }
}

/// The executor's fallback for a statement it cannot evaluate: bounds the term
/// structurally so a method body stays live across a declaration this narrow
/// evaluator cannot model. `None` if the term cannot be bounded.
pub fn skip_statement(aml: &[u8], p: usize) -> Option<usize> {
    let op = *aml.get(p)?;
    let mut nop = NopVisitor;
    let end = aml.len();
    match op {
        OP_NAME => walk_name(aml, p, &mut nop),
        OP_ALIAS => skip_alias(aml, p),
        OP_SCOPE => walk_scope(aml, p, end, &mut nop),
        OP_METHOD => walk_method(aml, p, end, &mut nop),
        OP_EXTERNAL => walk_external(aml, p, &mut nop),
        OP_CREATE_DWORD_FIELD
        | OP_CREATE_WORD_FIELD
        | OP_CREATE_BYTE_FIELD
        | OP_CREATE_QWORD_FIELD
        | OP_CREATE_BIT_FIELD => walk_create_field(aml, p, op, &mut nop),
        OP_EXT_PREFIX => walk_ext(aml, p, end, &mut nop),
        OP_NOOP => Some(p + 1),
        0xa5 => Some(p + 1), // Break
        0x9f => Some(p + 1), // Continue
        _ => None,
    }
}

/// Descends `Scope`, and `Device` when the visitor allows.
pub fn walk_terms(aml: &[u8], start: usize, end: usize, v: &mut impl Visitor) {
    let depth = WALK_DEPTH.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let _guard = WalkDepthGuard;
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    let mut p = start;
    while p < end {
        let op = match aml.get(p) {
            Some(&b) => b,
            None => return,
        };
        match op {
            OP_IF => {
                let Some(next) = walk_predicated_pkg(aml, p, end, v, true) else {
                    return;
                };
                p = next;
            }
            OP_ELSE => {
                let Some(next) = walk_plain_pkg(aml, p, end, v) else {
                    return;
                };
                p = next;
            }
            OP_WHILE => {
                let Some(next) = walk_predicated_pkg(aml, p, end, v, false) else {
                    return;
                };
                p = next;
            }
            OP_NOOP => {
                p += 1;
            }
            OP_RETURN => {
                let Some(q) = skip_term_arg(aml, p + 1) else {
                    return;
                };
                p = q;
            }
            0x70 => {
                // Store(Source, Target): only declarations are collected, so a
                // store inside a namespace-level conditional is skipped.
                let Some(q) = skip_term_arg(aml, p + 1) else {
                    return;
                };
                let Some(q) = skip_term_arg(aml, q) else {
                    return;
                };
                p = q;
            }
            OP_SCOPE => {
                let Some(next) = walk_scope(aml, p, end, v) else {
                    return;
                };
                p = next;
            }
            OP_NAME => {
                let Some(next) = walk_name(aml, p, v) else {
                    return;
                };
                p = next;
            }
            OP_METHOD => {
                let Some(next) = walk_method(aml, p, end, v) else {
                    return;
                };
                p = next;
            }
            OP_EXTERNAL => {
                let Some(next) = walk_external(aml, p, v) else {
                    return;
                };
                p = next;
            }
            OP_ALIAS => {
                let Some(next) = skip_alias(aml, p) else {
                    return;
                };
                p = next;
            }
            OP_CREATE_DWORD_FIELD
            | OP_CREATE_WORD_FIELD
            | OP_CREATE_BYTE_FIELD
            | OP_CREATE_QWORD_FIELD => {
                let Some(next) = walk_create_field(aml, p, op, v) else {
                    return;
                };
                p = next;
            }
            OP_CREATE_BIT_FIELD => {
                let Some(next) = walk_create_field(aml, p, op, v) else {
                    return;
                };
                p = next;
            }
            OP_EXT_PREFIX => {
                let Some(next) = walk_ext(aml, p, end, v) else {
                    return;
                };
                p = next;
            }
            _ => return,
        }
    }
}
