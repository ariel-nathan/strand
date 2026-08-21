//! The M0 walking-skeleton demo, kept as a subcommand so the actor runtime
//! stays exercised while the language comes up.

use std::time::Duration;

use anyhow::Result;
use strand_render::inspect::StatsHandle;
use strand_runtime::{
    engine, spawn_actor, spawn_supervised, Message, Policy, Registry, Wiring, HOST,
};

const ACTORS: [(u32, &str, &str); 3] = [
    (0, "ping", "wasm/ping.wat"),
    (1, "pong", "wasm/pong.wat"),
    (2, "ticker", "wasm/ticker.wat"),
];

/// Who each actor's out port 0 leads to.
///
/// The fixtures name a port of their own and nothing else; that a `send` from
/// ping arrives at pong is decided here, by the host, exactly as an `app`
/// block decides it for compiled Strand. Written out because these two are
/// hand-written WAT and have no `app` block to be read from.
const WIRES: [(u32, u32); 2] = [(0, 1), (1, 0)];

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
        return rt.block_on(run_actors(traced, None));
    }

    // Windowed: the actor runtime becomes a guest of the compositor. winit
    // requires the main thread, so the runtime gets a background thread and
    // main() belongs to the renderer for the rest of the process lifetime.
    println!("--- strand M0: actors + compositor ---");
    println!("press F12 for the debug overlay (§8.4)");
    let stats = StatsHandle::new();
    let _guard = rt.enter();
    let published = stats.clone();
    rt.spawn(async move {
        if let Err(e) = run_actors(traced, Some(published)).await {
            eprintln!("actor runtime failed: {e:#}");
        }
    });
    strand_render::run_with_stats(None, None, Some(stats))
}

async fn run_actors(traced: bool, stats: Option<StatsHandle>) -> Result<()> {
    let engine = engine()?;
    let registry = Registry::new();
    if let Some(stats) = stats {
        tokio::spawn(crate::stats::publish(registry.clone(), stats));
    }

    println!("--- strand M0: 3 actors, 1 worker thread ---");
    for (from, to) in WIRES {
        registry.route_out(from, vec![Some(Wiring { to, port: 0 })]);
    }
    let mut handles = Vec::new();
    for (id, name, path) in ACTORS {
        let wat = crate::examples::read(path)?;
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
    rt.block_on(run_crash(traced, false, None))
}

/// The same script, on a loop, under the compositor — so §7's "isolation
/// visible" claim is something you watch rather than read.
///
/// Press F12: the crasher's row picks up a generation each time round, its
/// arena size drops back to a fresh module's, and the ticker's row beside it
/// never so much as pauses.
pub fn crash_windowed() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;

    println!("--- strand M2: supervision, under the compositor ---");
    println!("press F12 for the debug overlay (§8.4)");
    let stats = StatsHandle::new();
    let _guard = rt.enter();
    let published = stats.clone();
    rt.spawn(async move {
        if let Err(e) = run_crash(false, true, Some(published)).await {
            eprintln!("actor runtime failed: {e:#}");
        }
    });
    strand_render::run_with_stats(None, None, Some(stats))
}

/// `forever` loops the script instead of running it once, for the windowed
/// version where the point is to keep watching.
async fn run_crash(traced: bool, forever: bool, stats: Option<StatsHandle>) -> Result<()> {
    const CRASHER: u32 = 0;
    const TICKER: u32 = 1;

    let engine = engine()?;
    let registry = Registry::new();
    if let Some(stats) = stats {
        tokio::spawn(crate::stats::publish(registry.clone(), stats));
    }
    println!("--- strand M2: supervision ---");

    let crasher = spawn_supervised(
        &engine,
        &registry,
        CRASHER,
        "crasher",
        crate::examples::read("wasm/crasher.wat")?.as_bytes(),
        Policy::Restart,
        None,
    );
    let ticker = spawn_supervised(
        &engine,
        &registry,
        TICKER,
        "ticker",
        crate::examples::read("wasm/ticker.wat")?.as_bytes(),
        Policy::Restart,
        None,
    );

    let beat = Duration::from_millis(120);
    let poke = |bytes: &[u8]| {
        let _ = registry.send(CRASHER, Message::Blob { from: HOST, port: 0, bytes: bytes.to_vec() });
    };

    loop {
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

        if !forever {
            break;
        }
        // Long enough to read the overlay between rounds.
        tokio::time::sleep(beat * 8).await;
    }

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

/// §6.1's most visible claim: a busy app actor delays its own updates, never
/// the compositor.
///
/// The "app actor" here is a plain thread — it submits UI trees over the scene
/// channel exactly as an actor would, and `--burn` makes it hog its own thread
/// between submissions. The bar stops moving; the window does not stop
/// drawing.
pub fn ui(burn: bool) -> Result<()> {
    use strand_render::compositor::InputEvent;
    use strand_render::scene::{Color, HitId, Node, Sizing, Style};

    let (sender, receiver) = strand_render::compositor::scene_channel();
    let (input_sender, mut input_receiver) = strand_render::compositor::input_channel();

    std::thread::spawn(move || {
        let accent = Color::rgb(0.35, 0.55, 0.95);
        let hot = Color::rgb(0.95, 0.45, 0.35);
        let panel = Color::rgb(0.13, 0.14, 0.18);
        let mut step: f32 = 0.0;
        let mut armed = false;

        loop {
            // Input arrives as typed messages naming the node that was hit —
            // the app never hit-tests (§6.1).
            for event in input_receiver.drain() {
                match event {
                    InputEvent::PointerDown { id, .. } => {
                        println!("app: pointer down on node {}", id.0);
                        armed = !armed;
                    }
                    InputEvent::PointerEnter { id } => println!("app: enter {}", id.0),
                    InputEvent::PointerLeave { id } => println!("app: leave {}", id.0),
                    // This demo has no field to type into, nothing to
                    // scroll, and no actor state to throw away; the todo app
                    // is where those are exercised.
                    InputEvent::PointerUp { .. }
                    | InputEvent::Key { .. }
                    | InputEvent::FocusChanged { .. }
                    | InputEvent::Scroll { .. }
                    | InputEvent::Restart => {}
                }
            }

            step = (step + 0.02) % 1.0;
            // A bar whose width tracks `step`, so movement is app progress.
            let tree = Node::column(
                Style { width: Sizing::Grow, height: Sizing::Grow, padding: 24.0, gap: 16.0, ..Default::default() },
                vec![
                    Node::Box {
                        style: Style {
                            width: Sizing::Percent(0.1 + step * 0.85),
                            height: Sizing::Fixed(56.0),
                            background: Some(accent),
                            ..Default::default()
                        },
                    },
                    // A clickable panel: it carries a HitId, so input routes here.
                    Node::Box {
                        style: Style {
                            id: Some(HitId(1)),
                            width: Sizing::Grow,
                            height: Sizing::Grow,
                            background: Some(if armed { hot } else { panel }),
                            ..Default::default()
                        },
                    },
                ],
            );

            if !sender.submit(tree) {
                return; // compositor gone
            }

            if burn {
                // Deliberately hostile: hold this thread for a third of a
                // second. Under the old model this is where the UI freezes.
                let until = std::time::Instant::now() + Duration::from_millis(300);
                while std::time::Instant::now() < until {
                    std::hint::spin_loop();
                }
            } else {
                std::thread::sleep(Duration::from_millis(16));
            }
        }
    });

    if burn {
        println!("--- strand M3: app actor burning CPU; the compositor should not care ---");
    } else {
        println!("--- strand M3: app actor submitting frames ---");
    }
    strand_render::run_with(Some(receiver), Some(input_sender))
}
