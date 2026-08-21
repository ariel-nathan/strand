//! Lifting an actor's state out of its arena, and saying when it may be put
//! back (§9.3, §9.4, §10.4).
//!
//! The walk is driven by the state's `Ty` and `strandc::layout`, exactly as
//! `encode.rs` and `frame.rs` are. That is the whole trick: the compiler
//! already decided where every field sits, so reading a value back out is not
//! a decoder that has to be kept in step with the emitter — it is the same
//! table read in the other direction.
//!
//! What comes out is one relocatable image (`strand_runtime::Snapshot`), not a
//! tree of host objects. A Strand value *is* a block of bytes with pointers
//! into an arena, so copying the reachable graph into one block and recording
//! where the pointers are keeps it that way. Restoring is then a single
//! `strand_alloc` and a single write, in an arena that has never seen the old
//! addresses.
//!
//! Three properties this relies on, all of them §4.2's immutability:
//!
//! - **The walk terminates.** Data is immutable and has no back-references, so
//!   the graph is acyclic. There is nothing to cycle-detect.
//! - **Sharing survives.** Two fields that point at one list are copied once
//!   and point at one list afterwards, because a source address is visited
//!   once. Immutability is what makes that safe as well as cheaper.
//! - **A crashed actor's state is still readable.** A handler that traps never
//!   wrote its result to the state global, so what is in the arena is the last
//!   good state rather than a half-finished one.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use strand_runtime::{Root, Snapshot, Snapshots};
use strandc::hir::{Hir, Ty};
use strandc::layout::{self, LIST_HEADER, STR_HEADER, WORD};
use wasmtime::Val;

mod shape;
pub use shape::{difference, shape, Shape};

/// Reads one actor's state, for the runtime to call when it needs a snapshot.
///
/// Holds the `Hir` the running module was compiled from, which is what makes
/// the snapshot self-describing: the shape travels with the bytes, so the
/// module they are restored into can be checked against them.
pub struct Codec {
    hir: Hir,
    state: Ty,
    shape: String,
}

/// Just the shape. The `Hir` behind it is a compiler's worth of detail, and a
/// `Message::Reload` holding one of these is printed in a trace.
impl std::fmt::Debug for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "state of {}", self.shape)
    }
}

impl Codec {
    pub fn new(hir: &Hir, state: &Ty) -> Self {
        Self { hir: hir.clone(), state: state.clone(), shape: shape(hir, state).to_string() }
    }
}

impl Snapshots for Codec {
    fn capture(&self, memory: &[u8], root: Val) -> Result<Snapshot> {
        capture(&self.hir, &self.state, &self.shape, memory, root)
    }

    fn shape(&self) -> &str {
        &self.shape
    }
}

/// Walks the state out of `memory`, starting from the state global's value.
pub fn capture(
    hir: &Hir,
    state: &Ty,
    shape: &str,
    memory: &[u8],
    root: Val,
) -> Result<Snapshot> {
    let mut walker = Walker { hir, memory, out: Vec::new(), relocations: Vec::new(), seen: HashMap::new() };
    let root = walker.root(state, root)?;
    Ok(Snapshot {
        shape: shape.to_string(),
        bytes: walker.out,
        relocations: walker.relocations,
        root,
    })
}

struct Walker<'a> {
    hir: &'a Hir,
    memory: &'a [u8],
    out: Vec<u8>,
    relocations: Vec<u32>,
    /// Guest address to the offset it was copied to. An address holds one
    /// object, so this both preserves sharing and bounds the walk.
    seen: HashMap<u32, u32>,
}

