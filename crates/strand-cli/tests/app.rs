//! A Strand UI actor, driven end to end without a window.
//!
//! The loop this proves: an input event becomes a message, the actor's
//! `receive` returns a new state, the platform re-invokes the view, and the
//! frame that comes back has changed. Every layer is the real one — the
//! compiler, the actor runtime, the frame ABI and the decoder — with only the
//! window replaced, because a click and a `Message::Blob` are the same thing by
//! the time they reach the actor.
//!
//! Runs under `sim`, so the ordering is reproducible rather than lucky.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use strand_cli::app::spec_for;
use strand_render::compositor::{InputEvent, Key};
use strand_render::scene::{Command, HitId, Layouter, Node};
use strand_render::widgets::Theme;
use strand_runtime::sim::{self, SimOptions};
use strand_runtime::{engine, spawn_supervised, Frames, Message, Policy, Registry, HOST};
use strandc::hir::Hir;

const UI: u32 = 0;

/// Collects every frame the actor draws, decoded into a tree.
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

fn source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("strand")
        .join("toggles.str");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn compile(src: &str) -> (Hir, Vec<u8>) {
    let hir = match strandc::compile("toggles.str", src) {
        Ok(hir) => hir,
        Err(report) => panic!("{:?}", miette::Report::new(report)),
    };
    let wasm = strandc::codegen::emit(&hir).expect("emit failed");
    wasmparser::validate(&wasm).expect("emitted invalid WASM");
    (hir, wasm)
}

/// Every string the tree would draw, in paint order.
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

/// Runs the actor, feeding it `events`, and hands back every frame it drew.
fn drive(events: Vec<InputEvent>) -> Vec<Node> {
    let src = source();
    let (hir, wasm) = compile(&src);
    let message_ty = hir.actor.as_ref().expect("an actor").message.clone();
    let captured = Arc::new(Captured::default());
    let sink = captured.clone();

    sim::run(SimOptions::new(1), move |registry: Registry| {
        let hir = hir.clone();
        async move {
            registry.route_frames(UI, sink);
            let handle = spawn_supervised(
                &engine()?,
                &registry,
                UI,
                "toggles",
                &wasm,
                Policy::Restart,
                None,
            );
            // Let the actor register and draw its first frame.
            tokio::time::sleep(Duration::from_millis(20)).await;

            for event in events {
                let spec = spec_for(event).expect("this test sends deliverable events");
                let bytes = strand_cli::encode::encode(&hir, &message_ty, &spec)
                    .unwrap_or_else(|e| panic!("encoding {spec}: {e:#}"));
                registry.send_from(HOST, UI, Message::Blob { from: HOST, bytes })?;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            let _ = registry.send(UI, Message::Stop);
            let _ = handle.await;
            Ok::<(), anyhow::Error>(())
        }
    })
    .expect("simulation failed");

    Arc::try_unwrap(captured).ok().expect("the sink outlived the run").frames.into_inner().unwrap()
}

fn click(id: u32) -> InputEvent {
    InputEvent::PointerDown { id: HitId(id), x: 0.0, y: 0.0 }
}

#[test]
fn an_actor_draws_itself_before_anyone_touches_it() {
    // §6.5 re-invokes the view after every message, and startup counts: a
    // window must have something to show before the first click.
    let frames = drive(vec![]);
    assert_eq!(frames.len(), 1, "one frame, drawn at startup");
    assert!(labels(&frames[0]).iter().any(|text| text == "two of three done"));
}

#[test]
fn a_click_becomes_a_message_and_the_view_is_redrawn() {
    // The whole loop, in one assertion: the third checkbox starts off, a click
    // on its id arrives as `Click(12)`, `receive` returns a state with it on,
    // and the next frame says so.
    let frames = drive(vec![click(12)]);
    assert_eq!(frames.len(), 2, "startup, then one redraw");

    assert!(labels(&frames[0]).iter().any(|t| t == "two of three done"), "{:?}", labels(&frames[0]));
    assert!(labels(&frames[1]).iter().any(|t| t == "all three done"), "{:?}", labels(&frames[1]));
}

#[test]
fn clicking_the_same_thing_twice_returns_to_where_it_started() {
    let frames = drive(vec![click(12), click(12)]);
    assert_eq!(labels(&frames[0]), labels(&frames[2]), "a toggle is its own inverse");
}

#[test]
fn a_click_on_nothing_the_app_knows_leaves_the_state_alone() {
    // The `_ => state` arm. It still redraws — the platform cannot know the
    // handler chose to change nothing — but the tree is identical.
    let frames = drive(vec![click(9999)]);
    assert_eq!(frames.len(), 2);
    assert_eq!(labels(&frames[0]), labels(&frames[1]));
}

#[test]
fn a_button_swaps_which_panel_is_built() {
    // `if state.tab == 1` — a branch that does not run contributes no child, so
    // the two panels are not hidden, they are simply not there.
    let frames = drive(vec![click(2)]);
    let after = labels(&frames[1]);
    assert!(
        after.iter().any(|t| t.starts_with("This window is drawn")),
        "the About panel should appear: {after:?}"
    );
    assert!(
        !after.iter().any(|t| t == "write the compiler"),
        "and the task list should be gone entirely: {after:?}"
    );

    let back = labels(&drive(vec![click(2), click(1)]).pop().expect("a frame"));
    assert!(back.iter().any(|t| t == "write the compiler"), "{back:?}");
}

#[test]
fn events_the_app_ignores_still_arrive_and_still_type_check() {
    // `receive` matches exhaustively on `Input`, so keystrokes and focus reach
    // an app that has no use for them and fall into its `_` arm.
    let frames = drive(vec![
        InputEvent::Key { id: HitId(1), key: Key::Char('x') },
        InputEvent::Key { id: HitId(1), key: Key::Enter },
        InputEvent::FocusChanged { id: None },
        InputEvent::Scroll { id: HitId(1), offset: 4.5 },
    ]);
    assert_eq!(frames.len(), 5, "startup plus one redraw each");
    for frame in &frames {
        assert_eq!(labels(frame), labels(&frames[0]), "none of them changed anything");
    }
}

#[test]
fn the_actor_is_an_actor_like_any_other() {
    // Not a special case in the runtime: it has an arena, a mailbox and a row
    // in §8.4's overlay, because nothing about §5 knows a UI exists.
    let src = source();
    let (hir, wasm) = compile(&src);
    let stats = Arc::new(Mutex::new(Vec::new()));
    let collected = stats.clone();

    sim::run(SimOptions::new(2), move |registry: Registry| async move {
        registry.route_frames(UI, Arc::new(Captured::default()));
        let handle = spawn_supervised(
            &engine()?,
            &registry,
            UI,
            &hir.actor.expect("an actor").name,
            &wasm,
            Policy::Restart,
            None,
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        *collected.lock().unwrap() = registry.stats();

        let _ = registry.send(UI, Message::Stop);
        let _ = handle.await;
        Ok::<(), anyhow::Error>(())
    })
    .expect("simulation failed");

    let stats = stats.lock().unwrap();
    let row = stats.first().expect("the UI actor has a row");
    assert_eq!(row.name, "Toggles");
    assert!(row.alive);
    assert!(row.arena_bytes > 0, "it has an arena of its own");
}
