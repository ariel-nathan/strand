//! §5.3: sending a buffer transfers ownership, and the sender loses access.
//!
//! The design doc's claim is that data races are *unrepresentable* rather than
//! discouraged by convention. These tests hold that to its word: touching a
//! transferred handle is not a lint, it is a trap that kills the actor.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_runtime::sim::{self, SimOptions};
use strand_runtime::{engine, spawn_supervised, Event, Message, Policy, Registry};

const SENDER: u32 = 0;
const RECEIVER: u32 = 1;

fn wat(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("wasm")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// Runs the transfer scenario once, returning its trace and why the sender died.
fn run(seed: u64) -> (Vec<Event>, String) {
    let reason = Arc::new(Mutex::new(String::new()));
    let sink = reason.clone();

    let trace = sim::run(SimOptions::new(seed), move |registry: Registry| async move {
        let engine = engine()?;
        // Start the receiver first and let it register: the sender transfers
        // during its own startup, and you cannot send to an actor that has no
        // mailbox yet.
        let receiver = spawn_supervised(
            &engine,
            &registry,
            RECEIVER,
            "pong",
            wat("pong.wat").as_bytes(),
            Policy::Restart,
            None,
        );
        settle().await;

        let sender = spawn_supervised(
            &engine,
            &registry,
            SENDER,
            "sender",
            wat("transfer.wat").as_bytes(),
            // Stop, not Restart: we want the report rather than a new life.
            Policy::Stop,
            None,
        );
        settle().await;
        let outcome = sender.await.expect("task panicked");
        let report = outcome.expect_err("the sender should have trapped");
        *sink.lock().unwrap() = report.reason.clone();

        let _ = registry.send(RECEIVER, Message::Stop);
        let _ = receiver.await;
        Ok(())
    })
    .expect("simulation failed");

    let captured = reason.lock().unwrap().clone();
    (trace.events(), captured)
}

#[test]
fn the_payload_arrives_at_the_receiver() {
    let (events, _) = run(1);
    assert!(
        events.iter().any(|e| matches!(e, Event::Logged { id: RECEIVER, text } if text == "HELLO")),
        "the receiver should have got the transferred bytes:\n{events:#?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Sent { from: SENDER, to: RECEIVER, len: 5 })),
        "the transfer should appear in the trace:\n{events:#?}"
    );
}

#[test]
fn touching_a_transferred_handle_traps() {
    let (_, reason) = run(2);
    assert!(
        reason.contains("already transferred"),
        "the sender should have died on use-after-transfer, got: {reason}"
    );
}

#[test]
fn the_sender_dies_and_the_receiver_does_not() {
    let (events, _) = run(3);
    assert!(
        events.iter().any(|e| matches!(e, Event::Crashed { id: SENDER, .. })),
        "the sender should have crashed:\n{events:#?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Crashed { id: RECEIVER, .. })),
        "the receiver is unaffected by the sender's mistake:\n{events:#?}"
    );
}

#[test]
fn the_transfer_scenario_replays() {
    let (first, _) = run(7);
    let (second, _) = run(7);
    assert_eq!(first, second, "an ownership-transfer run must be reproducible");
}
