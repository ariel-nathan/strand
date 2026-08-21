//! Hot reload against a running supervision tree (§9.3).
//!
//! `tests/snapshot.rs` proves the state survives a move between two instances.
//! This is the same claim with the runtime underneath it: a live app, mid-use,
//! given a module compiled from edited source while its mailboxes, wiring and
//! frame route stay where they were. What has to come out the other side is
//! the *old* state drawn by the *new* code.
//!
//! The last two tests are the same snapshot in its second use: §9.4's crash
//! report, which the design doc has claimed carries one since M2 and which
//! only now does.
//!
//! Runs under `sim`, so the interleaving is reproducible.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_cli::app::spec_for;
use strand_cli::plan;
use strand_cli::snapshot::Codec;
use strand_cli::view::View;
use strand_render::compositor::{InputEvent, Key};
use strand_render::scene::{Command, HitId, Layouter, Node};
use strand_render::widgets::Theme;
use strand_runtime::sim::{self, SimOptions};
use strand_runtime::{
    engine, spawn_supervised, Event, Frames, Message, Policy, Registry, Trace, HOST,
};
use strandc::hir::Hir;

#[derive(Default)]
struct Captured {
    frames: Mutex<Vec<Node>>,
}

impl Frames for Captured {
    fn submit(&self, memory: &[u8], base: u32, count: u32) {
        let tree = strand_cli::frame::decode(&Theme::default(), memory, base, count)
            .expect("the actor drew a frame that will not decode");
        self.frames.lock().unwrap().push(tree);
    }
}

