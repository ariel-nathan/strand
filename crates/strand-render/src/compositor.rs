//! The channel between app actors and the platform-owned compositor (§6.1).
//!
//! An app actor builds a UI tree and *submits* it. Submission is a message, so
//! the compositor never waits on the app: it draws whatever it most recently
//! received. A slow actor therefore delays its own updates and nothing else —
//! the frame still goes out, showing the last tree it sent.
//!
//! Two consequences worth naming, because both are properties rather than
//! accidents:
//!
//! - **Latest wins.** Stale frames are dropped rather than queued. A backlog
//!   would mean the compositor rendering history while the app runs ahead.
//! - **Never empty after the first submission.** The compositor keeps the last
//!   tree, so there is always something to draw.

use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};

use crate::scene::{HitId, Node};

/// The app end of the channel. Cloneable, so several actors could submit,
/// though the POC has one UI actor.
#[derive(Clone)]
pub struct SceneSender {
    tx: Sender<Node>,
}

impl SceneSender {
    /// Submits a tree. Returns `false` once the compositor has gone away.
    pub fn submit(&self, tree: Node) -> bool {
        self.tx.send(tree).is_ok()
    }
}

/// The compositor end. Polling never blocks — that is the whole point.
pub struct SceneReceiver {
    rx: Receiver<Node>,
    current: Option<Node>,
    superseded: usize,
}

impl SceneReceiver {
    /// Takes the newest submitted tree, discarding any that were superseded
    /// while the compositor was busy. Returns whether the scene changed.
    pub fn poll(&mut self) -> bool {
        let mut updated = false;
        loop {
            match self.rx.try_recv() {
                Ok(tree) => {
                    if self.current.is_some() {
                        // Only counts trees that were never drawn.
                        self.superseded += usize::from(updated);
                    }
                    self.current = Some(tree);
                    updated = true;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        updated
    }

    /// The tree to draw: the most recent submission, or nothing before the
    /// first one arrives.
    pub fn current(&self) -> Option<&Node> {
        self.current.as_ref()
    }

    /// How many submissions were replaced before they were ever drawn. A
    /// healthy sign under load: the app ran ahead and the compositor skipped.
    pub fn superseded(&self) -> usize {
        self.superseded
    }
}

/// A keystroke, reduced to what a text field needs.
///
/// §2 lists text input beyond the todo app's minimum as a non-goal, and this is
/// that minimum: characters, the two edits that make a field usable, and a way
/// out. No selection, no composition, no clipboard — each of those is a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Already resolved through the keyboard layout and modifiers, so this is
    /// the character the user meant rather than the key they pressed.
    Char(char),
    Backspace,
    Enter,
    Escape,
}

/// What the compositor sends back when input lands on a node (§6.1).
///
/// Typed, and carrying the id of the node that was hit — routing is the
/// platform's job, so the app never hit-tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    PointerDown { id: HitId, x: f32, y: f32 },
    PointerUp { id: HitId, x: f32, y: f32 },
    /// The pointer entered a different node than it was over last frame.
    PointerEnter { id: HitId },
    PointerLeave { id: HitId },
    /// A keystroke, delivered to whichever node holds focus. A keyboard has no
    /// position, so focus is what routing has instead of hit-testing.
    Key { id: HitId, key: Key },
    /// Focus moved. `None` means a click landed somewhere that does not take it.
    FocusChanged { id: Option<HitId> },
    /// The wheel turned over a scrollable region.
    ///
    /// Carries where the offset now *is*, not how far it moved. The platform
    /// measured the content this frame, so it is the only party that can clamp
    /// the value — and sending the clamped result means an app can never hold a
    /// scroll position that shows nothing.
    Scroll { id: HitId, offset: f32 },
    /// The user asked for the app to start over (§9.3).
    ///
    /// Not input, and no actor can ask for it: it is a command to the platform,
    /// travelling on this channel because this is the path from the window to
    /// the host. Saving the file reloads the code and keeps the state; this
    /// throws the state away, which is the other half a person needs when a
    /// string they just edited is one their code put in the state.
    Restart,
}

/// The compositor end: sends events towards the app.
#[derive(Clone)]
pub struct InputSender {
    tx: Sender<InputEvent>,
}

impl InputSender {
    pub fn send(&self, event: InputEvent) -> bool {
        self.tx.send(event).is_ok()
    }
}

/// The app end: drains events without blocking.
pub struct InputReceiver {
    rx: Receiver<InputEvent>,
}

impl InputReceiver {
    /// Takes everything queued since the last call. Input is not coalesced the
    /// way scenes are: dropping a click would lose the user's intent.
    pub fn drain(&mut self) -> Vec<InputEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            events.push(event);
        }
        events
    }
}

