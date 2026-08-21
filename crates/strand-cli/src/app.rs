//! A UI actor written in Strand, driving the compositor (§10, M3 complete).
//!
//! §10 asks for the UI tree to be submitted "from a host-side actor first, then
//! from Strand code". `todo.rs` is the first half. This is the second, and the
//! shape is deliberately identical: state in a record, events as messages, the
//! handler returning the next state, the view a pure function of it (§6.5).
//! What changed is who runs it.
//!
//! It is a real actor, not a special case. It has a mailbox, an arena, a row in
//! §8.4's overlay, and a supervisor: a view that traps takes the actor down and
//! the supervisor builds a new one, while the compositor keeps drawing the last
//! frame that arrived. Nothing about §5's machinery had to know a UI exists.
//!
//! The only new piece is the direction of travel. Input goes in as a `Message`
//! like any other; frames come out through `Frames`, which hands the host the
//! bytes the actor left in its own arena and nothing else.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use strand_render::compositor::{
    input_channel, scene_channel, InputEvent, Key, SceneSender,
};
use strand_render::inspect::StatsHandle;
use strand_render::widgets::Theme;
use strand_runtime::{
    engine, spawn_supervised, Frames, Message, Policy, Registry, HOST,
};
use strandc::hir::Hir;
use strandc::input;

use crate::frame;
use crate::plan::{self, Plan};

/// Turns the frames an actor draws into trees the compositor can paint.
///
/// Runs on the actor's own thread, so decoding a frame costs the actor its own
/// time and nobody else's — which is §6.1's bargain exactly: an app that draws
/// something expensive delays its own next frame, never the compositor's.
struct ToCompositor {
    theme: Theme,
    scenes: SceneSender,
}

impl Frames for ToCompositor {
    fn submit(&self, memory: &[u8], base: u32, count: u32) {
        match frame::decode(&self.theme, memory, base, count) {
            Ok(tree) => {
                self.scenes.submit(tree);
            }
            // A malformed frame is the app's bug, not the window's. Say so and
            // keep the last good tree on screen.
            Err(error) => eprintln!("!! the view produced a frame that will not decode: {error:#}"),
        }
    }
}

/// Renders one input event as the message spec `encode` reads.
///
/// Going through the same encoder the CLI uses means the wire format has one
/// implementation rather than two that must agree. It also makes every event
/// something you could have sent by hand, which is what lets the translation be
/// tested without a window.
pub fn spec_for(event: InputEvent) -> Option<String> {
    Some(match event {
        InputEvent::PointerDown { id, .. } => format!("Click {}", id.0),
        InputEvent::Key { key, .. } => match key {
            // The scalar value, because a message may carry no pointer and a
            // string is one.
            Key::Char(character) => format!("Typed {}", character as u32),
            Key::Backspace => "Backspace".to_string(),
            Key::Enter => "Enter".to_string(),
            Key::Escape => "Escape".to_string(),
        },
        // Nothing focused is id 0, the same way a node with no id is 0.
        InputEvent::FocusChanged { id } => {
            format!("Focus {}", id.map_or(0, |id| id.0))
        }
        InputEvent::Scroll { id, offset } => format!("Scrolled {} {offset}", id.0),
        // Pointer release and hover are not in `input::VARIANTS`: an actor
        // cannot ask for them, so there is nothing to deliver them to.
        InputEvent::PointerUp { .. }
        | InputEvent::PointerEnter { .. }
        | InputEvent::PointerLeave { .. } => return None,
    })
}