impl<'a> Walker<'a> {
    fn root(&mut self, ty: &Ty, root: Val) -> Result<Root> {
        Ok(match ty {
            Ty::Int => Root::I64(as_i64(root)?),
            Ty::Float => Root::F64Bits(as_f64_bits(root)?),
            Ty::Bool => Root::I32(as_i32(root)?),
            // An all-niladic sum is the tag itself, so there is nothing to
            // walk and nothing to relocate (§6.3).
            Ty::Sum(id) if layout::is_bare_tag(&self.hir.sums[id.0 as usize]) => {
                Root::I32(as_i32(root)?)
            }
            Ty::Str | Ty::List(_) | Ty::Record(_) | Ty::Sum(_) => {
                let ptr = as_i32(root)? as u32;
                if ptr == 0 {
                    // Before `strand_main` runs, the global is still zero.
                    bail!("this actor has not built its state yet");
                }
                Root::Pointer(self.object(ty, ptr)?)
            }
            // `state_type` in codegen refuses anything wider than a word, so a
            // two-word state never reaches here.
            other => bail!("a state of type {} cannot be snapshotted", self.hir.ty(other)),
        })
    }

    /// Copies the object at `ptr` into the image and returns its offset.
    fn object(&mut self, ty: &Ty, ptr: u32) -> Result<u32> {
        if let Some(offset) = self.seen.get(&ptr) {
            return Ok(*offset);
        }
        let at = match ty {
            Ty::Str => {
                let len = self.read_u32(ptr as usize)?;
                let at = self.reserve(STR_HEADER + len as u64);
                self.seen.insert(ptr, at);
                let bytes = self.slice(ptr as usize, STR_HEADER as usize + len as usize)?;
                self.out[at as usize..at as usize + bytes.len()].copy_from_slice(&bytes);
                at
            }
            Ty::List(elem) => {
                let len = self.read_u32(ptr as usize)?;
                let step = layout::stride(elem);
                let at = self.reserve(LIST_HEADER + step * len as u64);
                self.seen.insert(ptr, at);
                self.put_u32(at as usize, len);
                for index in 0..len as u64 {
                    let offset = LIST_HEADER + step * index;
                    self.slot(elem, ptr as u64 + offset, at as u64 + offset)?;
                }
                at
            }
            Ty::Record(id) => {
                let def = self.hir.records[id.0 as usize].clone();
                let at = self.reserve(layout::record_size(&def));
                self.seen.insert(ptr, at);
                for (index, (_, field)) in def.fields.iter().enumerate() {
                    let offset = layout::field_offset(&def.fields, index);
                    self.slot(field, ptr as u64 + offset, at as u64 + offset)?;
                }
                at
            }
            Ty::Sum(id) => {
                let def = self.hir.sums[id.0 as usize].clone();
                let tag = self.read_u32(ptr as usize)?;
                let variant = def.variants.get(tag as usize).ok_or_else(|| {
                    anyhow!("`{}` has no variant {tag} — the arena is not what its type says", def.name)
                })?;
                // The whole width of the type, not of this variant: a narrow
                // variant still occupies the widest one's room (§6.3), and a
                // reader is entitled to look at all of it.
                let at = self.reserve(layout::boxed_sum_size(&def));
                self.seen.insert(ptr, at);
                self.put_u32(at as usize, tag);
                let fields = variant.fields.clone();
                let mut offset = WORD;
                for (_, field) in &fields {
                    self.slot(field, ptr as u64 + offset, at as u64 + offset)?;
                    offset += layout::words(field) * WORD;
                }
                at
            }
            other => bail!("{} is not a boxed value", self.hir.ty(other)),
        };
        Ok(at)
    }