/// Every string a tree would draw, in paint order.
fn labels(tree: &Node) -> Vec<String> {
    let mut layouter = Layouter::new();
    layouter
        .layout(tree, (800.0, 600.0))
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("strand")
        .join("todo_demo.str");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn compile(source: &str) -> Hir {
    match strandc::compile("todo_demo.str", source) {
        Ok(hir) => hir,
        Err(report) => panic!("{:?}", miette::Report::new(report)),
    }
}

fn typing(text: &str) -> Vec<InputEvent> {
    text.chars().map(|c| InputEvent::Key { id: HitId(1), key: Key::Char(c) }).collect()
}

/// Runs the app, types `title` and adds it, swaps in `replacement`, then reads
/// the last frame drawn.
///
/// Everything the reload touches is the real thing: the same `plan` the CLI
/// builds, the same `Message::Reload` the file watcher sends, the same
/// supervisor loop that handles a crash.
fn reload_after_typing(title: &str, replacement: &str) -> (Trace, Vec<Node>) {
    reload_after(title, replacement, Vec::new())
}

/// The same, with `trailing` sent immediately after the reload and before the
/// actor has had a chance to wake up — so those events are sitting in the old
/// mailbox at the moment it is replaced.
fn reload_after(title: &str, replacement: &str, trailing: Vec<InputEvent>) -> (Trace, Vec<Node>) {
    let hir = compile(&source());
    let plan = plan::plan(&hir).expect("a plan");
    let next_hir = compile(replacement);
    let next_plan = plan::plan(&next_hir).expect("a plan for the new source");

    let ui = plan.spawns[plan.ui.expect("something draws")].id;
    let port = plan.input_port.expect("it hears input");
    let message_ty =
        hir.actors[plan.spawns[plan.ui.unwrap()].actor].inbox[port as usize].ty.clone();
    let captured = Arc::new(Captured::default());
    let sink = captured.clone();

    let events: Vec<InputEvent> = typing(title)
        .into_iter()
        .chain([InputEvent::Key { id: HitId(1), key: Key::Enter }])
        .collect();

    let trace = sim::run(SimOptions::new(1), move |registry: Registry| {
        let hir = hir.clone();
        let next_hir = next_hir.clone();
        let next_plan = Arc::new(next_plan);
        async move {
            registry.route_frames(ui, sink);
            for spawn in &plan.spawns {
                registry.route_out(spawn.id, spawn.out.clone());
                registry.route_watchers(spawn.id, spawn.watchers.clone());
                registry.route_state(
                    spawn.id,
                    Arc::new(Codec::new(&hir, &hir.actors[spawn.actor].state)),
                );
                registry.reserve(spawn.id);
            }
            let engine = engine()?;
            let mut handles = Vec::new();
            for spawn in &plan.spawns {
                handles.push(spawn_supervised(
                    &engine,
                    &registry,
                    spawn.id,
                    &spawn.name,
                    &spawn.wasm,
                    Policy::Restart,
                    None,
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;

            for event in events {
                let spec = spec_for(event).expect("deliverable");
                let bytes = strand_cli::encode::encode(&hir, &message_ty, &spec)
                    .unwrap_or_else(|e| panic!("encoding {spec}: {e:#}"));
                registry.send_from(HOST, ui, Message::Blob { from: HOST, port, bytes })?;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }

            // The swap. This is exactly what `watch.rs` sends when the file
            // changes on disk.
            for spawn in next_plan.spawns.iter() {
                let state = &next_hir.actors[spawn.actor].state;
                registry.send(
                    spawn.id,
                    Message::Reload {
                        bytes: spawn.wasm.clone(),
                        state: Arc::new(Codec::new(&next_hir, state)),
                    },
                )?;
            }
            // No await in between: these land behind the reload, in a mailbox
            // that is about to be thrown away.
            for event in trailing {
                let spec = spec_for(event).expect("deliverable");
                let bytes = strand_cli::encode::encode(&hir, &message_ty, &spec)
                    .unwrap_or_else(|e| panic!("encoding {spec}: {e:#}"));
                registry.send_from(HOST, ui, Message::Blob { from: HOST, port, bytes })?;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;

            for spawn in &plan.spawns {
                let _ = registry.send(spawn.id, Message::Stop);
            }
            for handle in handles {
                let _ = handle.await;
            }
            Ok::<(), anyhow::Error>(())
        }
    })
    .expect("simulation failed");

    let frames =
        Arc::try_unwrap(captured).ok().expect("the sink outlived the run").frames.into_inner();
    (trace, frames.unwrap())
}

fn last(frames: &[Node]) -> Vec<String> {
    labels(frames.last().expect("the actor drew at least once"))
}

fn reloads(trace: &Trace) -> Vec<(u32, bool)> {
    trace
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Reloaded { id, carried, .. } => Some((id, carried)),
            _ => None,
        })
        .collect()
}

#[test]
fn newer_code_comes_up_holding_the_running_state() {
    // The claim in one test. A todo typed into the running app is still there
    // after the swap, and the button label proves the code that drew it is the
    // edited one.
    let edited = source().replace("\"crash stats\"", "\"break stats\"");
    let (trace, frames) = reload_after_typing("survive the swap", &edited);
    let drawn = last(&frames);

    assert!(
        drawn.iter().any(|text| text == "survive the swap"),
        "the todo did not survive: {drawn:?}"
    );
    assert!(
        drawn.iter().any(|text| text == "break stats"),
        "the new code is not the one drawing: {drawn:?}"
    );
    assert!(
        reloads(&trace).iter().all(|(_, carried)| *carried),
        "both actors should have carried their state:\n{}",
        trace.render()
    );
}

#[test]
fn an_edited_state_record_comes_up_from_init() {
    // §9.3's check saying no. The old image would have fitted in the new
    // record — the field was added at the end — and reading it as the new type
    // would leave `pinned` holding whatever the arena had there. So the actor
    // starts from `init`, and the typed todo is gone: honest, and visibly so.
    let edited = source()
        .replace("  burning: bool,\n}", "  burning: bool,\n  pinned: int,\n}")
        .replace("      burning: false,\n    }", "      burning: false,\n      pinned: 0,\n    }");
    let (trace, frames) = reload_after_typing("this one is lost", &edited);
    let drawn = last(&frames);

    assert!(
        !drawn.iter().any(|text| text == "this one is lost"),
        "a state that cannot be read must not be guessed at: {drawn:?}"
    );
    assert!(
        drawn.iter().any(|text| text == "write the compiler"),
        "the replacement ran its own `init`: {drawn:?}"
    );
    let ui = reloads(&trace);
    assert!(
        ui.iter().any(|(id, carried)| *id == 0 && !*carried),
        "the UI actor should have started fresh:\n{}",
        trace.render()
    );
}

#[test]
fn a_reload_is_a_new_life_of_the_same_actor() {
    // Not a new actor: the same id, the same mailbox, the same row in the
    // overlay, with its generation moved on. That is what makes it a
    // supervisor restart with newer code rather than a machine of its own.
    let (trace, _) = reload_after_typing("x", &source());
    let events = trace.events();

    let spawned: Vec<u32> = events
        .iter()
        .filter_map(|event| match event {
            Event::Spawned { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(spawned, vec![0, 1, 0, 1], "each actor came up twice, under its own id");
    assert_eq!(reloads(&trace), vec![(0, true), (1, true)]);
    assert!(
        !events.iter().any(|event| matches!(event, Event::Crashed { .. })),
        "nothing crashed:\n{}",
        trace.render()
    );
}

#[test]
fn a_message_queued_behind_a_reload_is_not_lost() {
    // A keystroke that arrived while the swap was happening. It was in the old
    // mailbox, and the old life took that with it — so a reload has to carry
    // what was queued as well as what was held. Losing it would be a character
    // that vanished because the file happened to be saved at that moment.
    let (trace, frames) = reload_after("counted", &source(), typing("zz"));
    let drawn = last(&frames);
    assert!(
        drawn.iter().any(|text| text == "zz"),
        "the keystrokes behind the reload were dropped: {drawn:?}\n{}",
        trace.render()
    );
}

// ---- the same snapshot, in a crash report (§9.4) --------------------------

/// An actor that keeps something worth having and then falls over.
const FRAGILE: &str = r#"type Model = { count: int, note: string }

actor Fragile {
  state: Model
  in input: Input

  fn init(): Model { Model { count: 0, note: "start" } }

  on input(state: Model, msg: Input): Model {
    match msg {
      Typed(ch) => Model { count: state.count + 1, note: "typed" },
      Enter => panic("asked for it"),
      _ => state,
    }
  }

  view fn draw(state: Model): Node {
    screen(gap: 4, padding: 8) { text(state.note + " " + str(state.count)) }
  }
}
"#;

/// Runs `FRAGILE`, types three characters, then kills it, and hands back the
/// report the supervisor got.
fn crash_report() -> (Hir, Vec<u8>, strand_runtime::CrashReport) {
    let hir = match strandc::compile("fragile.str", FRAGILE) {
        Ok(hir) => hir,
        Err(report) => panic!("{:?}", miette::Report::new(report)),
    };
    let plan = plan::plan(&hir).expect("a plan");
    let wasm = plan.spawns[0].wasm.clone();
    let port = plan.input_port.expect("it hears input");
    let message_ty = hir.actors[plan.spawns[0].actor].inbox[port as usize].ty.clone();
    let report = Arc::new(Mutex::new(None));
    let out = report.clone();
    // Kept back for the assertions: the simulation takes ownership of its own
    // copies.
    let (compiled, module) = (hir.clone(), wasm.clone());

    sim::run(SimOptions::new(1), move |registry: Registry| {
        let hir = hir.clone();
        let wasm = wasm.clone();
        let out = out.clone();
        async move {
            registry.route_state(
                0,
                Arc::new(Codec::new(&hir, &hir.actors[0].state)),
            );
            registry.reserve(0);
            let engine = engine()?;
            // `Stop`, so the report comes back here instead of being handled
            // by a restart nobody can see.
            let handle =
                spawn_supervised(&engine, &registry, 0, "fragile", &wasm, Policy::Stop, None);
            tokio::time::sleep(Duration::from_millis(20)).await;

            for event in typing("abc").into_iter().chain([InputEvent::Key {
                id: HitId(1),
                key: Key::Enter,
            }]) {
                let spec = spec_for(event).expect("deliverable");
                let bytes = strand_cli::encode::encode(&hir, &message_ty, &spec)
                    .unwrap_or_else(|e| panic!("encoding {spec}: {e:#}"));
                registry.send_from(HOST, 0, Message::Blob { from: HOST, port, bytes })?;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }

            *out.lock().unwrap() = handle.await.expect("the task ran").err();
            Ok::<(), anyhow::Error>(())
        }
    })
    .expect("simulation failed");

    let report = report.lock().unwrap().clone().expect("it should have died");
    (compiled, module, report)
}

#[test]
fn a_crash_report_carries_what_the_actor_believed() {
    // §9.4 asks a crash report for the actor, the message in flight, and a
    // state snapshot. The first two were there; this is the third, and it is
    // the same mechanism hot reload uses rather than a second one.
    let (_, _, report) = crash_report();
    assert_eq!(report.reason, "asked for it");
    assert!(report.handling.is_some(), "the message that did it");
    let state = report.state.expect("a snapshot came with it");
    assert!(state.shape.starts_with("Model"), "{}", state.shape);
}

#[test]
fn the_state_in_a_crash_report_is_the_last_good_one() {
    // Not a half-written state: the handler that panicked never stored its
    // result, so what the report carries is the actor as it was going *into*
    // the message that killed it. Three characters typed, three counted.
    let (hir, wasm, report) = crash_report();
    let snapshot = report.state.expect("a snapshot");

    let mut fresh = View::new(&hir, &wasm).expect("a fresh instance");
    fresh.restore(&snapshot).expect("the shape is its own");
    let tree = fresh.frame(&Theme::default()).expect("a frame");

    assert!(
        labels(&tree).iter().any(|text| text == "typed 3"),
        "the state at the moment of the crash: {:?}",
        labels(&tree)
    );
}

// ---- F5: the other half of a reload ---------------------------------------

#[test]
fn a_restart_puts_the_actor_back_to_its_init() {
    // A reload keeps the state, which is the point of it — and is also why a
    // string a handler put in the state keeps its old text until that handler
    // runs again. This is the way out: same code, state let go, `init` again.
    let hir = match strandc::compile("fragile.str", FRAGILE) {
        Ok(hir) => hir,
        Err(report) => panic!("{:?}", miette::Report::new(report)),
    };
    let plan = plan::plan(&hir).expect("a plan");
    let wasm = plan.spawns[0].wasm.clone();
    let port = plan.input_port.expect("it hears input");
    let message_ty = hir.actors[plan.spawns[0].actor].inbox[port as usize].ty.clone();
    let captured = Arc::new(Captured::default());
    let sink = captured.clone();

    let trace = sim::run(SimOptions::new(1), move |registry: Registry| {
        let hir = hir.clone();
        let wasm = wasm.clone();
        let sink = sink.clone();
        async move {
            registry.route_frames(0, sink);
            registry.route_state(0, Arc::new(Codec::new(&hir, &hir.actors[0].state)));
            registry.reserve(0);
            let engine = engine()?;
            let handle =
                spawn_supervised(&engine, &registry, 0, "fragile", &wasm, Policy::Restart, None);
            tokio::time::sleep(Duration::from_millis(20)).await;

            for event in typing("abc") {
                let spec = spec_for(event).expect("deliverable");
                let bytes = strand_cli::encode::encode(&hir, &message_ty, &spec)
                    .unwrap_or_else(|e| panic!("encoding {spec}: {e:#}"));
                registry.send_from(HOST, 0, Message::Blob { from: HOST, port, bytes })?;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }

            registry.send(0, Message::Restart)?;
            tokio::time::sleep(Duration::from_millis(20)).await;
            registry.send(0, Message::Stop)?;
            let _ = handle.await;
            Ok::<(), anyhow::Error>(())
        }
    })
    .expect("simulation failed");

    let frames = captured.frames.lock().unwrap().clone();
    let before = labels(&frames[frames.len() - 2]);
    let after = labels(frames.last().expect("it drew after coming back"));
    assert_eq!(before, vec!["typed 3".to_string()], "what it had");
    assert_eq!(after, vec!["start 0".to_string()], "what `init` builds");

    assert!(
        trace.events().iter().any(|event| matches!(event, Event::Restarted { id: 0, .. })),
        "a restart is a new life of the same actor:
{}",
        trace.render()
    );
    assert!(
        !trace.events().iter().any(|event| matches!(event, Event::Crashed { .. })),
        "and not a crash:
{}",
        trace.render()
    );
}