/// Checks that the drawing actor can actually receive input before a window
/// opens.
///
/// The failure it exists for is an actor that draws itself but declares no
/// `Input` port: it would run, paint once, and then ignore every click in
/// silence. Better to refuse at the door and say why.
fn check_receives_input(hir: &Hir, plan: &Plan) -> Result<()> {
    let Some(ui) = plan.ui else {
        return Err(anyhow!("nothing in this app draws a window"));
    };
    if plan.input_port.is_some() {
        return Ok(());
    }
    let actor = &hir.actors[plan.spawns[ui].actor];
    let ports: Vec<String> = actor
        .inbox
        .iter()
        .map(|port| format!("`{}` ({})", port.name, hir.ty(&port.ty)))
        .collect();
    let has = if ports.is_empty() {
        "it declares no `in` ports at all".to_string()
    } else {
        format!("it receives on {}", ports.join(", "))
    };
    Err(anyhow!(
        "`{}` draws a window but cannot be told about clicks — {has}. Add \
         `in input: {}` and an `on input` handler (§6.1)",
        actor.name,
        input::TYPE_NAME
    ))
}

/// Runs a Strand app: every actor in its own arena, wired as the file says.
pub fn run(hir: &Hir) -> Result<()> {
    let plan = plan::plan(hir)?;
    check_receives_input(hir, &plan)?;
    let ui = plan.ui.expect("checked just above");
    let ui_id = plan.spawns[ui].id;
    let input_port = plan.input_port.expect("checked just above");
    let message_ty = hir.actors[plan.spawns[ui].actor].inbox[input_port as usize].ty.clone();

    let (scenes, scene_receiver) = scene_channel();
    let (input_sender, mut input_receiver) = input_channel();
    let stats = StatsHandle::new();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;

    let engine = engine()?;
    let registry = Registry::new();
    // Cloned because the encoder reads layout from the same `Hir` the compiler
    // produced — both ends of the channel agree by construction (§5.3).
    let hir_for_encoding = hir.clone();

    // Before spawning: an actor draws itself and may send from `init`, so a
    // route registered afterwards would already have missed something.
    registry.route_frames(ui_id, Arc::new(ToCompositor { theme: Theme::default(), scenes }));
    for spawn in &plan.spawns {
        registry.route_out(spawn.id, spawn.out.clone());
    }

    let published = stats.clone();
    let _guard = runtime.enter();
    let feeding = registry.clone();
    runtime.spawn(crate::stats::publish(registry.clone(), published));
    runtime.spawn(async move {
        let mut handles = Vec::new();
        for spawn in &plan.spawns {
            handles.push(spawn_supervised(
                &engine,
                &registry,
                spawn.id,
                &spawn.name,
                &spawn.wasm,
                // §5.4: a handler that traps takes its actor down, and the
                // supervisor puts a fresh one in its place. Its siblings never
                // notice, and the window never closes.
                Policy::Restart,
                None,
            ));
        }

        loop {
            for event in input_receiver.drain() {
                let Some(spec) = spec_for(event) else { continue };
                match crate::encode::encode(&hir_for_encoding, &message_ty, &spec) {
                    Ok(bytes) => {
                        let _ = feeding.send_from(
                            HOST,
                            ui_id,
                            Message::Blob { from: HOST, port: input_port, bytes },
                        );
                    }
                    Err(error) => eprintln!("!! could not deliver {spec}: {error:#}"),
                }
            }
            // The window follows the actor that draws it: when that one is
            // gone for good, there is nothing left to show.
            if handles[ui].is_finished() {
                return;
            }
            // Input is polled rather than awaited: the channel is a std one, so
            // that a compositor thread can send without touching the runtime.
            tokio::time::sleep(Duration::from_millis(4)).await;
        }
    });

    println!("--- strand: a UI actor written in Strand (§6.2, §6.5) ---");
    println!("press F12 for the debug overlay (§8.4) — every actor here has a row");
    strand_render::run_with_stats(Some(scene_receiver), Some(input_sender), Some(stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use strand_render::scene::HitId;

    #[test]
    fn every_event_an_actor_can_receive_has_a_spec() {
        // The two tables have to line up: anything `spec_for` produces must name
        // a variant `input::VARIANTS` declares, or the encoder will reject it at
        // the moment someone clicks.
        let events = [
            InputEvent::PointerDown { id: HitId(7), x: 0.0, y: 0.0 },
            InputEvent::Key { id: HitId(1), key: Key::Char('a') },
            InputEvent::Key { id: HitId(1), key: Key::Backspace },
            InputEvent::Key { id: HitId(1), key: Key::Enter },
            InputEvent::Key { id: HitId(1), key: Key::Escape },
            InputEvent::FocusChanged { id: Some(HitId(3)) },
            InputEvent::FocusChanged { id: None },
            InputEvent::Scroll { id: HitId(4), offset: 12.5 },
        ];

        for event in events {
            let spec = spec_for(event).unwrap_or_else(|| panic!("no spec for {event:?}"));
            let head = spec.split_whitespace().next().expect("a variant name");
            let variant = input::variant(head)
                .unwrap_or_else(|| panic!("`{head}` is not a declared input variant"));
            let args = spec.split_whitespace().count() - 1;
            assert_eq!(
                args,
                variant.fields.len(),
                "`{head}` was given {args} argument(s), but takes {}",
                variant.fields.len()
            );
        }
    }

    #[test]
    fn events_no_actor_can_ask_for_are_not_delivered() {
        // Pointer release and hover have no variant, so there is nothing to
        // deliver them to. Silently dropping them beats inventing a message.
        assert!(spec_for(InputEvent::PointerUp { id: HitId(1), x: 0.0, y: 0.0 }).is_none());
        assert!(spec_for(InputEvent::PointerEnter { id: HitId(1) }).is_none());
        assert!(spec_for(InputEvent::PointerLeave { id: HitId(1) }).is_none());
    }

    #[test]
    fn nothing_focused_is_the_same_zero_a_node_without_an_id_has() {
        assert_eq!(spec_for(InputEvent::FocusChanged { id: None }).as_deref(), Some("Focus 0"));
    }

    #[test]
    fn a_typed_character_travels_as_its_scalar_value() {
        // A message may carry no pointer, so the character cannot be a string.
        let spec = spec_for(InputEvent::Key { id: HitId(1), key: Key::Char('é') });
        assert_eq!(spec.as_deref(), Some("Typed 233"));
    }

    fn checked(src: &str) -> Result<()> {
        let hir = strandc::compile("t.str", src).expect("should compile");
        let plan = plan::plan(&hir)?;
        check_receives_input(&hir, &plan)
    }

    #[test]
    fn an_actor_that_cannot_hear_input_is_refused_before_a_window_opens() {
        // It would open a window, paint once, and then ignore every click
        // without saying anything. Refusing at the door and naming the fix is
        // the whole reason the check exists.
        let message = checked(
            "type Count = { total: int }
             actor Counter {
               state: Count
               in ticks: int
               fn init(): Count { Count { total: 0 } }
               on ticks(state: Count, msg: int): Count { state }
               view fn draw(state: Count): Node { text(\"hi\") }
             }",
        )
        .unwrap_err()
        .to_string();
        assert!(message.contains("in input: Input"), "{message}");
        assert!(message.contains("`ticks` (int)"), "it says what it does hear: {message}");
    }

    #[test]
    fn an_actor_that_asked_for_input_is_accepted() {
        checked(
            "type Count = { total: int }
             actor Counter {
               state: Count
               in input: Input
               fn init(): Count { Count { total: 0 } }
               on input(state: Count, msg: Input): Count { state }
               view fn draw(state: Count): Node { text(\"hi\") }
             }",
        )
        .expect("this one can hear");
    }

    #[test]
    fn the_input_port_is_found_by_type_rather_than_by_name() {
        // Calling it `input` is a convention; carrying `Input` is the fact. A
        // port named anything still receives clicks, because the platform
        // matches on the type it declared (docs/abi.md §9) — the same reason
        // the type is declared there rather than matched by spelling.
        checked(
            "type Count = { total: int }
             actor Counter {
               state: Count
               in fromTheUser: Input
               fn init(): Count { Count { total: 0 } }
               on fromTheUser(state: Count, msg: Input): Count { state }
               view fn draw(state: Count): Node { text(\"hi\") }
             }",
        )
        .expect("the name is not the protocol");
    }
}
