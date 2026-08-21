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

fn example(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("strand")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn source() -> String {
    example("toggles.str")
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
    drive_source(&source(), events)
}

fn drive_source(src: &str, events: Vec<InputEvent>) -> Vec<Node> {
    let (hir, wasm) = compile(src);
    let message_ty = hir.lone_actor().expect("an actor").inbox[0].ty.clone();
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
                registry.send_from(HOST, UI, Message::Blob { from: HOST, port: 0, bytes })?;
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
            &hir.lone_actor().expect("an actor").name.clone(),
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

// ---- typing ---------------------------------------------------------------

fn typing(text: &str) -> Vec<InputEvent> {
    text.chars()
        .map(|character| InputEvent::Key { id: HitId(1), key: Key::Char(character) })
        .collect()
}

/// Drives `notes.str`, the example with a text field.
fn notes(events: Vec<InputEvent>) -> Vec<Node> {
    drive_source(&example("notes.str"), events)
}

#[test]
fn a_field_holds_what_was_typed_into_it() {
    // The loop this closes: a keystroke arrives as `Typed(ch)`, `receive`
    // appends `char(ch)` to a string in the actor's own record, and the field
    // is redrawn from it. There is no widget state anywhere.
    let frames = notes(typing("milk"));
    let last = labels(frames.last().expect("a frame"));
    assert!(last.iter().any(|t| t == "milk"), "{last:?}");
    assert!(last.iter().any(|t| t == "4 of 24 characters"), "and it is counted: {last:?}");
}

#[test]
fn backspace_removes_a_character_from_the_draft() {
    let mut events = typing("milk");
    events.push(InputEvent::Key { id: HitId(1), key: Key::Backspace });
    let last = labels(&notes(events).pop().expect("a frame"));
    assert!(last.iter().any(|t| t == "mil"), "{last:?}");
}

#[test]
fn a_character_that_is_not_ascii_survives_the_round_trip() {
    // It crosses as a scalar value because a message carries no pointers, and
    // becomes a string again on the far side.
    let last = labels(&notes(typing("café")).pop().expect("a frame"));
    assert!(last.iter().any(|t| t == "café"), "{last:?}");
}

#[test]
fn enter_commits_the_draft_and_empties_the_field() {
    let mut events = typing("buy milk");
    events.push(InputEvent::Key { id: HitId(1), key: Key::Enter });
    let last = labels(&notes(events).pop().expect("a frame"));

    assert!(last.iter().any(|t| t == "buy milk"), "the note was kept: {last:?}");
    assert!(
        last.iter().any(|t| t == "nothing typed yet"),
        "and the field is ready for the next one: {last:?}"
    );
}

#[test]
fn a_committed_note_is_trimmed() {
    let mut events = typing("   spaced   ");
    events.push(InputEvent::Key { id: HitId(1), key: Key::Enter });
    let last = labels(&notes(events).pop().expect("a frame"));
    assert!(last.iter().any(|t| t == "spaced"), "{last:?}");
}

#[test]
fn committing_nothing_is_a_notice_rather_than_a_note() {
    // §7: a rejected action surfaces a notice, it does not crash.
    let events = vec![InputEvent::Key { id: HitId(1), key: Key::Enter }];
    let last = labels(&notes(events).pop().expect("a frame"));
    assert!(last.iter().any(|t| t == "a note needs some words"), "{last:?}");
}

#[test]
fn an_over_long_note_says_how_long_is_too_long() {
    let mut events = typing(&"x".repeat(30));
    events.push(InputEvent::Key { id: HitId(1), key: Key::Enter });
    let last = labels(&notes(events).pop().expect("a frame"));
    assert!(
        last.iter().any(|t| t == "keep it under 24 characters"),
        "the number came from `str(MAX())`: {last:?}"
    );
}

#[test]
fn escape_abandons_the_draft() {
    let mut events = typing("never mind");
    events.push(InputEvent::Key { id: HitId(1), key: Key::Escape });
    let last = labels(&notes(events).pop().expect("a frame"));
    assert!(last.iter().any(|t| t == "nothing typed yet"), "{last:?}");
    assert!(!last.iter().any(|t| t == "never mind"), "{last:?}");
}

#[test]
fn an_empty_slot_costs_no_row_and_no_gap() {
    // The guard is outside `note`, so an empty slot contributes no child at
    // all. Two notes should sit exactly one gap apart.
    let mut events = typing("one");
    events.push(InputEvent::Key { id: HitId(1), key: Key::Enter });
    events.extend(typing("two"));
    events.push(InputEvent::Key { id: HitId(1), key: Key::Enter });

    let tree = notes(events).pop().expect("a frame");
    let mut layouter = Layouter::new();
    let frame = layouter.layout(&tree, (700.0, 500.0));

    let ys: Vec<f32> = frame
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Text { text, y, .. } if text == "one" || text == "two" => Some(*y),
            _ => None,
        })
        .collect();
    assert_eq!(ys.len(), 2, "both notes are drawn");
    assert_eq!(ys[1] - ys[0], 28.0, "a 20px line plus the panel's 8px gap, and nothing else");
}

// ---- §7's todo app, in Strand (M4) ---------------------------------------

/// Drives `todo_app.str`.
fn todo(events: Vec<InputEvent>) -> Vec<Node> {
    drive_source(&example("todo_app.str"), events)
}