    /// Copies one field- or element-sized value from the arena into the image.
    fn slot(&mut self, ty: &Ty, from: u64, to: u64) -> Result<()> {
        match ty {
            // Zero-width: nothing was stored, so nothing is read.
            Ty::Unit | Ty::Node | Ty::Never | Ty::Error => Ok(()),
            Ty::Int | Ty::Float | Ty::Bool => self.copy_words(ty, from, to),
            Ty::Sum(id) if layout::is_bare_tag(&self.hir.sums[id.0 as usize]) => {
                self.copy_words(ty, from, to)
            }
            Ty::Str | Ty::List(_) | Ty::Record(_) | Ty::Sum(_) => {
                let ptr = self.read_u32(from as usize)?;
                // Only a value that was never built is null, and there is no
                // way to write one in Strand; carrying the zero through beats
                // inventing an object for it.
                if ptr != 0 {
                    let child = self.object(ty, ptr)?;
                    self.put_u32(to as usize, child);
                    self.relocations.push(to as u32);
                }
                Ok(())
            }
            // Two words: the tag, then §6.2's payload slot. Which type the
            // payload has depends on the tag, which is exactly the question
            // the checker answers statically at every *other* site — here the
            // value is all there is, so the tag is read.
            Ty::Option(inner) => {
                let tag = self.read_u32(from as usize)?;
                self.copy_words(ty, from, to)?;
                if tag == 0 {
                    self.payload(inner, from, to)?;
                }
                Ok(())
            }
            Ty::Result(ok, err) => {
                let tag = self.read_u32(from as usize)?;
                self.copy_words(ty, from, to)?;
                let live = if tag == 0 { ok } else { err };
                self.payload(live, from, to)
            }
        }
    }

    /// Follows the pointer a `Result`/`Option` payload slot holds, if it is
    /// one. The pointer was zero-extended into the 64-bit slot, so the low
    /// four bytes are all of it — which is why one relocation width covers
    /// every case.
    fn payload(&mut self, ty: &Ty, from: u64, to: u64) -> Result<()> {
        if !ty.is_boxed() {
            return Ok(());
        }
        if let Ty::Sum(id) = ty {
            if layout::is_bare_tag(&self.hir.sums[id.0 as usize]) {
                return Ok(());
            }
        }
        let ptr = self.read_u32((from + WORD) as usize)?;
        if ptr != 0 {
            let child = self.object(ty, ptr)?;
            self.put_u32((to + WORD) as usize, child);
            self.relocations.push((to + WORD) as u32);
        }
        Ok(())
    }

    /// Copies a value's slots verbatim. A scalar needs no interpretation: the
    /// bytes in the new arena are the bytes in the old one.
    fn copy_words(&mut self, ty: &Ty, from: u64, to: u64) -> Result<()> {
        let size = (layout::words(ty) * WORD) as usize;
        let bytes = self.slice(from as usize, size)?;
        self.out[to as usize..to as usize + size].copy_from_slice(&bytes);
        Ok(())
    }

    /// Makes room for an object, rounded up to a whole word exactly as the
    /// guest's bump allocator does.
    ///
    /// WASM permits an unaligned load, so this is not what makes the restore
    /// correct. It is what makes the image indistinguishable from memory the
    /// guest allocated itself: same sizes, same spacing, every field on its
    /// natural boundary.
    fn reserve(&mut self, size: u64) -> u32 {
        let at = self.out.len() as u32;
        let padded = size.div_ceil(WORD) * WORD;
        self.out.resize(at as usize + padded as usize, 0);
        at
    }

    fn slice(&self, at: usize, len: usize) -> Result<Vec<u8>> {
        self.memory
            .get(at..at + len)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| anyhow!("the state reaches {len} bytes at {at:#x}, past this arena"))
    }

    fn read_u32(&self, at: usize) -> Result<u32> {
        let bytes = self.slice(at, 4)?;
        Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
    }

    fn put_u32(&mut self, at: usize, value: u32) {
        self.out[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
}

fn as_i32(value: Val) -> Result<i32> {
    match value {
        Val::I32(value) => Ok(value),
        other => Err(anyhow!("the state global is {other:?}, expected an i32")),
    }
}

fn as_i64(value: Val) -> Result<i64> {
    match value {
        Val::I64(value) => Ok(value),
        other => Err(anyhow!("the state global is {other:?}, expected an i64")),
    }
}

fn as_f64_bits(value: Val) -> Result<u64> {
    match value {
        Val::F64(bits) => Ok(bits),
        other => Err(anyhow!("the state global is {other:?}, expected an f64")),
    }
}
