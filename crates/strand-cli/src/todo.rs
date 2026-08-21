//! A host-side UI actor driving the compositor (§10, M3).
//!
//! §10 asks for the UI tree to be submitted "from a host-side actor first,
//! then from Strand code". This is the first half: the state, the view function
//! and the update loop all live in Rust, but they talk to the compositor
//! through exactly the channels a Strand UI actor will use, so replacing this
//! with compiled Strand code changes who builds the tree and nothing else.
//!
//! The state model is §6.5's: the actor owns a record, events are messages,
//! and the handler returns the next state. Nothing mutates in place.

use std::time::Duration;

use anyhow::Result;
use strand_render::compositor::{scene_channel, input_channel, InputEvent};
use strand_render::scene::{HitId, Node, Sizing, Style};
use strand_render::widgets::{button, label, muted_label, panel, screen, Theme};

#[derive(Debug, Clone, PartialEq)]
struct Todo {
    title: String,
    done: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct State {
    todos: Vec<Todo>,
    /// Set when a validation rule rejects an action — §7's "empty title shows
    /// a notice, not a crash".
    notice: Option<String>,
}

/// Ids are assigned by position, with a base per widget kind so a checkbox and
/// a button can never collide. §6.2 calls these stable ids.
const CHECKBOX_BASE: u32 = 1000;
const ADD_BUTTON: HitId = HitId(1);
const CLEAR_BUTTON: HitId = HitId(2);

impl State {
    fn new() -> Self {
        Self {
            todos: vec![
                Todo { title: "write the compiler".into(), done: true },
                Todo { title: "write the runtime".into(), done: true },
                Todo { title: "write the renderer".into(), done: false },
            ],
            notice: None,
        }
    }

