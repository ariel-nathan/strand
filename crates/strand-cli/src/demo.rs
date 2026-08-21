//! The M0 walking-skeleton demo, kept as a subcommand so the actor runtime
//! stays exercised while the language comes up.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use strand_runtime::{engine, spawn_actor, Message, Registry};

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
        handles.push(spawn_actor(&engine, &registry, id, name, &wat).await?);
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
