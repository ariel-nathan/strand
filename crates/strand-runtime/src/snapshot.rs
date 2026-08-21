//! An actor's state, lifted out of its arena so it can be put into another
//! one (§9.3, §9.4, §10.4).
//!
//! One mechanism, three uses. Hot reload restores the snapshot into a module
//! compiled from newer source; a crash report carries it, so the supervisor
//! knows what the actor believed when it died; and §10.4's "resume, do not
//! hydrate" is the same snapshot arriving from another machine. Building it
//! once for the first use is what makes the other two nearly free.
//!
//! **The runtime does not know what is in here.** Reading a value out of an
//! arena means knowing its type's layout, and a second implementation of that
//! next to the compiler's is the mistake §6.8 exists to prevent — the same
//! reason `Watch` carries bytes somebody else encoded. So the walk lives on
//! the compiler side, behind [`Snapshots`], and what comes back is an image
//! plus a list of the pointers in it. This module relocates and compares. It
//! never interprets.

use std::fmt;

use anyhow::{anyhow, Result};
use wasmtime::{Instance, Store, Val};

/// Everything an actor's state points at, as one relocatable image.
///
/// A Strand value is a tree of pointers into a bump arena, and the arena it
/// was in is about to be dropped. Rather than rebuild the tree object by
/// object in the new arena — which would need one allocator call per node —
/// the whole graph is copied into a single block whose internal pointers are
/// offsets from its own start. Restoring it is then one `strand_alloc`, one
/// write, and adding the base to the pointers listed in `relocations`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The state type, as the compiler describes it. Two snapshots are
    /// interchangeable exactly when these agree — this is §9.3's check, the
    /// one Erlang's hot code load cannot make, and it is a string comparison
    /// here only because the compiler already did the structural work.
    pub shape: String,
    /// The image: every record, list, string and boxed variant the state
    /// reaches, at 8-byte-aligned offsets from the start.
    pub bytes: Vec<u8>,
    /// Byte offsets in `bytes` of 4-byte pointer fields. Each needs the
    /// address the image lands at added to it, and nothing else does — which
    /// is why a snapshot can be moved, written to disk, or sent to another
    /// machine without knowing what any of it means (§10.4).
    pub relocations: Vec<u32>,
    /// What the state global should hold once the image is in place.
    pub root: Root,
}

/// The state global's value: a pointer into the image, or a scalar that never
/// needed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Root {
    /// An offset into [`Snapshot::bytes`].
    Pointer(u32),
    /// A `bool`, or an all-niladic sum's bare tag (§6.3).
    I32(i32),
    /// An `int`.
    I64(i64),
    /// A `float`, as bits, so a snapshot stays comparable.
    F64Bits(u64),
}

impl Snapshot {
    /// The image with every pointer in it moved to where the bytes actually
    /// landed.
    ///
    /// A relocation site holds a 4-byte pointer. That covers the `i64` payload
    /// slot of a `Result` too: the pointer was zero-extended into it (§6.2),
    /// so on a little-endian target the low four bytes are the whole of it.
    pub fn relocated(&self, base: u32) -> Vec<u8> {
        let mut bytes = self.bytes.clone();
        for site in &self.relocations {
            let at = *site as usize;
            let old = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
            bytes[at..at + 4].copy_from_slice(&(old + base).to_le_bytes());
        }
        bytes
    }

    /// What to put in the state global once the image is at `base`.
    pub fn root_value(&self, base: u32) -> Val {
        match self.root {
            Root::Pointer(offset) => Val::I32((offset + base) as i32),
            Root::I32(value) => Val::I32(value),
            Root::I64(value) => Val::I64(value),
            Root::F64Bits(bits) => Val::F64(bits),
        }
    }

    /// Whether this state can be restored into a module expecting `shape`.
    ///
    /// The whole safety argument for a swap is here. Same shape means the same
    /// offsets, so the image is a valid value in the new arena. A different
    /// shape means the record was edited as well as the code, and the honest
    /// answer is a fresh `init` rather than a reinterpretation of old bytes.
    pub fn fits(&self, shape: &str) -> bool {
        self.shape == shape
    }
}

