//! A supervision tree of Strand actors, driven end to end without a window.
//!
//! What this holds to account is the claim §7 rests on and `tests/app.rs`
//! cannot reach: that a `send` written in Strand leaves one arena and arrives
//! in another as a value the receiver can match on. Every layer is the real
//! one — the checker resolving a port name to a number, the emitter putting
//! the bytes where the host expects, the registry turning an out port into a
//! destination, and the receiving module dispatching on the port it arrived
//! on.
//!
//! Runs under `sim`, so the interleaving is reproducible rather than lucky.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_cli::app::spec_for;
use strand_cli::plan;
use strand_render::compositor::{InputEvent, Key};
use strand_render::scene::{Command, HitId, Layouter, Node};
use strand_render::widgets::Theme;
use strand_runtime::sim::{self, SimOptions};
use strand_runtime::{
    engine, spawn_supervised, Event, Frames, Message, Policy, Registry, Trace, HOST,
};
use strandc::hir::Hir;

/// Collects every frame the drawing actor produces, decoded into a tree.
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

fn example(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("strand")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn compile(name: &str) -> Hir {
    match strandc::compile(name, &example(name)) {
        Ok(hir) => hir,
        Err(report) => panic!("{:?}", miette::Report::new(report)),
    }
}

fn click(id: u32) -> InputEvent {
    InputEvent::PointerDown { id: HitId(id), x: 0.0, y: 0.0 }
}

fn typing(text: &str) -> Vec<InputEvent> {
    text.chars().map(|c| InputEvent::Key { id: HitId(1), key: Key::Char(c) }).collect()
}

/// Spawns the whole tree the file describes, feeds the UI actor `events`, and
/// hands back the trace.
fn drive(name: &str, events: Vec<InputEvent>) -> Trace {
    drive_capturing(name, events).0
}

/// The same, with every frame the UI actor drew.
fn drive_capturing(name: &str, events: Vec<InputEvent>) -> (Trace, Vec<Node>) {
    let hir = compile(name);
    let plan = plan::plan(&hir).expect("a plan");
    let ui = plan.spawns[plan.ui.expect("something draws")].id;
    let port = plan.input_port.expect("it hears input");
    let message_ty = hir.actors[plan.spawns[plan.ui.unwrap()].actor].inbox[port as usize].ty.clone();
    let captured = Arc::new(Captured::default());
    let sink = captured.clone();

    let trace = sim::run(SimOptions::new(1), move |registry: Registry| {
        let hir = hir.clone();
        async move {
            registry.route_frames(ui, sink);
            for spawn in &plan.spawns {
                registry.route_out(spawn.id, spawn.out.clone());
                registry.route_watchers(spawn.id, spawn.watchers.clone());
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
            // Let every actor register and run its `init`.
            tokio::time::sleep(Duration::from_millis(20)).await;

            for event in events {
                let spec = spec_for(event).expect("this test sends deliverable events");
                let bytes = strand_cli::encode::encode(&hir, &message_ty, &spec)
                    .unwrap_or_else(|e| panic!("encoding {spec}: {e:#}"));
                registry.send_from(HOST, ui, Message::Blob { from: HOST, port, bytes })?;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

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

/// The same scenario on a real clock, for the cases virtual time cannot hold.
fn drive_realtime(name: &str, events: Vec<InputEvent>) -> (Trace, Vec<Node>) {
    let hir = compile(name);
    let plan = plan::plan(&hir).expect("a plan");
    let ui = plan.spawns[plan.ui.expect("something draws")].id;
    let port = plan.input_port.expect("it hears input");
    let message_ty = hir.actors[plan.spawns[plan.ui.unwrap()].actor].inbox[port as usize].ty.clone();
    let captured = Arc::new(Captured::default());
    let sink = captured.clone();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    let registry = Registry::new();
    let trace = registry.trace();

    runtime.block_on(async {
        registry.route_frames(ui, sink);
        for spawn in &plan.spawns {
            registry.route_out(spawn.id, spawn.out.clone());
            registry.route_watchers(spawn.id, spawn.watchers.clone());
            registry.reserve(spawn.id);
        }
        let engine = engine().expect("an engine");
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
        tokio::time::sleep(Duration::from_millis(60)).await;

        for event in events {
            let spec = spec_for(event).expect("this test sends deliverable events");
            let bytes = strand_cli::encode::encode(&hir, &message_ty, &spec)
                .unwrap_or_else(|e| panic!("encoding {spec}: {e:#}"));
            registry
                .send_from(HOST, ui, Message::Blob { from: HOST, port, bytes })
                .expect("the UI actor should still be there");
            // Long enough that a UI actor sharing a runtime with a pegged one
            // has to actually be scheduled, rather than getting through on a
            // gap that happened to be there.
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        for spawn in &plan.spawns {
            let _ = registry.send(spawn.id, Message::Stop);
        }
        for handle in handles {
            handle.abort();
        }
    });

    // Read through the Arc rather than unwrapping it: the registry is still
    // holding its own reference, because unlike `sim::run` nothing here has
    // torn the runtime down yet.
    let frames = captured.frames.lock().unwrap().clone();
    (trace, frames)
}

/// Everything the actors wrote to the log, in order.
fn logs(trace: &Trace) -> Vec<String> {
    trace
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Logged { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

const ADD: u32 = 1;
const RESET: u32 = 2;

#[test]
fn a_send_written_in_strand_arrives_in_another_actor() {
    // The whole chain: a click reaches the meter, the meter's handler sends on
    // a port it named, and the reporter's handler runs with a value it can
    // match on. Nothing in either actor's source names the other.
    let trace = drive("pipeline.str", vec![click(ADD)]);
    assert!(
        logs(&trace).iter().any(|line| line == "reporter: total is 7 after 1 samples"),
        "the reporter should have been told the meter's total:\n{}",
        trace.render()
    );
}

#[test]
fn the_payload_survives_the_crossing_intact() {
    // Two fields, both read back on the far side. A message is copied into a
    // different arena, so a field read from the wrong offset would show up
    // here as a number that is right for the other field.
    let trace = drive("pipeline.str", vec![click(ADD), click(ADD), click(ADD)]);
    let lines = logs(&trace);
    assert!(
        lines.iter().any(|line| line == "reporter: total is 21 after 3 samples"),
        "totals accumulate and the sample count travels with them:\n{lines:#?}"
    );
}

#[test]
fn a_niladic_variant_crosses_as_a_bare_tag() {
    // `Cleared` carries nothing, so it goes as an i32 rather than a pointer to
    // a payload block — a different path through the encoder, and one where a
    // wrong size would land on the wrong variant.
    let trace = drive("pipeline.str", vec![click(ADD), click(RESET)]);
    let lines = logs(&trace);
    assert!(
        lines.iter().any(|line| line == "reporter: the meter was reset"),
        "the reset should have arrived as `Cleared`:\n{lines:#?}"
    );
}

#[test]
fn the_trace_shows_the_message_leaving_one_actor_and_reaching_the_other() {
    // §8.4's causal log covers actor-to-actor traffic, not just what the host
    // sends in — which is what makes it a record of the app rather than of the
    // harness driving it.
    let trace = drive("pipeline.str", vec![click(ADD)]);
    let events = trace.events();
    assert!(
        events.iter().any(|e| matches!(e, Event::Sent { from: 0, to: 1, .. })),
        "the meter's send should appear:\n{}",
        trace.render()
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Delivered { to: 1, from: 0, .. })),
        "and its delivery:\n{}",
        trace.render()
    );
}

#[test]
fn each_actor_keeps_its_own_arena() {
    // Two instances of two modules, two Stores. If they shared memory, the
    // reporter's `seen` count would be reading the meter's state.
    let trace = drive("pipeline.str", vec![click(ADD), click(ADD)]);
    let spawned: Vec<String> = trace
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::Spawned { name, .. } => Some(name),
            _ => None,
        })
        .collect();
    assert_eq!(spawned, vec!["meter".to_string(), "reporter".to_string()]);
}

// ---- §7's demo script ------------------------------------------------------
//
// The two beats the whole architecture argument rests on, asserted rather than
// demonstrated by hand. They are here rather than in `app.rs` because both need
// a second actor to happen to.

const CRASH: u32 = 5;
const BURN: u32 = 6;
const CLEAR: u32 = 3;

fn demo(events: Vec<InputEvent>) -> (Trace, Vec<Node>) {
    drive_capturing("todo_demo.str", events)
}

#[test]
fn the_counts_arrive_from_the_other_actor() {
    // Nothing draws a count until Stats has been asked and has answered. The
    // first ask is the platform's `Up` at startup, so this also covers a
    // lifecycle message reaching a guest.
    let (_, frames) = demo(vec![]);
    let last = labels(frames.last().expect("a frame"));
    assert!(
        last.iter().any(|t| t.starts_with("3/4 done ·")),
        "the tally the Stats actor computed should be on screen: {last:?}"
    );
}

#[test]
fn crashing_stats_shows_a_boundary_and_leaves_the_todos_alone() {
    // §7's first beat. The panic is real — `panic()` in Strand — and what
    // reviewers see is a boundary rather than a dead window.
    let (trace, frames) = demo(vec![click(CRASH)]);

    assert!(
        trace.events().iter().any(|e| matches!(
            e,
            Event::Crashed { id: 1, reason } if reason.contains("asked to fall over")
        )),
        "the message `panic` was given should be the crash report's reason:\n{}",
        trace.render()
    );
    assert!(
        trace.events().iter().any(|e| matches!(e, Event::Restarted { id: 1, .. })),
        "the supervisor should have replaced it:\n{}",
        trace.render()
    );

    let boundary = frames
        .iter()
        .any(|frame| labels(frame).iter().any(|t| t.starts_with("stats unavailable")));
    assert!(boundary, "the panel should have shown a failure boundary for a beat");

    // The claim the beat exists to make: the todos were never in that arena.
    let last = labels(frames.last().expect("a frame"));
    assert!(last.iter().any(|t| t == "write the compiler"), "{last:?}");
    assert!(last.iter().any(|t| t == "todo — 3/4 done"), "{last:?}");
}

#[test]
fn the_counts_come_back_after_the_restart() {
    // The replacement starts from `init` with nothing, so the tally only
    // returns because `Up` prompted the UI to say it all again.
    let (_, frames) = demo(vec![click(CRASH)]);
    let last = labels(frames.last().expect("a frame"));
    assert!(
        last.iter().any(|t| t.starts_with("3/4 done ·")),
        "the counts should have reappeared: {last:?}"
    );
    assert!(
        !last.iter().any(|t| t.starts_with("stats unavailable")),
        "and the boundary should be gone: {last:?}"
    );
}

#[test]
fn the_ui_actor_keeps_working_while_the_other_one_burns() {
    // §7's second beat, as far as a headless test can hold it: with Stats
    // pegged, the UI actor still handles input and still draws. What a test
    // cannot assert is the frame rate — that is the compositor's thread, and
    // it is what the window is for.
    //
    // On a real clock rather than `sim`'s. Virtual time advances only when
    // every task is idle, and an actor that hands itself work is never idle —
    // so the scenario that is *about* one actor never yielding is the one
    // scenario virtual time cannot represent.
    let mut events = vec![click(BURN)];
    events.extend(typing("still typing"));
    events.push(InputEvent::Key { id: HitId(1), key: Key::Enter });
    let (_, frames) = drive_realtime("todo_demo.str", events);

    let last = labels(frames.last().expect("a frame"));
    assert!(
        last.iter().any(|t| t == "still typing"),
        "the todo typed while Stats burned should be there: {last:?}"
    );
    assert!(last.iter().any(|t| t == "todo — 3/5 done"), "{last:?}");
}

#[test]
fn a_crash_does_not_disturb_an_edit_in_progress() {
    // The strongest form of the isolation claim: the other actor dies mid-way
    // through a sentence and the sentence is still there.
    let mut events = typing("half a thought");
    events.push(click(CRASH));
    events.extend(typing(" finished"));
    events.push(InputEvent::Key { id: HitId(1), key: Key::Enter });
    let (_, frames) = demo(events);

    let last = labels(frames.last().expect("a frame"));
    assert!(
        last.iter().any(|t| t == "half a thought finished"),
        "the draft survived the other actor's death: {last:?}"
    );
}

#[test]
fn clearing_done_todos_updates_the_count_the_other_actor_keeps() {
    // The two actors' views of the list stay in step, which is the ordinary
    // case the crash beats are the exception to.
    let (_, frames) = demo(vec![click(CLEAR)]);
    let last = labels(frames.last().expect("a frame"));
    assert!(
        last.iter().any(|t| t.starts_with("0/1 done ·")),
        "Stats should have been told about the sweep: {last:?}"
    );
}