pub fn input_channel() -> (InputSender, InputReceiver) {
    let (tx, rx) = mpsc::channel();
    (InputSender { tx }, InputReceiver { rx })
}

pub fn scene_channel() -> (SceneSender, SceneReceiver) {
    let (tx, rx) = mpsc::channel();
    (SceneSender { tx }, SceneReceiver { rx, current: None, superseded: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{Sizing, Style};
    use std::time::Duration;

    fn tree(width: f32) -> Node {
        Node::Box { style: Style { width: Sizing::Fixed(width), ..Default::default() } }
    }

    fn width_of(node: &Node) -> f32 {
        match node {
            Node::Box { style } => match style.width {
                Sizing::Fixed(width) => width,
                _ => -1.0,
            },
            _ => -1.0,
        }
    }

    #[test]
    fn the_compositor_has_nothing_to_draw_before_the_first_submission() {
        let (_sender, mut receiver) = scene_channel();
        assert!(!receiver.poll());
        assert!(receiver.current().is_none());
    }

    #[test]
    fn polling_takes_the_newest_tree_and_drops_the_stale_ones() {
        let (sender, mut receiver) = scene_channel();
        sender.submit(tree(1.0));
        sender.submit(tree(2.0));
        sender.submit(tree(3.0));

        assert!(receiver.poll(), "a submission should register as a change");
        assert_eq!(width_of(receiver.current().unwrap()), 3.0, "latest wins, no backlog");
    }

    #[test]
    fn a_frame_with_no_submission_redraws_the_last_tree() {
        // The §6.1 claim in its smallest form: the compositor is never blocked
        // waiting for the app, it just draws what it already has.
        let (sender, mut receiver) = scene_channel();
        sender.submit(tree(7.0));
        receiver.poll();

        for _ in 0..10 {
            assert!(!receiver.poll(), "nothing new arrived");
            assert_eq!(
                width_of(receiver.current().unwrap()),
                7.0,
                "but there is still a frame to draw"
            );
        }
    }

    #[test]
    fn a_slow_app_does_not_stall_the_compositor() {
        // A producer that submits rarely, against a compositor ticking fast.
        // The compositor's tick count must not depend on the app at all.
        let (sender, mut receiver) = scene_channel();
        let producer = std::thread::spawn(move || {
            for width in 1..=3 {
                std::thread::sleep(Duration::from_millis(30));
                if !sender.submit(tree(width as f32)) {
                    return;
                }
            }
        });

        let mut ticks = 0;
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_millis(120) {
            receiver.poll();
            ticks += 1;
            std::thread::sleep(Duration::from_millis(1));
        }
        producer.join().expect("producer panicked");

        assert!(
            ticks > 20,
            "the compositor should keep ticking regardless of the app, got {ticks}"
        );
    }

    #[test]
    fn an_app_running_ahead_has_its_extra_frames_skipped() {
        let (sender, mut receiver) = scene_channel();
        sender.submit(tree(1.0));
        receiver.poll();

        // Three more arrive before the next poll; two are never drawn.
        sender.submit(tree(2.0));
        sender.submit(tree(3.0));
        sender.submit(tree(4.0));
        receiver.poll();

        assert_eq!(width_of(receiver.current().unwrap()), 4.0);
        assert_eq!(receiver.superseded(), 2, "skipped rather than queued");
    }

    #[test]
    fn input_events_are_delivered_in_order_and_never_coalesced() {
        // Scenes coalesce because only the newest matters. Input does not:
        // dropping a click would lose what the user meant.
        let (sender, mut receiver) = input_channel();
        sender.send(InputEvent::PointerDown { id: HitId(1), x: 1.0, y: 2.0 });
        sender.send(InputEvent::PointerUp { id: HitId(1), x: 1.0, y: 2.0 });
        sender.send(InputEvent::PointerDown { id: HitId(2), x: 3.0, y: 4.0 });

        let events = receiver.drain();
        assert_eq!(events.len(), 3, "every event survives");
        assert!(matches!(events[0], InputEvent::PointerDown { id: HitId(1), .. }));
        assert!(matches!(events[2], InputEvent::PointerDown { id: HitId(2), .. }));
        assert!(receiver.drain().is_empty(), "draining twice yields nothing");
    }

    #[test]
    fn a_dead_app_leaves_the_last_frame_standing() {
        let (sender, mut receiver) = scene_channel();
        sender.submit(tree(5.0));
        receiver.poll();
        drop(sender);

        // A crashed UI actor must not take the window down with it.
        assert!(!receiver.poll());
        assert_eq!(width_of(receiver.current().unwrap()), 5.0);
    }
}
