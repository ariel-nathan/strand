//! Running a view function written in Strand (§6.2, §10's M3 second half).
//!
//! §10 asks for the UI tree to be submitted "from a host-side actor first, then
//! from Strand code". `todo.rs` is the first half. This is the second: the tree
//! is built by compiled Strand, and the host's part is to call the function,
//! read the frame it left behind, and hand it to the same compositor.
//!
//! What crosses the boundary is a flat array in the guest's own memory. No host
//! call, no serialisation, no allocation on the way out — `frame::decode` reads
//! the bytes the view wrote.

use anyhow::{anyhow, Result};
use wasmtime::error::Context as _;
use strand_render::scene::Node;
use strand_render::widgets::Theme;
use strandc::hir::Hir;
use wasmtime::{Engine, Instance, Store, Val};

use crate::frame;

/// A view compiled and instantiated, ready to be asked for a frame.
pub struct View {
    store: Store<()>,
    instance: Instance,
    name: String,
}

/// Says what is missing and what to do about it.
///
/// The mistake this exists for: reaching for `strand view` on a program that
/// is not a view. Naming the functions the module *does* have turns "no" into
/// "not this one", which is the difference between a dead end and a next step.
fn no_view(hir: &Hir) -> String {
    let views: Vec<&str> = hir
        .funcs
        .iter()
        .filter(|func| func.is_view)
        .map(|func| func.name.as_str())
        .collect();

    if !views.is_empty() {
        return format!(
            "every view here takes arguments ({}), so none of them can be the \
             one to draw. `strand view` calls a view that needs nothing — add a \
             `view fn main() -> Node` that supplies the arguments.",
            views.join(", ")
        );
    }

    let functions: Vec<&str> =
        hir.funcs.iter().map(|func| func.name.as_str()).take(4).collect();
    let has = if functions.is_empty() {
        String::new()
    } else {
        format!(" This module defines {}.", functions.join(", "))
    };
    format!(
        "this file has no view to draw.{has} A view is written \
         `view fn main() -> Node {{ ... }}` — see examples/strand/view.str. \
         (The todo app is its own command: `strand todo`.)"
    )
}

impl View {
    /// Instantiates `wasm` and finds the view to call.
    ///
    /// A module may hold several views — §6.2's `todoRow` is one — so the entry
    /// point is the one that needs nothing to draw itself.
    pub fn new(hir: &Hir, wasm: &[u8]) -> Result<Self> {
        let entry = hir
            .funcs
            .iter()
            .find(|func| func.is_view && func.param_count == 0)
            .ok_or_else(|| anyhow!("{}", no_view(hir)))?;

        let engine = Engine::default();
        let module =
            wasmtime::Module::new(&engine, wasm).context("wasmtime rejected the module")?;
        let mut store = Store::new(&engine, ());
        let instance =
            Instance::new(&mut store, &module, &[]).context("instantiation failed")?;

        Ok(Self { store, instance, name: entry.name.clone() })
    }

    /// Calls the view and rebuilds the tree it emitted.
    pub fn frame(&mut self, theme: &Theme) -> Result<Node> {
        let reset = self
            .instance
            .get_typed_func::<(), ()>(&mut self.store, "strand_frame_reset")
            .context("the module exports no frame arena")?;
        reset.call(&mut self.store, ()).context("resetting the frame arena failed")?;

        let view = self
            .instance
            .get_func(&mut self.store, &self.name)
            .ok_or_else(|| anyhow!("`{}` is not exported", self.name))?;
        // A view returns nothing: building the nodes *was* the result.
        view.call(&mut self.store, &[], &mut [])
            .with_context(|| format!("trap while running `{}`", self.name))?;

        let base = self.global("strand_nodes")?;
        let count = self.global("strand_node_count")?;
        let memory = self
            .instance
            .get_memory(&mut self.store, "memory")
            .ok_or_else(|| anyhow!("the module exports no memory"))?;

        frame::decode(theme, memory.data(&self.store), base, count)
    }

    fn global(&mut self, name: &str) -> Result<u32> {
        let global = self
            .instance
            .get_global(&mut self.store, name)
            .ok_or_else(|| anyhow!("the module exports no `{name}`"))?;
        match global.get(&mut self.store) {
            Val::I32(value) => Ok(value as u32),
            other => Err(anyhow!("`{name}` is {other:?}, expected an i32")),
        }
    }
}

/// Compiles `hir`/`wasm` and returns the laid-out tree as text (§8.4).
///
/// The way to check a Strand view without a screen: it reports where every node
/// actually ended up, measured with the font that would draw it.
pub fn inspect(hir: &Hir, wasm: &[u8], viewport: (f32, f32)) -> Result<String> {
    let tree = View::new(hir, wasm)?.frame(&Theme::default())?;
    Ok(strand_render::inspect::describe_with_fonts(&tree, viewport))
}

/// Opens a window showing what a Strand view drew.
///
/// The view is called once: with no actor there is no state to change, so there
/// is nothing to redraw for. `strand todo` is where a view meets a mailbox.
pub fn run(hir: &Hir, wasm: &[u8]) -> Result<()> {
    let tree = View::new(hir, wasm)?.frame(&Theme::default())?;

    let (scenes, receiver) = strand_render::compositor::scene_channel();
    scenes.submit(tree);

    println!("--- strand M3: a view written in Strand (§6.2) ---");
    println!("press F12 for the inspector overlay (§8.4)");
    strand_render::run_with(Some(receiver), None)
}