    /// The §6.5 handler: takes an event, returns the next state.
    fn update(mut self, event: InputEvent) -> Self {
        let InputEvent::PointerDown { id, .. } = event else { return self };
        self.notice = None;

        match id {
            ADD_BUTTON => {
                let next = self.todos.len() + 1;
                self.todos.push(Todo { title: format!("new todo {next}"), done: false });
            }
            CLEAR_BUTTON => {
                let before = self.todos.len();
                self.todos.retain(|todo| !todo.done);
                if self.todos.len() == before {
                    self.notice = Some("nothing completed to clear".into());
                }
            }
            HitId(raw) if raw >= CHECKBOX_BASE => {
                let index = (raw - CHECKBOX_BASE) as usize;
                if let Some(todo) = self.todos.get_mut(index) {
                    todo.done = !todo.done;
                }
            }
            _ => {}
        }
        self
    }
}

/// The view: state in, tree out. Pure, so it can be re-run every frame.
fn view(theme: &Theme, state: &State) -> Node {
    let rows: Vec<Node> = state
        .todos
        .iter()
        .enumerate()
        .map(|(index, todo)| {
            strand_render::widgets::checkbox(
                theme,
                HitId(CHECKBOX_BASE + index as u32),
                todo.done,
                todo.title.clone(),
            )
        })
        .collect();

    let done = state.todos.iter().filter(|todo| todo.done).count();
    let mut children = vec![
        label(theme, format!("todo — {done}/{} done", state.todos.len())),
        panel(theme, rows),
        Node::row(
            Style { gap: theme.gap, ..Default::default() },
            vec![button(theme, ADD_BUTTON, "Add"), button(theme, CLEAR_BUTTON, "Clear done")],
        ),
    ];
    if let Some(notice) = &state.notice {
        children.push(muted_label(theme, notice.clone()));
    }
    children.push(Node::Box {
        style: Style { height: Sizing::Grow, ..Default::default() },
    });

    screen(theme, children)
}

/// Prints the todo UI as a laid-out tree (§8.4), without opening a window.
///
/// This is the inspector's most useful form for anyone who cannot see the
/// screen: it reports where every node actually ended up, so a layout can be
/// checked without a screenshot.
pub fn inspect(viewport: (f32, f32)) -> String {
    strand_render::inspect::describe(&view(&Theme::default(), &State::new()), viewport)
}

/// Runs the todo UI: an actor on the runtime, the compositor on this thread.
pub fn run() -> Result<()> {
    let (scenes, scene_receiver) = scene_channel();
    let (input_sender, mut input_receiver) = input_channel();

    // The UI actor. A tokio task rather than a raw thread, so it is scheduled
    // by the same runtime that will host the Strand version.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;

    runtime.spawn(async move {
        let theme = Theme::default();
        let mut state = State::new();
        // Submit once so the window has something before any input arrives.
        scenes.submit(view(&theme, &state));

        loop {
            let events = input_receiver.drain();
            if !events.is_empty() {
                for event in events {
                    state = state.update(event);
                }
                // Rebuild only when something actually changed (§9.6: the
                // actor is the re-render unit).
                if !scenes.submit(view(&theme, &state)) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(8)).await;
        }
    });

    println!("--- strand M3: todo UI, host-side actor driving the compositor ---");
    println!("click the checkboxes, Add, or Clear done");
    strand_render::run_with(Some(scene_receiver), Some(input_sender))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click(id: HitId) -> InputEvent {
        InputEvent::PointerDown { id, x: 0.0, y: 0.0 }
    }

    #[test]
    fn toggling_a_checkbox_flips_one_todo() {
        let state = State::new();
        let before = state.todos[2].done;
        let after = state.update(click(HitId(CHECKBOX_BASE + 2)));
        assert_eq!(after.todos[2].done, !before);
        assert_eq!(after.todos[0].done, State::new().todos[0].done, "others untouched");
    }

    #[test]
    fn add_appends_a_todo() {
        let state = State::new().update(click(ADD_BUTTON));
        assert_eq!(state.todos.len(), 4);
        assert!(!state.todos[3].done, "a new todo starts undone");
    }

    #[test]
    fn clear_removes_completed_todos() {
        let state = State::new().update(click(CLEAR_BUTTON));
        assert_eq!(state.todos.len(), 1, "the two done ones go");
        assert!(state.todos.iter().all(|todo| !todo.done));
    }

    #[test]
    fn clearing_nothing_shows_a_notice_rather_than_failing() {
        // §7: a rejected action surfaces a notice, it does not crash.
        let mut state = State::new();
        state.todos.iter_mut().for_each(|todo| todo.done = false);
        let state = state.update(click(CLEAR_BUTTON));
        assert_eq!(state.notice.as_deref(), Some("nothing completed to clear"));
        assert_eq!(state.todos.len(), 3, "and nothing was removed");
    }

    #[test]
    fn a_notice_clears_on_the_next_action() {
        let mut state = State::new();
        state.todos.iter_mut().for_each(|todo| todo.done = false);
        let state = state.update(click(CLEAR_BUTTON));
        assert!(state.notice.is_some());
        let state = state.update(click(ADD_BUTTON));
        assert!(state.notice.is_none(), "the notice is about the last action only");
    }

    #[test]
    fn clicks_that_hit_nothing_known_change_nothing() {
        let state = State::new();
        let after = state.clone().update(click(HitId(999)));
        assert_eq!(after.todos, state.todos);
    }

    #[test]
    fn every_todo_gets_its_own_hit_target() {
        let state = State::new();
        let tree = view(&Theme::default(), &state);
        let mut layouter = strand_render::scene::Layouter::new();
        let frame = layouter.layout(&tree, (600.0, 400.0));

        let ids: Vec<u32> = frame.hits.iter().map(|region| region.id.0).collect();
        assert_eq!(
            ids,
            vec![CHECKBOX_BASE, CHECKBOX_BASE + 1, CHECKBOX_BASE + 2, ADD_BUTTON.0, CLEAR_BUTTON.0],
            "three checkboxes and two buttons, in paint order"
        );
    }

    #[test]
    fn the_view_is_a_pure_function_of_state() {
        // Re-running the view must produce the same tree, which is what lets
        // the compositor treat frames as interchangeable.
        let state = State::new();
        let theme = Theme::default();
        assert_eq!(view(&theme, &state), view(&theme, &state));
    }
}
