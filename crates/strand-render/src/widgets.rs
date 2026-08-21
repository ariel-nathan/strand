//! The POC widget set (§6.4), built from the layout primitives.
//!
//! These are plain functions returning `Node` — §4.2's "UI is functions, not
//! classes", and the shape §6.2's builder DSL will desugar to once the
//! language grows view syntax. Nothing here is a framework: a widget is a tree
//! its caller could have written by hand.
//!
//! Styling is typed props with no cascade (§6.3), so a widget's appearance
//! comes from a theme value passed in, never from ambient global state.

use crate::scene::{Align, Color, HitId, Node, Sizing, Style, TextStyle};

/// Typed theme constants, the §6.3 answer to design tokens. A plain struct:
/// unused fields are dead code the compiler can see.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub surface: Color,
    pub raised: Color,
    pub accent: Color,
    pub text: Color,
    pub muted: Color,
    pub gap: f32,
    pub padding: f32,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            surface: Color::rgb(0.07, 0.07, 0.09),
            raised: Color::rgb(0.13, 0.14, 0.18),
            accent: Color::rgb(0.35, 0.55, 0.95),
            text: Color::rgb(0.90, 0.90, 0.93),
            muted: Color::rgb(0.45, 0.46, 0.52),
            gap: 8.0,
            padding: 12.0,
        }
    }
}

/// A text run in the theme's body colour.
pub fn label(theme: &Theme, text: impl Into<String>) -> Node {
    Node::text(text, TextStyle { size: 16.0, color: theme.text })
}

/// Secondary text — the same widget, a different token.
pub fn muted_label(theme: &Theme, text: impl Into<String>) -> Node {
    Node::text(text, TextStyle { size: 14.0, color: theme.muted })
}

/// A clickable box with a label. `id` is what input events will name, so it
/// must be stable across frames for clicks to keep landing on the same button.
pub fn button(theme: &Theme, id: HitId, text: impl Into<String>) -> Node {
    Node::row(
        Style {
            id: Some(id),
            padding: theme.padding,
            background: Some(theme.accent),
            main_axis: Align::Center,
            cross_axis: Align::Center,
            ..Default::default()
        },
        vec![label(theme, text)],
    )
}

/// A checkbox and its label, as one hit target: clicking the text toggles it,
/// which is the behaviour people expect and the reason the row carries the id.
pub fn checkbox(theme: &Theme, id: HitId, checked: bool, text: impl Into<String>) -> Node {
    let mark = Node::Box {
        style: Style {
            width: Sizing::Fixed(18.0),
            height: Sizing::Fixed(18.0),
            background: Some(if checked { theme.accent } else { theme.muted }),
            ..Default::default()
        },
    };

    Node::row(
        Style {
            id: Some(id),
            gap: theme.gap,
            cross_axis: Align::Center,
            ..Default::default()
        },
        vec![mark, label(theme, text)],
    )
}

/// A panel: a raised surface that lays its children out vertically.
pub fn panel(theme: &Theme, children: Vec<Node>) -> Node {
    Node::column(
        Style {
            width: Sizing::Grow,
            padding: theme.padding,
            gap: theme.gap,
            background: Some(theme.raised),
            ..Default::default()
        },
        children,
    )
}

/// The size a field types at, and the line box that follows from it.
///
/// `text::FontMeasure` gives a run a line height of `size * 1.25`, and the
/// caret is exactly one line tall so that a field holding nothing but a caret
/// is the same height as one holding text. If that factor ever changes, the
/// field height stops being constant across states — which is what
/// `a_field_is_the_same_size_in_every_state` is there to catch.
const FIELD_SIZE: f32 = 16.0;
const FIELD_LINE: f32 = FIELD_SIZE * 1.25;
const CARET_WIDTH: f32 = 2.0;

/// A single-line text field (§6.4).
///
/// The caret is a sibling box, not a measured position. Layout places it
/// immediately after the glyphs because that is where the next sibling goes —
/// so it lands exactly right, measured by the same font that drew the text,
/// with no measuring code in the widget at all.
///
/// **The prompt and the caret never share the field.** A caret has to occupy
/// layout space — §6.3 has no out-of-flow positioning, and floating elements
/// with attach points, which are the mechanism it does specify, are for
/// tooltips and modals rather than for something inline. So wherever the caret
/// sits, everything after it moves to make room: showing both meant the prompt
/// jumped 4px right when the field took focus and 4px back on the first
/// keystroke. Hiding the prompt on focus removes the state instead of
/// compensating for it, and the value's left edge is then the same in all four
/// combinations. The prompt says what the field is for; focus has answered that.
///
/// `focused` comes from the app's state. The platform decides *where* focus is
/// and says so in an event; what a focused field looks like is the view's
/// business (§6.5).
pub fn text_input(
    theme: &Theme,
    id: HitId,
    value: &str,
    placeholder: &str,
    focused: bool,
) -> Node {
    let caret = || Node::Box {
        style: Style {
            width: Sizing::Fixed(CARET_WIDTH),
            height: Sizing::Fixed(FIELD_LINE),
            background: Some(theme.accent),
            ..Default::default()
        },
    };
    let run = |text: &str, color| Node::text(text, TextStyle { size: FIELD_SIZE, color });

    let children = match (value.is_empty(), focused) {
        (true, true) => vec![caret()],
        (true, false) => vec![run(placeholder, theme.muted)],
        (false, true) => vec![run(value, theme.text), caret()],
        (false, false) => vec![run(value, theme.text)],
    };

    Node::row(
        Style {
            id: Some(id),
            focusable: true,
            width: Sizing::Grow,
            // Fixed, not fitted: a row that sizes to its contents is a row that
            // changes height when the contents change, and the contents here
            // change on every keystroke.
            height: Sizing::Fixed(FIELD_LINE + theme.padding * 2.0),
            padding: theme.padding,
            gap: CARET_WIDTH,
            background: Some(theme.raised),
            cross_axis: Align::Center,
            ..Default::default()
        },
        children,
    )
}

