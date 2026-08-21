//! Proves the actor runtime replays identically — the TigerBeetle property
//! from §17, asserted rather than assumed.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use strand_runtime::sim::{self, SimOptions};
use strand_runtime::{engine, spawn_actor, Event, Message, Registry, Trace, Wiring};

const ACTORS: [(u32, &str, &str); 3] = [
    (0, "ping", "ping.wat"),
    (1, "pong", "pong.wat"),
    (2, "ticker", "ticker.wat"),
];

fn wat(file: &str) -> String {
    // Tests run with the crate root as cwd; the fixtures live at the repo root.
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("wasm")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

/// The M0 scenario, driven through the simulated scheduler.
async fn ping_pong(registry: Registry) -> Result<()> {
    let engine = engine()?;
    // The two fixtures name a port of their own and know nothing else; that
    // ping's out port 0 arrives at pong is decided here, the way an `app`
    // block decides it for compiled Strand.
    registry.route_out(0, vec![Some(Wiring { to: 1, port: 0 })]);
    registry.route_out(1, vec![Some(Wiring { to: 0, port: 0 })]);
    let mut handles = Vec::new();
    for (id, name, file) in ACTORS {
        handles.push(spawn_actor(&engine, &registry, id, name, wat(file).as_bytes()).await?);
    }

    // Virtual time: this returns immediately, having advanced the clock.
    tokio::time::sleep(Duration::from_millis(900)).await;
    for (id, _, _) in ACTORS {
        let _ = registry.send(id, Message::Stop);
    }
    for handle in handles {
        handle.await??;
    }
    Ok(())
}

fn run(options: SimOptions) -> Trace {
    sim::run(options, ping_pong).expect("simulation failed")
}

#[test]
fn the_same_seed_replays_the_same_run() {
    let first = run(SimOptions::new(42));
    let second = run(SimOptions::new(42));
    assert_eq!(
        first.events(),
        second.events(),
        "identical seeds must produce identical traces:\n--- first ---\n{}\n--- second ---\n{}",
        first.render(),
        second.render()
    );
    assert!(!first.events().is_empty(), "the scenario should produce events");
}

#[test]
fn chaos_replays_too() {
    // Perturbation must be seeded, not ambient: a chaotic run is still exact.
    let first = run(SimOptions::chaotic(7));
    let second = run(SimOptions::chaotic(7));
    assert_eq!(first.events(), second.events(), "a chaotic run must still be reproducible");
}

#[test]
fn the_trace_captures_the_message_exchange() {
    // §8.4: a causal log of who sent what to whom, with the payloads typed.
    let trace = run(SimOptions::new(1));
    let events = trace.events();

    assert!(
        events.contains(&Event::Sent { from: 0, to: 1, len: 4 }),
        "ping should send PING to pong:\n{}",
        trace.render()
    );
    assert!(
        events.contains(&Event::Delivered { to: 1, from: 0, len: 4 }),
        "pong should receive it:\n{}",
        trace.render()
    );
    assert!(
        events.contains(&Event::Sent { from: 1, to: 0, len: 4 }),
        "pong should reply:\n{}",
        trace.render()
    );

    // A send always precedes its delivery.
    let sent = events.iter().position(|e| matches!(e, Event::Sent { from: 0, to: 1, .. }));
    let delivered =
        events.iter().position(|e| matches!(e, Event::Delivered { to: 1, from: 0, .. }));
    assert!(sent < delivered, "causality violated:\n{}", trace.render());
}

#[test]
fn every_actor_is_spawned_and_stopped() {
    let trace = run(SimOptions::new(3));
    let events = trace.events();
    for (id, name, _) in ACTORS {
        assert!(
            events.contains(&Event::Spawned { id, name: name.to_string() }),
            "actor {name} should appear as spawned:\n{}",
            trace.render()
        );
        assert!(
            events.contains(&Event::Stopped { id }),
            "actor {name} should shut down cleanly:\n{}",
            trace.render()
        );
    }
}

#[test]
fn virtual_time_makes_a_slow_scenario_instant() {
    // ping sleeps 300ms and ticker sleeps 8x50ms of simulated time. Under a
    // real clock this scenario takes about a second; here it should not.
    let started = std::time::Instant::now();
    run(SimOptions::new(5));
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "virtual time should collapse the wait, took {:?}",
        started.elapsed()
    );
}
