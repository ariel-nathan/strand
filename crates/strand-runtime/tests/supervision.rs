//! §5.4: a panicking actor dies, its arena is reclaimed, the supervisor
//! restarts it, and nothing else notices.
//!
//! Every case runs under `sim`, so a failure here is reproducible by seed
//! rather than being a flaky ordering artefact.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use strand_runtime::sim::{self, SimOptions};
use strand_runtime::{engine, spawn_supervised, Event, Message, Policy, Registry, Trace, HOST};

const CRASHER: u32 = 0;
const TICKER: u32 = 1;

fn wat(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("wasm")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn ping(registry: &Registry) {
    let _ = registry.send(CRASHER, Message::Blob { from: HOST, bytes: b"PING".to_vec() });
}

fn boom(registry: &Registry) {
    let _ = registry.send(CRASHER, Message::Blob { from: HOST, bytes: b"BOOM".to_vec() });
}

/// Lets the runtime make progress. Time is virtual, so this is free.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// The §5.4 demo: PING, PING, BOOM, PING.
async fn crash_and_restart(registry: Registry) -> Result<()> {
    let engine = engine()?;
    let crasher = spawn_supervised(
        &engine,
        &registry,
        CRASHER,
        "crasher",
        wat("crasher.wat").as_bytes(),
        Policy::Restart,
        None,
    );
    let ticker =
        spawn_supervised(&engine, &registry, TICKER, "ticker", wat("ticker.wat").as_bytes(), Policy::Restart, None);

    settle().await;
    ping(&registry); // #1
    settle().await;
    ping(&registry); // "again"
    settle().await;

    boom(&registry); // dies here
    settle().await;

    ping(&registry); // #1 again, if the arena really was reclaimed
    settle().await;

    let _ = registry.send(CRASHER, Message::Stop);
    let _ = registry.send(TICKER, Message::Stop);
    let _ = crasher.await;
    let _ = ticker.await;
    Ok(())
}

fn run(options: SimOptions) -> Trace {
    sim::run(options, crash_and_restart).expect("simulation failed")
}

#[test]
fn a_crash_is_reported_and_the_actor_restarts() {
    let trace = run(SimOptions::new(1));
    let events = trace.events();

    let crashed = events.iter().any(|e| matches!(e, Event::Crashed { id: CRASHER, .. }));
    assert!(crashed, "the crasher should have died:\n{}", trace.render());

    let restarted =
        events.iter().any(|e| matches!(e, Event::Restarted { id: CRASHER, generation: 1 }));
    assert!(restarted, "the supervisor should have restarted it:\n{}", trace.render());
}

#[test]
fn the_crash_names_the_message_that_caused_it() {
    // §8.4: a structured report, not stack-trace soup.
    let trace = run(SimOptions::new(2));
    let Some(Event::Crashed { reason, .. }) =
        trace.events().into_iter().find(|e| matches!(e, Event::Crashed { .. }))
    else {
        panic!("no crash recorded:\n{}", trace.render());
    };
    assert!(
        reason.contains("unreachable") || reason.contains("wasm trap"),
        "the reason should describe the trap, was: {reason}"
    );
}

#[test]
fn restarting_reclaims_the_arena() {
    // The crasher counts messages in a global. Seeing "#1" a second time is
    // proof the Store — and the whole arena with it — was dropped (§5.1).
    let trace = run(SimOptions::new(3));
    let firsts = trace
        .events()
        .iter()
        .filter(|e| matches!(e, Event::Logged { text, .. } if text.contains("handled #1")))
        .count();
    assert_eq!(
        firsts, 2,
        "the counter should restart from zero after the crash:\n{}",
        trace.render()
    );
}

#[test]
fn the_restarted_actor_starts_up_again() {
    let trace = run(SimOptions::new(4));
    let ups = trace
        .events()
        .iter()
        .filter(|e| matches!(e, Event::Logged { text, .. } if text.contains("crasher: up")))
        .count();
    assert_eq!(ups, 2, "startup should run once per life:\n{}", trace.render());
}

#[test]
fn a_sibling_is_untouched_by_the_crash() {
    // The isolation claim: the ticker keeps ticking across the crash.
    let trace = run(SimOptions::new(5));
    let events = trace.events();
    let crash_at = events
        .iter()
        .position(|e| matches!(e, Event::Crashed { .. }))
        .expect("expected a crash");

    let ticks_after = events[crash_at..]
        .iter()
        .filter(|e| matches!(e, Event::Logged { id: TICKER, text } if text == "tick"))
        .count();
    assert!(ticks_after > 0, "the ticker should survive a sibling's death:\n{}", trace.render());

    assert!(
        !events.iter().any(|e| matches!(e, Event::Crashed { id: TICKER, .. })),
        "the ticker must not be affected:\n{}",
        trace.render()
    );
}

#[test]
fn crash_and_restart_replays_exactly() {
    // The whole point of building determinism first: even the failure path is
    // reproducible, so a supervision bug can be re-run rather than re-hunted.
    let first = run(SimOptions::new(9));
    let second = run(SimOptions::new(9));
    assert_eq!(
        first.events(),
        second.events(),
        "a crash/restart run must replay:\n--- first ---\n{}\n--- second ---\n{}",
        first.render(),
        second.render()
    );
}

#[test]
fn a_stop_policy_surfaces_the_report_instead_of_restarting() {
    let trace = sim::run(SimOptions::new(11), |registry: Registry| async move {
        let engine = engine()?;
        let handle = spawn_supervised(
            &engine,
            &registry,
            CRASHER,
            "crasher",
            wat("crasher.wat").as_bytes(),
            Policy::Stop,
            None,
        );
        settle().await;
        boom(&registry);
        settle().await;

        let outcome = handle.await.expect("task panicked");
        let report = outcome.expect_err("the actor should have died");
        assert_eq!(report.actor, CRASHER);
        assert_eq!(report.name, "crasher");
        assert_eq!(report.handling.as_deref(), Some("\"BOOM\" from the host"));
        Ok(())
    })
    .expect("simulation failed");

    assert!(
        !trace.events().iter().any(|e| matches!(e, Event::Restarted { .. })),
        "Policy::Stop must not restart:\n{}",
        trace.render()
    );
}
