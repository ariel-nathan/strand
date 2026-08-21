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
use strandc::hir::{Hir, Ty};
use strandc::input;

use crate::frame;

/// The one UI actor. §5.1 gives every actor an id; this app has one to give.
const UI: u32 = 0;

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

/// Checks that this module can actually receive input before a window opens.
///
/// The failure it exists for is an actor that draws itself but declares some
/// other message type: it would run, paint once, and then ignore every click in
/// silence. Better to refuse at the door and say why.
fn check_receives_input(hir: &Hir) -> Result<()> {
    let Some(actor) = &hir.actor else {
        return Err(anyhow!("this module declares no actor"));
    };
    let Ty::Sum(id) = &actor.message else {
        return Err(anyhow!(
            "`{}` receives {}, so it cannot be told about clicks — write \
             `message: {}` to receive input (§6.1)",
            actor.name,
            hir.ty(&actor.message),
            input::TYPE_NAME
        ));
    };
    if hir.sums[id.0 as usize].name != input::TYPE_NAME {
        return Err(anyhow!(
            "`{}` receives `{}`, which is not the platform's input type — write \
             `message: {}` to receive input (§6.1)",
            actor.name,
            hir.sums[id.0 as usize].name,
            input::TYPE_NAME
        ));
    }
    Ok(())
}

/// Runs a Strand UI actor under the compositor.
pub fn run(hir: &Hir, wasm: &[u8]) -> Result<()> {
    check_receives_input(hir)?;
    let actor = hir.actor.as_ref().expect("checked just above");
    let (name, message_ty) = (actor.name.clone(), actor.message.clone());

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
    let wasm = wasm.to_vec();

    // Before spawning: the actor draws itself once at startup, and a sink
    // registered afterwards would miss that first frame.
    registry.route_frames(UI, Arc::new(ToCompositor { theme: Theme::default(), scenes }));

    let published = stats.clone();
    let _guard = runtime.enter();
    let feeding = registry.clone();
    runtime.spawn(crate::stats::publish(registry.clone(), published));
    runtime.spawn(async move {
        let handle = spawn_supervised(
            &engine,
            &registry,
            UI,
            &name,
            &wasm,
            // §5.4: a view that traps takes its actor down, and the supervisor
            // puts a fresh one in its place. The window never closes.
            Policy::Restart,
            None,
        );

        loop {
            for event in input_receiver.drain() {
                let Some(spec) = spec_for(event) else { continue };
                match crate::encode::encode(&hir_for_encoding, &message_ty, &spec) {
                    Ok(bytes) => {
                        let _ = feeding.send_from(
                            HOST,
                            UI,
                            Message::Blob { from: HOST, bytes },
                        );
                    }
                    Err(error) => eprintln!("!! could not deliver {spec}: {error:#}"),
                }
            }
            if handle.is_finished() {
                return;
            }
            // Input is polled rather than awaited: the channel is a std one, so
            // that a compositor thread can send without touching the runtime.
            tokio::time::sleep(Duration::from_millis(4)).await;
        }
    });

    println!("--- strand M3: a UI actor written in Strand (§6.2, §6.5) ---");
    println!("press F12 for the debug overlay (§8.4) — the actor drawing this has a row");
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

    #[test]
    fn an_actor_that_cannot_hear_input_is_refused_before_a_window_opens() {
        let src = "\
type Count = { total: int }
actor Counter {
  state: Count
  fn init(): Count { Count { total: 0 } }
  fn receive(state: Count, msg: string): Count { state }
  view fn draw(state: Count): Node { text(\"hi\") }
}
";
        let hir = strandc::compile("t.str", src).expect("should compile");
        let message = check_receives_input(&hir).unwrap_err().to_string();
        assert!(message.contains("message: Input"), "{message}");
    }

    #[test]
    fn an_actor_that_asked_for_input_is_accepted() {
        let src = "\
type Count = { total: int }
actor Counter {
  state: Count
  message: Input
  fn init(): Count { Count { total: 0 } }
  fn receive(state: Count, msg: Input): Count { state }
  view fn draw(state: Count): Node { text(\"hi\") }
}
";
        let hir = strandc::compile("t.str", src).expect("should compile");
        check_receives_input(&hir).expect("this one can hear");
    }
}