/// A panel you can scroll: raised surface, clipped content, and an indicator
/// when there is more than fits (§6.4).
///
/// `offset` is the app's, not the platform's (§6.5). The platform clamps it
/// against the content it just measured and reports the clamped value back as
/// an event, so the app can hold a scroll position but never an impossible one.
pub fn scroll(theme: &Theme, id: HitId, offset: f32, height: Sizing, children: Vec<Node>) -> Node {
    Node::Scroll {
        style: Style {
            id: Some(id),
            width: Sizing::Grow,
            height,
            padding: theme.padding,
            gap: theme.gap,
            background: Some(theme.raised),
            ..Default::default()
        },
        offset,
        bar: Some(theme.muted),
        children,
    }
}

/// The window-filling background every screen starts from.
pub fn screen(theme: &Theme, children: Vec<Node>) -> Node {
    Node::column(
        Style {
            width: Sizing::Grow,
            height: Sizing::Grow,
            padding: theme.padding,
            gap: theme.gap,
            background: Some(theme.surface),
            ..Default::default()
        },
        children,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{Command, Layouter};

    fn theme() -> Theme {
        Theme::default()
    }

    #[test]
    fn a_button_is_one_hit_target_covering_its_label() {
        let tree = screen(&theme(), vec![button(&theme(), HitId(1), "Add")]);
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&tree, (400.0, 200.0));

        assert_eq!(frame.hits.len(), 1, "the label is not separately clickable");
        let region = frame.hits[0];
        assert_eq!(region.id, HitId(1));
        assert!(region.width > 0.0 && region.height > 0.0, "it has an area to click");
        // A click in the middle of the button lands on it.
        assert_eq!(
            frame.hit_test(region.x + region.width / 2.0, region.y + region.height / 2.0),
            Some(HitId(1))
        );
    }

    #[test]
    fn a_checkbox_label_is_part_of_the_hit_target() {
        let tree = screen(&theme(), vec![checkbox(&theme(), HitId(2), false, "done")]);
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&tree, (400.0, 200.0));

        let region = frame.hits[0];
        // The far end of the row is the text, and it must still toggle.
        let near_text_end = region.x + region.width - 1.0;
        assert_eq!(frame.hit_test(near_text_end, region.y + 2.0), Some(HitId(2)));
    }

    #[test]
    fn checking_a_box_changes_only_its_colour() {
        let mut layouter = Layouter::new();
        let off = layouter.layout(&checkbox(&theme(), HitId(3), false, "x"), (200.0, 50.0)).clone();
        let on = layouter.layout(&checkbox(&theme(), HitId(3), true, "x"), (200.0, 50.0)).clone();

        assert_eq!(off.commands.len(), on.commands.len(), "the same shapes either way");
        let colour = |frame: &crate::scene::Frame| match frame.commands[0] {
            Command::Rect { color, .. } => color,
            _ => panic!("expected the mark first"),
        };
        assert_ne!(colour(&off), colour(&on));
        assert_eq!(colour(&on), theme().accent);
    }

    #[test]
    fn widgets_compose_without_a_framework() {
        // A small screen: two buttons and two checkboxes, four hit targets,
        // all of it plain function calls.
        let tree = screen(
            &theme(),
            vec![
                panel(
                    &theme(),
                    vec![
                        checkbox(&theme(), HitId(10), true, "write the compiler"),
                        checkbox(&theme(), HitId(11), false, "write the renderer"),
                    ],
                ),
                Node::row(
                    Style { gap: theme().gap, ..Default::default() },
                    vec![button(&theme(), HitId(20), "Add"), button(&theme(), HitId(21), "Clear")],
                ),
            ],
        );

        let mut layouter = Layouter::new();
        let frame = layouter.layout(&tree, (500.0, 300.0));
        let ids: Vec<u32> = frame.hits.iter().map(|region| region.id.0).collect();
        assert_eq!(ids, vec![10, 11, 20, 21], "hit regions follow paint order");
    }

    /// Where the field puts its text and its caret, for a given state.
    fn field(value: &str, focused: bool) -> (Option<f32>, Option<f32>, (f32, f32)) {
        let mut layouter = Layouter::new();
        let tree = text_input(&theme(), HitId(1), value, "what needs doing?", focused);
        let frame = layouter.layout(&tree, (400.0, 80.0)).clone();

        let text = frame.commands.iter().find_map(|c| match c {
            Command::Text { x, .. } => Some(*x),
            _ => None,
        });
        let caret = frame.commands.iter().rev().find_map(|c| match c {
            Command::Rect { x, width, .. } if *width == CARET_WIDTH => Some(*x),
            _ => None,
        });
        let region = frame.hits[0];
        (text, caret, (region.width, region.height))
    }

    #[test]
    fn the_value_does_not_move_when_the_field_takes_focus() {
        // The bug this exists for, reported from the running app: with the
        // prompt and the caret both on screen, the prompt jumped 4px right on
        // focus and 4px back on the first keystroke. The caret has to occupy
        // layout space, so the fix was to stop the two from coexisting.
        let (empty_blur, _, _) = field("", false);
        let (empty_focus, caret_focus, _) = field("", true);
        let (typed_focus, _, _) = field("milk", true);
        let (typed_blur, _, _) = field("milk", false);

        let left = empty_blur.expect("the prompt is drawn when unfocused");
        assert_eq!(empty_focus, None, "focus replaces the prompt rather than sharing with it");
        assert_eq!(caret_focus, Some(left), "and the caret starts exactly where text would");
        assert_eq!(typed_focus, Some(left), "typing does not move it either");
        assert_eq!(typed_blur, Some(left), "nor does losing focus");
    }

    #[test]
    fn a_field_is_the_same_size_in_every_state() {
        // A field holding only a caret must be as tall as one holding text, or
        // the row jitters vertically on focus instead of horizontally.
        let sizes: Vec<(f32, f32)> = [("", false), ("", true), ("milk", true), ("milk", false)]
            .into_iter()
            .map(|(value, focused)| field(value, focused).2)
            .collect();
        assert!(
            sizes.windows(2).all(|pair| pair[0] == pair[1]),
            "the field changes size between states: {sizes:?}"
        );
    }

    #[test]
    fn a_focused_field_shows_a_caret_after_what_was_typed() {
        let (text_x, caret_x, _) = field("milk", true);
        let text_x = text_x.expect("the value should be drawn");
        let caret_x = caret_x.expect("a focused field should show a caret");
        assert!(caret_x > text_x, "the caret sits after the text, at {caret_x}");
    }

    #[test]
    fn an_unfocused_field_shows_no_caret() {
        assert_eq!(field("milk", false).1, None, "the caret is what focus looks like");
        assert_eq!(field("", false).1, None);
    }

    #[test]
    fn an_empty_unfocused_field_prompts_in_muted_text() {
        let mut layouter = Layouter::new();
        let tree = text_input(&theme(), HitId(1), "", "what needs doing?", false);
        let frame = layouter.layout(&tree, (400.0, 80.0));
        let (text, color) = frame
            .commands
            .iter()
            .find_map(|c| match c {
                Command::Text { text, color, .. } => Some((text.clone(), *color)),
                _ => None,
            })
            .expect("the placeholder should be drawn");
        assert_eq!(text, "what needs doing?");
        assert_eq!(color, theme().muted, "a prompt is not the value");
    }

    #[test]
    fn a_text_field_takes_focus_and_a_button_does_not() {
        // The rule the platform routes keys by: clicking a field starts typing,
        // clicking anything else stops it.
        let mut layouter = Layouter::new();
        let tree = screen(
            &theme(),
            vec![
                text_input(&theme(), HitId(1), "", "type here", false),
                button(&theme(), HitId(2), "Add"),
            ],
        );
        let frame = layouter.layout(&tree, (400.0, 200.0));
        let focusable: Vec<(u32, bool)> =
            frame.hits.iter().map(|r| (r.id.0, r.focusable)).collect();
        assert_eq!(focusable, vec![(1, true), (2, false)]);
    }

    #[test]
    fn a_scroll_reports_how_far_it_could_go() {
        let mut layouter = Layouter::new();
        let rows: Vec<Node> = (0..20)
            .map(|i| checkbox(&theme(), HitId(100 + i), false, format!("row {i}")))
            .collect();
        let tree = screen(
            &theme(),
            vec![scroll(&theme(), HitId(9), 0.0, Sizing::Fixed(100.0), rows)],
        );
        let frame = layouter.layout(&tree, (400.0, 300.0));

        let extent = frame.scrolls.first().expect("the scroll should report itself");
        assert_eq!(extent.id, HitId(9));
        assert!(extent.max_offset > 0.0, "twenty rows do not fit in 100px");
        assert_eq!(extent.offset, 0.0);
    }

    #[test]
    fn labels_carry_theme_colours_rather_than_inheriting_them() {
        // §6.3: no cascade. A label's colour comes from the token it was given.
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&muted_label(&theme(), "note"), (200.0, 50.0));
        let Command::Text { color, .. } = frame.commands[0] else {
            panic!("expected text");
        };
        assert_eq!(color, theme().muted);
    }
}