/// Reads an actor's state out of its arena.
///
/// Implemented on the compiler side, where the layout is known. The parallel
/// with [`crate::Frames`] is exact: the runtime knows *where* the state is and
/// hands over the bytes; what they mean stays on the other side of the trait.
pub trait Snapshots: Send + Sync + fmt::Debug {
    /// `memory` is the actor's whole arena and `root` is its `strand_state`
    /// global, which is where every value it holds is reachable from.
    fn capture(&self, memory: &[u8], root: Val) -> anyhow::Result<Snapshot>;

    /// The shape a module compiled from this source expects, for the check in
    /// [`Snapshot::fits`].
    fn shape(&self) -> &str;
}

/// Puts a snapshot into a freshly instantiated actor, in place of its `init`.
///
/// One allocation and one write, whatever the state holds: the image is a
/// block of bytes whose only outside references are the pointers named in
/// `relocations`, and the address it lands at is the only thing that was
/// unknown until now.
///
/// The caller must have checked [`Snapshot::fits`] first. Restoring an image
/// into a module whose state has a different shape would be reading old bytes
/// as a new type — the one thing §9.3 exists to prevent.
pub fn restore<T>(
    snapshot: &Snapshot,
    store: &mut Store<T>,
    instance: &Instance,
) -> Result<()> {
    let alloc = allocator(store, instance)?;
    let base = alloc.call(&mut *store, snapshot.bytes.len() as i32)?;
    install(snapshot, store, instance, base)
}

/// The same, for the async stores actors run in. Only the allocator call
/// differs; a second copy of the rest would be a second thing to keep right.
pub async fn restore_async<T: Send>(
    snapshot: &Snapshot,
    store: &mut Store<T>,
    instance: &Instance,
) -> Result<()> {
    let alloc = allocator(store, instance)?;
    let base = alloc.call_async(&mut *store, snapshot.bytes.len() as i32).await?;
    install(snapshot, store, instance, base)
}

fn allocator<T>(
    store: &mut Store<T>,
    instance: &Instance,
) -> Result<wasmtime::TypedFunc<i32, i32>> {
    instance
        .get_typed_func::<i32, i32>(&mut *store, "strand_alloc")
        .map_err(|_| anyhow!("this module exports no `strand_alloc` to restore into"))
}

fn install<T>(
    snapshot: &Snapshot,
    store: &mut Store<T>,
    instance: &Instance,
    base: i32,
) -> Result<()> {
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| anyhow!("this module exports no memory"))?;
    memory.write(&mut *store, base as usize, &snapshot.relocated(base as u32))?;

    let global = instance
        .get_global(&mut *store, "strand_state")
        .ok_or_else(|| anyhow!("this module exports no `strand_state`"))?;
    global
        .set(&mut *store, snapshot.root_value(base as u32))
        .map_err(|error| anyhow!("the state global would not take the snapshot: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Snapshot {
        // A record at offset 0 whose one field points at something at 16.
        let mut bytes = vec![0u8; 24];
        bytes[0..4].copy_from_slice(&16u32.to_le_bytes());
        Snapshot {
            shape: "Model{n:int}".to_string(),
            bytes,
            relocations: vec![0],
            root: Root::Pointer(0),
        }
    }

    #[test]
    fn relocation_moves_the_pointers_and_nothing_else() {
        let snapshot = image();
        let moved = snapshot.relocated(4096);
        assert_eq!(u32::from_le_bytes(moved[0..4].try_into().unwrap()), 4096 + 16);
        assert_eq!(&moved[4..], &snapshot.bytes[4..], "nothing outside a site changes");
    }

    #[test]
    fn the_root_lands_where_the_image_did() {
        assert_eq!(image().root_value(4096).unwrap_i32(), 4096);
    }

    #[test]
    fn a_snapshot_only_fits_the_shape_it_came_from() {
        let snapshot = image();
        assert!(snapshot.fits("Model{n:int}"));
        assert!(!snapshot.fits("Model{n:int,done:bool}"));
    }
}
