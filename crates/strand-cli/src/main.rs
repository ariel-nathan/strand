//! M0 walking skeleton.
//!
//!   strand            headless: 3 actors on ONE worker thread
//!   strand --window   the same actors, with the compositor owning main()

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use strand_runtime::{engine, spawn_actor, Message, Registry};

const ACTORS: [(u32, &str, &str); 3] = [
    (0, "ping", "examples/wasm/ping.wat"),
    (1, "pong", "examples/wasm/pong.wat"),
    (2, "ticker", "examples/wasm/ticker.wat"),
];

fn main() -> Result<()> {
    let windowed = std::env::args().any(|a| a == "--window");

    // ONE worker thread on purpose. If a sleeping actor held its OS thread,
    // the ticker could not tick during ping's 300ms sleep.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;

    if !windowed {
        return rt.block_on(run_actors());
    }

    // Windowed: the actor runtime becomes a guest of the compositor. winit
    // requires the main thread, so the runtime gets a background thread and
    // main() belongs to the renderer for the rest of the process lifetime.
    println!("--- strand M0: actors + compositor ---");
    let _guard = rt.enter();
    rt.spawn(async {
        if let Err(e) = run_actors().await {
            eprintln!("actor runtime failed: {e:#}");
        }
    });
    strand_render::run()
}

async fn run_actors() -> Result<()> {
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
    Ok(())
}
