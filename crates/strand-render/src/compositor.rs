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

use crate::scene::Node;

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
