//! The inspector (§8.4), in two halves.
//!
//! **Textual.** `describe` prints the laid-out tree with computed geometry.
//! The command array is deliberately flat, so once a frame exists there is no
//! way back to "which node is this" — this walks the tree and the layout
//! together, before flattening, and is the only view that can answer it.
//!
//! **Visual.** `Inspector::overlay` appends outline commands to a finished
//! frame. That is clay's trick, cited in `docs/inspiration-canon.md`: an
//! inspector needs no special rendering path because it is just more render
//! commands. Outlines go through the same pipeline as everything else.

use std::fmt::Write as _;

use crate::scene::{Color, Command, Frame, HitId, Node};

/// Outline colour for ordinary boxes.
const OUTLINE: Color = Color::rgba(0.20, 0.85, 0.80, 0.55);
/// Outline colour for the node under the pointer.
const HIGHLIGHT: Color = Color::rgba(1.0, 0.45, 0.20, 0.95);

#[derive(Debug, Clone, Copy, Default)]
pub struct Inspector {
    pub enabled: bool,
    /// The node under the pointer, drawn differently — the devtools habit of
    /// showing you what you are about to click.
    pub highlight: Option<HitId>,
}

impl Inspector {
    pub fn toggle(&mut self) {
        self.enabled = !self.enabled;
    }

    /// Appends overlay commands. Call after layout, before painting.
    pub fn overlay(&self, frame: &mut Frame) {
        if !self.enabled {
            return;
        }

        // Snapshot first: the outlines being appended must not be outlined.
        let boxes: Vec<(f32, f32, f32, f32)> = frame
            .commands
            .iter()
            .filter_map(|command| match command {
                Command::Rect { x, y, width, height, .. } => Some((*x, *y, *width, *height)),
                Command::Text { .. } => None,
            })
            .collect();

        for (x, y, width, height) in boxes {
            outline(frame, x, y, width, height, 1.0, OUTLINE);
        }

        if let Some(id) = self.highlight {
            if let Some(region) = frame.hits.iter().find(|region| region.id == id).copied() {
                outline(frame, region.x, region.y, region.width, region.height, 2.0, HIGHLIGHT);
            }
        }
    }
}

/// Four thin rects. A stroked rectangle primitive would be fewer commands, but
/// would also be a second thing the renderer has to understand.
fn outline(frame: &mut Frame, x: f32, y: f32, width: f32, height: f32, weight: f32, color: Color) {
    let mut edge = |x: f32, y: f32, width: f32, height: f32| {
        frame.commands.push(Command::Rect { x, y, width, height, color });
    };
    edge(x, y, width, weight);
    edge(x, y + height - weight, width, weight);
    edge(x, y, weight, height);
    edge(x + width - weight, y, weight, height);
}

/// Renders the laid-out tree as indented text.
///
/// Geometry comes from a real layout pass, so this reports where things
/// actually ended up rather than what the tree asked for.
pub fn describe(root: &Node, viewport: (f32, f32)) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "viewport {:.0}x{:.0}", viewport.0, viewport.1);
    crate::scene::walk_laid_out(root, viewport, &mut |node, depth, x, y, width, height| {
        let indent = "  ".repeat(depth + 1);
        let _ = write!(out, "{indent}{}", kind(node));
        if let Some(id) = node.style().and_then(|style| style.id) {
            let _ = write!(out, " #{}", id.0);
        }
        let _ = write!(out, "  ({x:.0},{y:.0} {width:.0}x{height:.0})");
        if let Some(color) = node.style().and_then(|style| style.background) {
            let _ = write!(out, "  bg {}", hex(color));
        }
        if let Node::Text { text, style } = node {
            let _ = write!(out, "  {text:?} {}", hex(style.color));
        }
        let _ = writeln!(out);
    });
    out
}

fn kind(node: &Node) -> &'static str {
    match node {
        Node::Row { .. } => "row",
        Node::Column { .. } => "column",
        Node::Box { .. } => "box",
        Node::Text { .. } => "text",
    }
}

fn hex(color: Color) -> String {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", channel(color.r), channel(color.g), channel(color.b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{Layouter, Sizing, Style, TextStyle};

    fn sample() -> Node {
        Node::column(
            Style {
                width: Sizing::Fixed(200.0),
                height: Sizing::Fixed(100.0),
                padding: 10.0,
                background: Some(Color::rgb(0.1, 0.1, 0.12)),
                ..Default::default()
            },
            vec![
                Node::Box {
                    style: Style {
                        id: Some(HitId(42)),
                        width: Sizing::Fixed(30.0),
                        height: Sizing::Fixed(20.0),
                        background: Some(Color::rgb(1.0, 0.0, 0.0)),
                        ..Default::default()
                    },
                },
                Node::text("hi", TextStyle::default()),
            ],
        )
    }

    #[test]
    fn describe_reports_where_things_actually_are() {
        let text = describe(&sample(), (400.0, 300.0));
        assert!(text.contains("viewport 400x300"), "{text}");
        assert!(text.contains("column  (0,0 200x100)"), "the root's real size:\n{text}");
        // Padding pushed the child in, and describe reports that rather than
        // the 0,0 the tree asked for.
        assert!(text.contains("box #42  (10,10 30x20)"), "the child's real position:\n{text}");
    }

    #[test]
    fn describe_shows_colours_and_text() {
        let text = describe(&sample(), (400.0, 300.0));
        assert!(text.contains("bg #ff0000"), "box colour:\n{text}");
        assert!(text.contains("\"hi\""), "text content:\n{text}");
    }

    #[test]
    fn describe_indents_by_depth() {
        let text = describe(&sample(), (400.0, 300.0));
        let lines: Vec<&str> = text.lines().collect();
        // viewport, root, then two children one level deeper.
        assert!(lines[1].starts_with("  column"), "{:?}", lines[1]);
        assert!(lines[2].starts_with("    box"), "{:?}", lines[2]);
    }

    #[test]
    fn the_overlay_is_off_by_default_and_adds_nothing() {
        let mut layouter = Layouter::new();
        let mut frame = layouter.layout(&sample(), (400.0, 300.0)).clone();
        let before = frame.commands.len();
        Inspector::default().overlay(&mut frame);
        assert_eq!(frame.commands.len(), before);
    }

    #[test]
    fn the_overlay_outlines_every_box_with_four_edges() {
        let mut layouter = Layouter::new();
        let mut frame = layouter.layout(&sample(), (400.0, 300.0)).clone();
        let rects = frame
            .commands
            .iter()
            .filter(|c| matches!(c, Command::Rect { .. }))
            .count();

        Inspector { enabled: true, highlight: None }.overlay(&mut frame);
        let after = frame.commands.iter().filter(|c| matches!(c, Command::Rect { .. })).count();
        assert_eq!(after, rects + rects * 4, "four edges per box, and no outlines of outlines");
    }

    #[test]
    fn highlighting_adds_one_more_outline_for_the_named_node() {
        let mut layouter = Layouter::new();
        let mut frame = layouter.layout(&sample(), (400.0, 300.0)).clone();
        let plain = {
            let mut copy = frame.clone();
            Inspector { enabled: true, highlight: None }.overlay(&mut copy);
            copy.commands.len()
        };
        Inspector { enabled: true, highlight: Some(HitId(42)) }.overlay(&mut frame);
        assert_eq!(frame.commands.len(), plain + 4, "the highlight is four more edges");
    }
}
