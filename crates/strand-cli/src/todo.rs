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
//!
//! `addTodo` is deliberately shaped like §4.5's Strand sample — a `Result` with
//! a small typed error — because §7 asks for validation failures surfaced as
//! notices rather than crashes, and that is what having the error in the type
//! buys: the caller cannot forget it is there.

use std::time::Duration;

use anyhow::Result;
use strand_render::compositor::{scene_channel, input_channel, InputEvent, Key};
use strand_render::scene::{Align, HitId, Node, Sizing, Style, TextStyle};
use strand_render::widgets::{
    button, checkbox, label, muted_label, scroll, screen, text_input, Theme,
};

#[derive(Debug, Clone, PartialEq)]
struct Todo {
    title: String,
    done: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct State {
    todos: Vec<Todo>,
    /// What has been typed into the field but not yet committed.
    draft: String,
    /// Which node the platform says has keyboard focus. The platform decides
    /// where focus is; what focus *looks* like is this view's business.
    focus: Option<HitId>,
    /// How far the list has been scrolled. The platform clamps it against the
    /// content and hands the clamped value back, so this is always a position
    /// that shows something.
    scroll: f32,
    /// Set when a validation rule rejects an action — §7's "empty title shows
    /// a notice, not a crash".
    notice: Option<String>,
}

/// Ids are assigned by position, with a base per widget kind so a checkbox and
/// a delete button can never collide. §6.2 calls these stable ids.
const CHECKBOX_BASE: u32 = 1000;
const DELETE_BASE: u32 = 2000;
const INPUT: HitId = HitId(1);
const ADD_BUTTON: HitId = HitId(2);
const CLEAR_BUTTON: HitId = HitId(3);
const LIST: HitId = HitId(4);

/// Long enough for a real todo, short enough that the rule is worth having.
const MAX_TITLE: usize = 60;

/// Why a todo could not be added — §4.5's `AddError`, in Rust.
///
/// A typed error rather than a bare `bool` or a panic: the caller has to say
/// what it does about each case, and §7's demo depends on it doing something
/// visible.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AddError {
    EmptyTitle,
    TooLong { max: usize },
}

impl AddError {
    fn notice(&self) -> String {
        match self {
            AddError::EmptyTitle => "a todo needs a title".to_string(),
            AddError::TooLong { max } => format!("keep it under {max} characters"),
        }
    }
}

/// The §4.5 shape: validation lives in the return type, and the list is
/// returned rather than mutated.
fn add_todo(todos: &[Todo], title: &str) -> Result<Vec<Todo>, AddError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(AddError::EmptyTitle);
    }
    if title.chars().count() > MAX_TITLE {
        return Err(AddError::TooLong { max: MAX_TITLE });
    }

    let mut next = todos.to_vec();
    next.push(Todo { title: title.to_string(), done: false });
    Ok(next)
}

impl State {
    fn new() -> Self {
        Self {
            todos: vec![
                Todo { title: "write the compiler".into(), done: true },
                Todo { title: "write the runtime".into(), done: true },
                Todo { title: "write the renderer".into(), done: false },
            ],
            draft: String::new(),
            focus: None,
            scroll: 0.0,
            notice: None,
        }
    }

    /// The §6.5 handler: takes an event, returns the next state.
    fn update(mut self, event: InputEvent) -> Self {
        match event {
            InputEvent::FocusChanged { id } => {
                self.focus = id;
                self
            }
            // The platform already clamped this against the content it drew, so
            // it is a position, not a request.
            InputEvent::Scroll { id, offset } if id == LIST => {
                self.scroll = offset;
                self
            }
            InputEvent::Key { key, .. } => self.typed(key),
            InputEvent::PointerDown { id, .. } => self.clicked(id),
            _ => self,
        }
    }

    fn typed(mut self, key: Key) -> Self {
        // A keystroke is an action, and a notice is about the last one.
        self.notice = None;
        match key {
            Key::Char(character) => self.draft.push(character),
            Key::Backspace => {
                self.draft.pop();
            }
            Key::Escape => self.draft.clear(),
            Key::Enter => return self.commit(),
        }
        self
    }

