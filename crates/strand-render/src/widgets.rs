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
