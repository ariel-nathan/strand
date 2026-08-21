//! The M0 walking-skeleton demo, kept as a subcommand so the actor runtime
//! stays exercised while the language comes up.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use strand_runtime::{engine, spawn_actor, spawn_supervised, Message, Policy, Registry, HOST};

const ACTORS: [(u32, &str, &str); 3] = [
    (0, "ping", "examples/wasm/ping.wat"),
    (1, "pong", "examples/wasm/pong.wat"),
    (2, "ticker", "examples/wasm/ticker.wat"),
];

pub fn run(windowed: bool) -> Result<()> {
    run_with(windowed, false)
}

/// `traced` prints the causal message log afterwards (§8.4).
pub fn run_with(windowed: bool, traced: bool) -> Result<()> {
    // ONE worker thread on purpose. If a sleeping actor held its OS thread,
    // the ticker could not tick during ping's 300ms sleep.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;

    if !windowed {
        return rt.block_on(run_actors(traced));
    }

    // Windowed: the actor runtime becomes a guest of the compositor. winit
    // requires the main thread, so the runtime gets a background thread and
    // main() belongs to the renderer for the rest of the process lifetime.
    println!("--- strand M0: actors + compositor ---");
    let _guard = rt.enter();
    rt.spawn(async move {
        if let Err(e) = run_actors(traced).await {
            eprintln!("actor runtime failed: {e:#}");
        }
    });
    strand_render::run()
}

async fn run_actors(traced: bool) -> Result<()> {
    let engine = engine()?;
    let registry = Registry::new();

    println!("--- strand M0: 3 actors, 1 worker thread ---");
    let mut handles = Vec::new();
    for (id, name, path) in ACTORS {
        let wat =
            std::fs::read_to_string(Path::new(path)).with_context(|| format!("reading {path}"))?;
        handles.push(spawn_actor(&engine, &registry, id, name, wat.as_bytes()).await?);
    }

    tokio::time::sleep(Duration::from_millis(900)).await;
    for (id, _, _) in ACTORS {
        let _ = registry.send(id, Message::Stop);
    }
    for handle in handles {
        handle.await??;
    }

    println!("--- actors done ---");
    if traced {
        // §8.4: actors interact only through typed messages, so this log is a
        // complete record of their inputs.
        println!("
--- message trace ---
{}", registry.trace().render());
    }
    Ok(())
}

/// §5.4's demo: a deliberately-crashable actor that the supervisor restarts
/// while a sibling keeps running.
pub fn crash(traced: bool) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    rt.block_on(run_crash(traced))
}

async fn run_crash(traced: bool) -> Result<()> {
    const CRASHER: u32 = 0;
    const TICKER: u32 = 1;

    let engine = engine()?;
    let registry = Registry::new();
    println!("--- strand M2: supervision ---");

    let crasher = spawn_supervised(
        &engine,
        &registry,
        CRASHER,
        "crasher",
        std::fs::read_to_string("examples/wasm/crasher.wat")?.as_bytes(),
        Policy::Restart,
        None,
    );
    let ticker = spawn_supervised(
        &engine,
        &registry,
        TICKER,
        "ticker",
        std::fs::read_to_string("examples/wasm/ticker.wat")?.as_bytes(),
        Policy::Restart,
        None,
    );

    let beat = Duration::from_millis(120);
    let poke = |bytes: &[u8]| {
        let _ = registry.send(CRASHER, Message::Blob { from: HOST, bytes: bytes.to_vec() });
    };

    tokio::time::sleep(beat).await;
    poke(b"PING");
    tokio::time::sleep(beat).await;
    poke(b"PING");
    tokio::time::sleep(beat).await;

    println!(">>> sending BOOM to the crasher");
    poke(b"BOOM");
    tokio::time::sleep(beat).await;

    println!(">>> pinging the restarted actor (its count should be back at #1)");
    poke(b"PING");
    tokio::time::sleep(beat).await;

    let _ = registry.send(CRASHER, Message::Stop);
    let _ = registry.send(TICKER, Message::Stop);
    let _ = crasher.await;
    let _ = ticker.await;

    println!("--- done ---");
    if traced {
        println!("\n--- message trace ---\n{}", registry.trace().render());
    }
    Ok(())
}
