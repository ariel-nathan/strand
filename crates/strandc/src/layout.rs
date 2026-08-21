//! How a Strand value is laid out in an actor's arena (§6.2–§6.6).
//!
//! This is the one table the emitter and every host-side reader work from.
//! Codegen writes these offsets, the message encoder puts them on a channel,
//! the frame decoder reads them back, and the state snapshot (§9.3) walks
//! them. A second copy of any rule here would be a byte-level disagreement
//! waiting to happen, which is the bug §6.8 exists to prevent.
//!
//! Nothing here allocates a layout or decides one at run time: a value's size
//! is a property of its type. That is what lets `send` put a constant length
//! on the wire, and what lets a snapshot be walked from a `Ty` alone.

use wasm_encoder::ValType;

use crate::hir::{RecordDef, SumDef, Ty};

/// Every value occupies whole 8-byte slots in memory, so field offsets are
/// just word counts. Simpler than tight packing and irrelevant at POC scale.
pub const WORD: u64 = 8;

/// A list is `{ i32 len, <pad>, elements... }`. The header is a whole word so
/// the elements after it stay 8-byte aligned, which is what lets an element be
/// loaded by exactly the code that loads a record field.
pub const LIST_HEADER: u64 = WORD;

/// A string is `{ i32 len, bytes... }` (§6.5).
pub const STR_HEADER: u64 = 4;

/// The WASM representation of a Strand type.
pub fn rep(ty: &Ty) -> Vec<ValType> {
    match ty {
        Ty::Int => vec![ValType::I64],
        Ty::Float => vec![ValType::F64],
        Ty::Bool => vec![ValType::I32],
        // Pointers into linear memory, and immediate tags for all-niladic sums.
        Ty::Str | Ty::List(_) | Ty::Record(_) | Ty::Sum(_) => vec![ValType::I32],
        // The multi-value pair. This is the whole point of §6.2.
        Ty::Option(_) | Ty::Result(..) => vec![ValType::I32, ValType::I64],
        // A node leaves nothing behind: building it *was* the effect. See
        // `Ty::Node` in the HIR for why that is the point rather than a saving.
        Ty::Unit | Ty::Never | Ty::Error | Ty::Node => vec![],
    }
}

/// How many WASM values a type occupies when returned. The runner needs this
/// to size a dynamic call, and it must agree with `rep`.
pub fn wasm_arity(ty: &Ty) -> usize {
    rep(ty).len()
}

/// How many 8-byte slots a value of this type takes as a field or an element.
pub fn words(ty: &Ty) -> u64 {
    rep(ty).len() as u64
}

/// Bytes one element of `elem` occupies. Whole words, like a record's fields —
/// a two-word `Result` takes two.
pub fn stride(elem: &Ty) -> u64 {
    words(elem).max(1) * WORD
}

/// Where field `index` sits inside a record, and how big the record is when
/// `index` is its field count.
pub fn field_offset(fields: &[(String, Ty)], index: usize) -> u64 {
    fields[..index].iter().map(|(_, ty)| words(ty) * WORD).sum()
}

/// Whether a sum is a bare `i32` tag rather than a pointer to one (§6.3).
pub fn is_bare_tag(def: &SumDef) -> bool {
    def.variants.iter().all(|variant| variant.fields.is_empty())
}

/// Slots the payload of a boxed sum occupies: one per field of the *widest*
/// variant, so the size of a value is a property of its type and not of its
/// tag (§6.3).
pub fn payload_words(def: &SumDef) -> u64 {
    def.variants
        .iter()
        .map(|variant| variant.fields.iter().map(|(_, ty)| words(ty)).sum::<u64>())
        .max()
        .unwrap_or(0)
}

/// Bytes a boxed sum value occupies: the tag's word, then the widest payload.
pub fn boxed_sum_size(def: &SumDef) -> u64 {
    (payload_words(def) + 1) * WORD
}

/// Bytes a record occupies.
pub fn record_size(def: &RecordDef) -> u64 {
    field_offset(&def.fields, def.fields.len())
}