fn enter() -> InputEvent {
    InputEvent::Key { id: HitId(1), key: Key::Enter }
}

#[test]
fn the_todo_app_starts_with_a_list_it_built_from_a_for_loop() {
    let last = labels(&todo(vec![]).pop().expect("a frame"));
    assert!(last.iter().any(|t| t == "todo — 3/4 done"), "a count, via str(): {last:?}");
    assert!(last.iter().any(|t| t == "write the compiler"), "{last:?}");
    assert!(last.iter().any(|t| t == "write the todo app in Strand"), "{last:?}");
}

#[test]
fn typing_and_enter_adds_a_todo() {
    let mut events = typing("buy milk");
    events.push(enter());
    let last = labels(&todo(events).pop().expect("a frame"));

    assert!(last.iter().any(|t| t == "buy milk"), "the new todo: {last:?}");
    assert!(last.iter().any(|t| t == "todo — 3/5 done"), "the count grew: {last:?}");
}

#[test]
fn clicking_a_row_toggles_it() {
    // Toggle ids are 1000 + the todo's own id, so a row keeps its identity
    // when the ones above it are deleted.
    let last = labels(&todo(vec![click(1004)]).pop().expect("a frame"));
    assert!(last.iter().any(|t| t == "todo — 4/4 done"), "{last:?}");

    let twice = labels(&todo(vec![click(1004), click(1004)]).pop().expect("a frame"));
    assert!(twice.iter().any(|t| t == "todo — 3/4 done"), "and back again: {twice:?}");
}

#[test]
fn clicking_the_cross_deletes_that_row_and_only_that_row() {
    let last = labels(&todo(vec![click(2002)]).pop().expect("a frame"));
    assert!(!last.iter().any(|t| t == "write the runtime"), "it went: {last:?}");
    assert!(last.iter().any(|t| t == "write the compiler"), "its neighbours stayed: {last:?}");
    assert!(last.iter().any(|t| t == "todo — 2/3 done"), "{last:?}");
}

#[test]
fn an_id_survives_a_deletion_above_it() {
    // The bug this rules out: ids derived from position would slide up when a
    // row is deleted, so the next click would land on the wrong todo.
    let last = labels(&todo(vec![click(2001), click(1004)]).pop().expect("a frame"));
    assert!(last.iter().any(|t| t == "todo — 3/3 done"), "{last:?}");
}

#[test]
fn clear_done_sweeps_the_completed_ones() {
    let last = labels(&todo(vec![click(3)]).pop().expect("a frame"));
    assert!(last.iter().any(|t| t == "todo — 0/1 done"), "{last:?}");
    assert!(last.iter().any(|t| t == "write the todo app in Strand"), "{last:?}");
    assert!(!last.iter().any(|t| t == "write the compiler"), "{last:?}");
}

#[test]
fn clearing_nothing_is_a_notice_rather_than_a_no_op() {
    // §7: a rejected action surfaces a notice.
    let last = labels(&todo(vec![click(3), click(3)]).pop().expect("a frame"));
    assert!(last.iter().any(|t| t == "nothing completed to clear"), "{last:?}");
}

#[test]
fn an_empty_title_is_refused_and_what_was_typed_is_kept() {
    let events = vec![enter()];
    let last = labels(&todo(events).pop().expect("a frame"));
    assert!(last.iter().any(|t| t == "a todo needs a title"), "{last:?}");
    assert!(last.iter().any(|t| t == "todo — 3/4 done"), "nothing was added: {last:?}");
}

#[test]
fn an_over_long_title_says_how_long_is_too_long() {
    let mut events = typing(&"x".repeat(50));
    events.push(enter());
    let last = labels(&todo(events).pop().expect("a frame"));
    assert!(last.iter().any(|t| t == "keep it under 40 characters"), "{last:?}");
    // §7: what was typed stays put so it can be fixed rather than retyped.
    assert!(last.iter().any(|t| t.starts_with("xxxx")), "{last:?}");
}

#[test]
fn emptying_the_list_shows_the_branch_that_says_so() {
    let last = labels(&todo(vec![click(2001), click(2002), click(2003), click(2004)])
        .pop()
        .expect("a frame"));
    assert!(last.iter().any(|t| t == "nothing yet — type something above"), "{last:?}");
    assert!(last.iter().any(|t| t == "todo — 0/0 done"), "{last:?}");
}

#[test]
fn a_long_list_scrolls_and_the_offset_comes_back_from_the_platform() {
    let mut events = Vec::new();
    for index in 0..20 {
        events.extend(typing(&format!("todo number {index}")));
        events.push(enter());
    }
    let tree = todo(events).pop().expect("a frame");

    let mut layouter = Layouter::new();
    let frame = layouter.layout(&tree, (800.0, 600.0));
    let extent = frame.scrolls.first().expect("the list reports itself scrollable");
    assert_eq!(extent.id, HitId(4));
    assert!(extent.max_offset > 0.0, "24 rows do not fit in 600px");

    // And the actor takes the clamped position the platform hands back.
    let scrolled = drive_source(
        &example("todo_app.str"),
        vec![InputEvent::Scroll { id: HitId(4), offset: 40.0 }],
    );
    let mut layouter = Layouter::new();
    let after = layouter.layout(scrolled.last().expect("a frame"), (800.0, 600.0));
    assert_eq!(after.scrolls[0].offset, 0.0, "four rows fit, so there is nowhere to go");
}
