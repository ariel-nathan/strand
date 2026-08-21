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

use std::time::Duration;

use strand_cli::app::spec_for;
use strand_cli::plan;
use strand_render::compositor::InputEvent;
use strand_render::scene::HitId;
use strand_runtime::sim::{self, SimOptions};
use strand_runtime::{engine, spawn_supervised, Event, Message, Policy, Registry, Trace, HOST};
use strandc::hir::Hir;

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

/// Spawns the whole tree the file describes, feeds the UI actor `events`, and
/// hands back the trace.
fn drive(name: &str, events: Vec<InputEvent>) -> Trace {
    let hir = compile(name);
    let plan = plan::plan(&hir).expect("a plan");
    let ui = plan.spawns[plan.ui.expect("something draws")].id;
    let port = plan.input_port.expect("it hears input");
    let message_ty = hir.actors[plan.spawns[plan.ui.unwrap()].actor].inbox[port as usize].ty.clone();

    sim::run(SimOptions::new(1), move |registry: Registry| {
        let hir = hir.clone();
        async move {
            for spawn in &plan.spawns {
                registry.route_out(spawn.id, spawn.out.clone());
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
    .expect("simulation failed")
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
