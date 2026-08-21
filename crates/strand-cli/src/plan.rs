//! Turning a checked file into a supervision tree the runtime can spawn (§7).
//!
//! The `app` block says which actors run and which port meets which; this
//! turns that into ids, modules and a routing table. It is a plain function of
//! the `Hir` so that the wiring can be tested without opening a window — the
//! same reason `frame::decode` is separate from the compositor.
//!
//! A file with one actor and no `app` block is an app of one actor with no
//! wires. That is not a special case bolted on: it is what the general shape
//! degenerates to, so `strand view counter.str` and `strand view todo_demo.str`
//! go down one path.

use anyhow::{anyhow, bail, Result};
use strand_runtime::{ActorId, Wiring};
use strandc::hir::{Hir, Ty};
use strandc::input;

/// One actor about to run.
pub struct Spawn {
    pub id: ActorId,
    /// What the `app` block calls it — the name a crash report should use, so
    /// that two instances of one actor are still tellable apart.
    pub name: String,
    /// Index into `Hir::actors`.
    pub actor: usize,
    pub wasm: Vec<u8>,
    /// Indexed by the actor's out-port number.
    pub out: Vec<Option<Wiring>>,
}

pub struct Plan {
    pub spawns: Vec<Spawn>,
    /// Which spawn draws the window, if any.
    pub ui: Option<usize>,
    /// The UI actor's port carrying the platform's `Input` type.
    pub input_port: Option<u32>,
}

/// Builds the tree, compiling one module per actor.
pub fn plan(hir: &Hir) -> Result<Plan> {
    let instances: Vec<(String, usize)> = match &hir.app {
        Some(app) => app.instances.iter().map(|i| (i.name.clone(), i.actor)).collect(),
        None => match hir.actors.len() {
            0 => bail!("this file declares no actors"),
            1 => vec![(hir.actors[0].name.clone(), 0)],
            // Several actors and nothing saying which run or how they are
            // joined. The file is halfway to an app, so say what is missing.
            _ => bail!(
                "this file declares {} actors but no `app` block, so nothing says \
                 which of them run or how they are wired — add `app Name {{ ... }}`",
                hir.actors.len()
            ),
        },
    };

    let mut spawns = Vec::new();
    for (index, (name, actor)) in instances.iter().enumerate() {
        let wasm = strandc::codegen::emit_actor(hir, *actor).map_err(|e| anyhow!("{e}"))?;
        let mut out = vec![None; hir.actors[*actor].outbox.len()];
        if let Some(app) = &hir.app {
            for wire in app.wires.iter().filter(|w| w.from == index) {
                out[wire.from_port] =
                    Some(Wiring { to: wire.to as ActorId, port: wire.to_port as u32 });
            }
        }
        spawns.push(Spawn { id: index as ActorId, name: name.clone(), actor: *actor, wasm, out });
    }

    // Exactly one actor may draw: the compositor paints one tree, so a second
    // would silently win or silently lose depending on arrival order. Better to
    // refuse than to pick.
    let drawing: Vec<usize> = spawns
        .iter()
        .enumerate()
        .filter(|(_, spawn)| hir.actors[spawn.actor].view.is_some())
        .map(|(index, _)| index)
        .collect();
    if drawing.len() > 1 {
        let names: Vec<&str> = drawing.iter().map(|i| spawns[*i].name.as_str()).collect();
        bail!(
            "{} both draw, and the window shows one tree — give the others their \
             panels as `view fn` items the drawing actor calls",
            names.join(" and ")
        );
    }
    let ui = drawing.first().copied();

    let input_port = ui.and_then(|index| {
        let actor = &hir.actors[spawns[index].actor];
        actor.inbox.iter().position(|port| is_input(hir, &port.ty)).map(|port| port as u32)
    });

    Ok(Plan { spawns, ui, input_port })
}

/// Whether a port carries the platform's own event type (`docs/abi.md` §9).
fn is_input(hir: &Hir, ty: &Ty) -> bool {
    matches!(ty, Ty::Sum(id) if hir.sums[id.0 as usize].name == input::TYPE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(src: &str) -> Hir {
        match strandc::compile("t.str", src) {
            Ok(hir) => hir,
            Err(report) => panic!("{:?}", miette::Report::new(report)),
        }
    }

    const PAIR: &str = "\
type Count = { total: int }
type Bump = | Tick

actor Source {
  state: Count
  in  input: Input
  out bumps: Bump
  fn init(): Count { Count { total: 0 } }
  on input(state: Count, msg: Input): Count {
    send(bumps, Tick)
    state
  }
  view fn draw(state: Count): Node { text(\"hi\") }
}

actor Sink {
  state: Count
  in bumps: Bump
  fn init(): Count { Count { total: 0 } }
  on bumps(state: Count, msg: Bump): Count { Count { total: state.total + 1 } }
}

app Pair {
  source = Source
  sink = Sink
  source.bumps -> sink.bumps
}
";

    #[test]
    fn a_wire_becomes_a_destination_and_a_port() {
        let plan = plan(&compile(PAIR)).expect("a plan");
        assert_eq!(plan.spawns.len(), 2);
        // The source's only out port leads to the sink's only in port.
        assert_eq!(plan.spawns[0].out, vec![Some(Wiring { to: 1, port: 0 })]);
        // The sink sends nowhere, so it has no table at all.
        assert!(plan.spawns[1].out.is_empty());
    }

    #[test]
    fn the_actor_that_draws_is_the_one_the_window_follows() {
        let plan = plan(&compile(PAIR)).expect("a plan");
        assert_eq!(plan.ui, Some(0));
        assert_eq!(plan.input_port, Some(0), "its Input port, by type not by name");
    }

    #[test]
    fn each_actor_gets_its_own_module() {
        let plan = plan(&compile(PAIR)).expect("a plan");
        // Same source, different entry points: the two modules cannot be the
        // same bytes, or one of them would be running the other's handlers.
        assert_ne!(plan.spawns[0].wasm, plan.spawns[1].wasm);
        for spawn in &plan.spawns {
            wasmparser::validate(&spawn.wasm).expect("valid WASM");
        }
    }

    #[test]
    fn a_lone_actor_needs_no_app_block() {
        let hir = compile(
            "type Count = { total: int }
             actor Solo {
               state: Count
               in input: Input
               fn init(): Count { Count { total: 0 } }
               on input(state: Count, msg: Input): Count { state }
               view fn draw(state: Count): Node { text(\"hi\") }
             }",
        );
        let plan = plan(&hir).expect("a plan");
        assert_eq!(plan.spawns.len(), 1);
        assert_eq!(plan.ui, Some(0));
    }

    #[test]
    fn several_actors_without_an_app_block_is_refused_by_name() {
        let hir = compile(
            "type Count = { total: int }
             actor A {
               state: Count
               fn init(): Count { Count { total: 0 } }
             }
             actor B {
               state: Count
               fn init(): Count { Count { total: 0 } }
             }",
        );
        let message = plan(&hir).err().expect("refused").to_string();
        assert!(message.contains("`app Name"), "{message}");
    }
}