    fn clicked(mut self, id: HitId) -> Self {
        self.notice = None;
        match id {
            ADD_BUTTON => return self.commit(),
            CLEAR_BUTTON => {
                let before = self.todos.len();
                self.todos.retain(|todo| !todo.done);
                if self.todos.len() == before {
                    self.notice = Some("nothing completed to clear".into());
                }
            }
            // Deletes are checked first: their base is the higher one.
            HitId(raw) if raw >= DELETE_BASE => {
                let index = (raw - DELETE_BASE) as usize;
                if index < self.todos.len() {
                    self.todos.remove(index);
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

    /// Enter and the Add button mean the same thing, so they are one function.
    fn commit(mut self) -> Self {
        match add_todo(&self.todos, &self.draft) {
            Ok(todos) => {
                self.todos = todos;
                self.draft.clear();
            }
            // §7: a rejected action surfaces a notice. Nothing crashes, and
            // nothing is silently dropped either.
            Err(error) => self.notice = Some(error.notice()),
        }
        self
    }
}

/// A compact ✕ for one row. Small enough not to compete with the checkbox it
/// sits beside, and a hit target in its own right.
fn delete_button(theme: &Theme, id: HitId) -> Node {
    Node::row(
        Style {
            id: Some(id),
            width: Sizing::Fixed(24.0),
            height: Sizing::Fixed(24.0),
            background: Some(theme.muted),
            main_axis: Align::Center,
            cross_axis: Align::Center,
            ..Default::default()
        },
        vec![Node::text("x", TextStyle { size: 13.0, color: theme.text })],
    )
}

/// One todo: toggle it, or throw it away.
fn todo_row(theme: &Theme, index: usize, todo: &Todo) -> Node {
    Node::row(
        Style {
            width: Sizing::Grow,
            gap: theme.gap,
            cross_axis: Align::Center,
            main_axis: Align::SpaceBetween,
            ..Default::default()
        },
        vec![
            checkbox(
                theme,
                HitId(CHECKBOX_BASE + index as u32),
                todo.done,
                todo.title.clone(),
            ),
            delete_button(theme, HitId(DELETE_BASE + index as u32)),
        ],
    )
}

/// The view: state in, tree out. Pure, so it can be re-run every frame.
fn view(theme: &Theme, state: &State) -> Node {
    let rows: Vec<Node> =
        state.todos.iter().enumerate().map(|(index, todo)| todo_row(theme, index, todo)).collect();

    let done = state.todos.iter().filter(|todo| todo.done).count();
    let mut children = vec![
        label(theme, format!("todo — {done}/{} done", state.todos.len())),
        Node::row(
            Style { width: Sizing::Grow, gap: theme.gap, cross_axis: Align::Center, ..Default::default() },
            vec![
                text_input(
                    theme,
                    INPUT,
                    &state.draft,
                    "what needs doing?",
                    state.focus == Some(INPUT),
                ),
                button(theme, ADD_BUTTON, "Add"),
            ],
        ),
    ];
    if let Some(notice) = &state.notice {
        children.push(muted_label(theme, notice.clone()));
    }
    // Grow: the list takes whatever the rest of the screen leaves it, and
    // scrolls when the todos outrun that.
    children.push(scroll(theme, LIST, state.scroll, Sizing::Grow, rows));
    children.push(Node::row(
        Style { gap: theme.gap, ..Default::default() },
        vec![button(theme, CLEAR_BUTTON, "Clear done")],
    ));

    screen(theme, children)
}

/// Prints the todo UI as a laid-out tree (§8.4), without opening a window.
///
/// This is the inspector's most useful form for anyone who cannot see the
/// screen: it reports where every node actually ended up, so a layout can be
/// checked without a screenshot.
pub fn inspect(viewport: (f32, f32)) -> String {
    strand_render::inspect::describe_with_fonts(&view(&Theme::default(), &State::new()), viewport)
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
    println!("click the field and type; Enter or Add commits; x deletes; wheel scrolls");
    println!("press F12 for the debug overlay (§8.4)");
    strand_render::run_with(Some(scene_receiver), Some(input_sender))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click(id: HitId) -> InputEvent {
        InputEvent::PointerDown { id, x: 0.0, y: 0.0 }
    }

    fn typing(state: State, text: &str) -> State {
        text.chars().fold(state, |state, character| {
            state.update(InputEvent::Key { id: INPUT, key: Key::Char(character) })
        })
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
    fn typing_fills_the_draft_without_touching_the_todos() {
        let state = typing(State::new(), "buy milk");
        assert_eq!(state.draft, "buy milk");
        assert_eq!(state.todos.len(), 3, "nothing is committed until it is");
    }

    #[test]
    fn backspace_takes_the_last_character_back() {
        let state = typing(State::new(), "milk");
        let state = state.update(InputEvent::Key { id: INPUT, key: Key::Backspace });
        assert_eq!(state.draft, "mil");
    }

    #[test]
    fn backspace_on_an_empty_draft_is_not_an_error() {
        let state = State::new().update(InputEvent::Key { id: INPUT, key: Key::Backspace });
        assert_eq!(state.draft, "");
    }

    #[test]
    fn escape_abandons_the_draft() {
        let state = typing(State::new(), "never mind");
        let state = state.update(InputEvent::Key { id: INPUT, key: Key::Escape });
        assert_eq!(state.draft, "");
        assert_eq!(state.todos.len(), 3, "and adds nothing");
    }

    #[test]
    fn enter_and_the_add_button_do_the_same_thing() {
        let by_key = typing(State::new(), "buy milk")
            .update(InputEvent::Key { id: INPUT, key: Key::Enter });
        let by_click = typing(State::new(), "buy milk").update(click(ADD_BUTTON));
        assert_eq!(by_key, by_click);
        assert_eq!(by_key.todos.len(), 4);
        assert_eq!(by_key.todos[3].title, "buy milk");
        assert!(!by_key.todos[3].done, "a new todo starts undone");
        assert_eq!(by_key.draft, "", "and the field is ready for the next one");
    }

    #[test]
    fn an_empty_title_is_a_notice_rather_than_a_todo() {
        // §7's demo beat: the validation error surfaces, nothing crashes.
        let state = State::new().update(click(ADD_BUTTON));
        assert_eq!(state.notice.as_deref(), Some("a todo needs a title"));
        assert_eq!(state.todos.len(), 3);
    }

    #[test]
    fn whitespace_alone_is_not_a_title() {
        let state = typing(State::new(), "   ").update(click(ADD_BUTTON));
        assert_eq!(state.notice.as_deref(), Some("a todo needs a title"));
    }

    #[test]
    fn a_title_is_trimmed_before_it_is_kept() {
        let state = typing(State::new(), "  buy milk  ").update(click(ADD_BUTTON));
        assert_eq!(state.todos[3].title, "buy milk");
    }

    #[test]
    fn an_over_long_title_says_how_long_is_too_long() {
        let long = "x".repeat(MAX_TITLE + 1);
        let state = typing(State::new(), &long).update(click(ADD_BUTTON));
        assert_eq!(state.notice.as_deref(), Some("keep it under 60 characters"));
        assert_eq!(state.todos.len(), 3);
        assert_eq!(state.draft, long, "and what was typed is still there to fix");
    }

    #[test]
    fn the_error_is_in_the_type_not_in_a_convention() {
        // The point of §4.5's shape: a caller cannot get the list back without
        // having said what it does about the failure.
        assert_eq!(add_todo(&[], ""), Err(AddError::EmptyTitle));
        assert_eq!(add_todo(&[], &"x".repeat(61)), Err(AddError::TooLong { max: 60 }));
        assert!(add_todo(&[], "fine").is_ok());
    }

    #[test]
    fn deleting_removes_that_row_and_leaves_the_rest() {
        let state = State::new().update(click(HitId(DELETE_BASE)));
        assert_eq!(state.todos.len(), 2);
        assert_eq!(state.todos[0].title, "write the runtime", "the first one went");
    }

    #[test]
    fn deleting_a_row_that_is_gone_changes_nothing() {
        let state = State::new();
        let after = state.clone().update(click(HitId(DELETE_BASE + 99)));
        assert_eq!(after.todos, state.todos);
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
        let state = State::new().update(click(ADD_BUTTON));
        assert!(state.notice.is_some());
        let state = state.update(click(CLEAR_BUTTON));
        assert_ne!(state.notice.as_deref(), Some("a todo needs a title"));
    }

    #[test]
    fn clicks_that_hit_nothing_known_change_nothing() {
        let state = State::new();
        let after = state.clone().update(click(HitId(999)));
        assert_eq!(after.todos, state.todos);
    }

    #[test]
    fn focus_is_the_platforms_to_report_and_the_views_to_draw() {
        let state = State::new();
        assert_eq!(state.focus, None);
        let state = state.update(InputEvent::FocusChanged { id: Some(INPUT) });
        assert_eq!(state.focus, Some(INPUT));

        let mut layouter = strand_render::scene::Layouter::new();
        let focused = layouter.layout(&view(&Theme::default(), &state), (600.0, 400.0)).clone();
        let blurred = state.update(InputEvent::FocusChanged { id: None });
        let blurred = layouter.layout(&view(&Theme::default(), &blurred), (600.0, 400.0));
        assert!(
            focused.commands.len() > blurred.commands.len(),
            "the caret is one more command, and only while focused"
        );
    }

    #[test]
    fn the_scroll_offset_is_taken_from_the_platform() {
        let state = State::new().update(InputEvent::Scroll { id: LIST, offset: 42.0 });
        assert_eq!(state.scroll, 42.0);
        // An event for some other node is not this list's business.
        let state = state.update(InputEvent::Scroll { id: HitId(77), offset: 5.0 });
        assert_eq!(state.scroll, 42.0);
    }

    #[test]
    fn a_long_list_scrolls_rather_than_overflowing() {
        let mut state = State::new();
        state.todos = (0..40)
            .map(|i| Todo { title: format!("todo {i}"), done: false })
            .collect();

        let mut layouter = strand_render::scene::Layouter::new();
        let frame = layouter.layout(&view(&Theme::default(), &state), (600.0, 400.0));

        let extent = frame.scrolls.first().expect("the list should report itself scrollable");
        assert_eq!(extent.id, LIST);
        assert!(extent.max_offset > 0.0, "forty todos do not fit in 400px");
        // And the rows past the bottom are not clickable.
        assert!(
            frame.hits.iter().filter(|r| r.id.0 >= CHECKBOX_BASE).count() < 40,
            "only the visible rows are hit targets"
        );
    }

    #[test]
    fn every_todo_gets_its_own_pair_of_hit_targets() {
        let state = State::new();
        let tree = view(&Theme::default(), &state);
        let mut layouter = strand_render::scene::Layouter::new();
        let frame = layouter.layout(&tree, (600.0, 400.0));

        let ids: Vec<u32> = frame.hits.iter().map(|region| region.id.0).collect();
        assert_eq!(
            ids,
            vec![
                INPUT.0,
                ADD_BUTTON.0,
                LIST.0,
                CHECKBOX_BASE,
                DELETE_BASE,
                CHECKBOX_BASE + 1,
                DELETE_BASE + 1,
                CHECKBOX_BASE + 2,
                DELETE_BASE + 2,
                CLEAR_BUTTON.0,
            ],
            "field, button, list, then a toggle and a delete per todo, in paint order"
        );
    }

    #[test]
    fn only_the_field_takes_focus() {
        let tree = view(&Theme::default(), &State::new());
        let mut layouter = strand_render::scene::Layouter::new();
        let frame = layouter.layout(&tree, (600.0, 400.0));

        let focusable: Vec<u32> =
            frame.hits.iter().filter(|r| r.focusable).map(|r| r.id.0).collect();
        assert_eq!(focusable, vec![INPUT.0], "clicking anything else stops typing");
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
