//! §8.4's debug overlay, from the measuring end: per-actor arena sizes,
//! mailbox depths and fiber counts.
//!
//! §7 promises reviewers "a debug overlay showing live per-actor memory,
//! making isolation visible". A gauge is only worth drawing if it is true, so
//! these tests watch real actors rather than the display type.
//!
//! Everything runs under `sim`, so the interleavings these assertions depend on
//! — an actor caught mid-sleep with two messages still queued — are reproducible
//! rather than lucky.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use strand_runtime::sim::{self, SimOptions};
use strand_runtime::{
    engine, spawn_supervised, ActorStats, Message, Policy, Registry, HOST,
};

const SLOW: u32 = 0;
const CRASHER: u32 = 1;

/// One 64K wasm page — the unit an arena grows in.
const PAGE: u64 = 65_536;

fn wat(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("wasm")
        .join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn poke(registry: &Registry, to: u32, bytes: &[u8]) {
    let _ = registry.send(to, Message::Blob { from: HOST, bytes: bytes.to_vec() });
}

fn row(stats: &[ActorStats], name: &str) -> ActorStats {
    stats
        .iter()
        .find(|stat| stat.name == name)
        .unwrap_or_else(|| panic!("no row for {name} in {stats:#?}"))
        .clone()
}

/// Runs `scenario` and hands back every snapshot it recorded.
///
/// `sim::run` returns the trace, not the registry, so samples come out through
/// a slot the scenario writes into — history and liveness are different
/// questions and the runtime keeps them apart.
fn sampled<F, Fut>(seed: u64, scenario: F) -> Vec<Vec<ActorStats>>
where
    F: FnOnce(Registry, Arc<Mutex<Vec<Vec<ActorStats>>>>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let samples = Arc::new(Mutex::new(Vec::new()));
    let collected = samples.clone();
    sim::run(SimOptions::new(seed), move |registry| scenario(registry, collected))
        .expect("simulation failed");
    Arc::try_unwrap(samples).expect("scenario kept the slot").into_inner().unwrap()
}

async fn settle(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[test]
fn an_actor_reports_its_arena_mailbox_and_fiber_while_it_works() {
    let samples = sampled(1, |registry, samples| async move {
        let engine = engine()?;
        let slow = spawn_supervised(
            &engine,
            &registry,
            SLOW,
            "slowpoke",
            wat("slowpoke.wat").as_bytes(),
            Policy::Restart,
            None,
        );

        settle(10).await;
        samples.lock().unwrap().push(registry.stats()); // [0] idle

        // Three at once. The actor sleeps 40ms per message, so after 10ms it is
        // inside the first and two are still queued behind it.
        poke(&registry, SLOW, b"a");
        poke(&registry, SLOW, b"b");
        poke(&registry, SLOW, b"c");
        settle(10).await;
        samples.lock().unwrap().push(registry.stats()); // [1] mid-flight

        settle(300).await;
        samples.lock().unwrap().push(registry.stats()); // [2] caught up

        let _ = registry.send(SLOW, Message::Stop);
        let _ = slow.await;
        Ok(())
    });

    let idle = row(&samples[0], "slowpoke");
    assert!(idle.alive);
    assert_eq!(idle.arena_bytes, PAGE, "one page before it has done anything");
    assert_eq!(idle.fibers, 0, "an idle actor is running nothing");
    assert_eq!(idle.handled, 0);

    let busy = row(&samples[1], "slowpoke");
    assert_eq!(busy.mailbox, 2, "two messages waiting behind the one in hand");
    assert_eq!(busy.fibers, 1, "one fiber, suspended in the guest's sleep");

    let caught_up = row(&samples[2], "slowpoke");
    assert_eq!(caught_up.mailbox, 0, "the backlog drained");
    assert_eq!(caught_up.fibers, 0);
    assert_eq!(caught_up.handled, 3);
    assert_eq!(
        caught_up.arena_bytes,
        PAGE * 4,
        "a page per message, still held — the POC's bump allocator never frees"
    );
}

#[test]
fn a_restart_hands_the_arena_back_and_ticks_the_generation() {
    // §5.1's isolation claim as a number someone can watch: the arena a dead
    // actor grew is gone, not inherited.
    let samples = sampled(2, |registry, samples| async move {
        let engine = engine()?;
        let slow = spawn_supervised(
            &engine,
            &registry,
            SLOW,
            "slowpoke",
            wat("slowpoke.wat").as_bytes(),
            Policy::Restart,
            None,
        );
        let crasher = spawn_supervised(
            &engine,
            &registry,
            CRASHER,
            "crasher",
            wat("crasher.wat").as_bytes(),
            Policy::Restart,
            None,
        );

        // Actors register from inside their own task, so a message sent
        // before the first yield has nowhere to land.
        settle(10).await;
        for _ in 0..3 {
            poke(&registry, SLOW, b"grow");
        }
        poke(&registry, CRASHER, b"PING");
        settle(300).await;
        samples.lock().unwrap().push(registry.stats()); // [0] both fat and happy

        poke(&registry, CRASHER, b"BOOM");
        settle(300).await;
        samples.lock().unwrap().push(registry.stats()); // [1] one of them died

        let _ = registry.send(SLOW, Message::Stop);
        let _ = registry.send(CRASHER, Message::Stop);
        let _ = slow.await;
        let _ = crasher.await;
        Ok(())
    });

    let before = row(&samples[0], "crasher");
    assert_eq!(before.generation, 0);
    assert_eq!(before.handled, 1);

    let after = row(&samples[1], "crasher");
    assert_eq!(after.generation, 1, "the supervisor built a new one");
    assert_eq!(after.handled, 0, "and its message count starts over");
    assert!(after.alive, "restarted, not dead");

    // The sibling never noticed. Same arena, same count, right through.
    let sibling_before = row(&samples[0], "slowpoke");
    let sibling_after = row(&samples[1], "slowpoke");
    assert_eq!(sibling_before.arena_bytes, PAGE * 4);
    assert_eq!(sibling_after.arena_bytes, sibling_before.arena_bytes);
    assert_eq!(sibling_after.handled, sibling_before.handled);
    assert_eq!(sibling_after.generation, 0, "a sibling's death is not its own");
}

#[test]
fn a_stopped_actor_leaves_its_row_behind() {
    // The overlay's job is to say what happened to an actor, and "it is gone"
    // is the most important thing it can say. A row that vanished would read as
    // an actor that never existed.
    let samples = sampled(3, |registry, samples| async move {
        let engine = engine()?;
        let slow = spawn_supervised(
            &engine,
            &registry,
            SLOW,
            "slowpoke",
            wat("slowpoke.wat").as_bytes(),
            Policy::Restart,
            None,
        );

        settle(10).await;
        poke(&registry, SLOW, b"a");
        settle(200).await;
        let _ = registry.send(SLOW, Message::Stop);
        let _ = slow.await;
        settle(10).await;
        samples.lock().unwrap().push(registry.stats());
        Ok(())
    });

    let stopped = row(&samples[0], "slowpoke");
    assert!(!stopped.alive, "the row stays, reporting the death");
    assert_eq!(stopped.fibers, 0, "nothing is running in a dead actor");
    assert_eq!(stopped.handled, 1, "and what it did before dying is still readable");
}

#[test]
fn rows_keep_their_order_between_snapshots() {
    // Ordered by id, so the overlay's rows do not swap places from frame to
    // frame and make a still picture look like activity.
    let samples = sampled(4, |registry, samples| async move {
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
        settle(10).await;
        // Spawned second, but its id is lower.
        let slow = spawn_supervised(
            &engine,
            &registry,
            SLOW,
            "slowpoke",
            wat("slowpoke.wat").as_bytes(),
            Policy::Restart,
            None,
        );
        settle(10).await;
        samples.lock().unwrap().push(registry.stats());

        let _ = registry.send(SLOW, Message::Stop);
        let _ = registry.send(CRASHER, Message::Stop);
        let _ = slow.await;
        let _ = crasher.await;
        Ok(())
    });

    let names: Vec<&str> = samples[0].iter().map(|stat| stat.name.as_str()).collect();
    assert_eq!(names, vec!["slowpoke", "crasher"], "by id, not by spawn order");
}

#[test]
fn an_empty_registry_has_nothing_to_report() {
    let registry = Registry::new();
    assert!(registry.stats().is_empty());
}
