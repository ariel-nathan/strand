//! UI tree, layout, and the render command array (§6.1).
//!
//! Layout resolves a tree into a **flat list of draw commands**. That is clay's
//! architecture, adopted for the reasons §6.1 gives: the output is
//! renderer-agnostic, trivially diffable, and cheap to send across an actor
//! channel — a slow app actor delays its own frame, never the compositor's.
//!
//! Sizing uses §6.3's four-word vocabulary (`fit`, `grow`, `fixed`, `percent`)
//! and Flutter-style `main_axis` / `cross_axis` naming, so the meaning does not
//! flip when a row becomes a column.

use taffy::prelude::*;

/// Straight RGBA, 0..1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub const fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self::rgba(r, g, b, 1.0)
    }
}

/// §6.3: the whole sizing vocabulary. No width/height/min/max/basis/grow/shrink
/// interplay to reason about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sizing {
    /// Shrink to the content.
    Fit,
    /// Take the free space, sharing it with other `Grow` siblings.
    Grow,
    /// Exactly this many logical pixels.
    Fixed(f32),
    /// A fraction of the parent, 0..1.
    Percent(f32),
}

impl Sizing {
    fn to_dimension(self) -> Dimension {
        match self {
            Sizing::Fixed(px) => length(px),
            Sizing::Percent(fraction) => percent(fraction),
            Sizing::Fit | Sizing::Grow => Dimension::auto(),
        }
    }

    fn grow_factor(self) -> f32 {
        match self {
            Sizing::Grow => 1.0,
            _ => 0.0,
        }
    }
}

/// Alignment along an axis. One mechanism, not five (§6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    pub width: Sizing,
    pub height: Sizing,
    pub padding: f32,
    pub gap: f32,
    /// `None` paints nothing — the node is pure layout.
    pub background: Option<Color>,
    pub main_axis: Align,
    pub cross_axis: Align,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            width: Sizing::Fit,
            height: Sizing::Fit,
            padding: 0.0,
            gap: 0.0,
            background: None,
            main_axis: Align::Start,
            cross_axis: Align::Start,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextStyle {
    pub size: f32,
    pub color: Color,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self { size: 16.0, color: Color::rgb(0.9, 0.9, 0.92) }
    }
}

/// A node in the UI tree. Built fresh each frame by a view function; the
/// platform owns everything that happens after (§6.1).
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Row { style: Style, children: Vec<Node> },
    Column { style: Style, children: Vec<Node> },
    /// A leaf that paints its background and nothing else.
    Box { style: Style },
    Text { text: String, style: TextStyle },
}

impl Node {
    pub fn row(style: Style, children: Vec<Node>) -> Self {
        Node::Row { style, children }
    }

    pub fn column(style: Style, children: Vec<Node>) -> Self {
        Node::Column { style, children }
    }

    pub fn text(text: impl Into<String>, style: TextStyle) -> Self {
        Node::Text { text: text.into(), style }
    }
}

/// One drawing instruction. Deliberately flat and free of tree structure, so a
/// frame is a `Vec` that can be diffed, serialised, or replayed.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Rect { x: f32, y: f32, width: f32, height: f32, color: Color },
    Text { x: f32, y: f32, size: f32, color: Color, text: String },
}

/// A finished frame: draw these in order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frame {
    pub commands: Vec<Command>,
}

impl Frame {
    /// Reuses the backing allocation. §6.1's per-frame arena, in the form Rust
    /// makes natural: keep the capacity, drop the contents.
    pub fn clear(&mut self) {
        self.commands.clear();
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// Text measurement. One monospace-ish approximation until glyphon lands —
/// §12 calls text a tarpit, and pretending otherwise here would hide that.
const ADVANCE: f32 = 0.55;
const LINE_HEIGHT: f32 = 1.25;

fn measure_text(text: &str, size: f32) -> (f32, f32) {
    (text.chars().count() as f32 * size * ADVANCE, size * LINE_HEIGHT)
}

/// Turns UI trees into command arrays, reusing its allocations between frames.
pub struct Layouter {
    frame: Frame,
}

impl Default for Layouter {
    fn default() -> Self {
        Self::new()
    }
}

impl Layouter {
    pub fn new() -> Self {
        Self { frame: Frame::default() }
    }

    /// Lays `root` out in a `viewport` and returns the commands to paint it.
    pub fn layout(&mut self, root: &Node, viewport: (f32, f32)) -> &Frame {
        self.frame.clear();

        let mut tree: TaffyTree<()> = TaffyTree::new();
        let Ok(node) = build(&mut tree, root) else { return &self.frame };

        let space = Size {
            width: AvailableSpace::Definite(viewport.0),
            height: AvailableSpace::Definite(viewport.1),
        };
        if tree.compute_layout(node, space).is_err() {
            return &self.frame;
        }

        emit(&tree, node, root, 0.0, 0.0, &mut self.frame);
        &self.frame
    }
}

fn taffy_style(style: &Style, direction: FlexDirection) -> taffy::Style {
    taffy::Style {
        display: Display::Flex,
        flex_direction: direction,
        size: Size { width: style.width.to_dimension(), height: style.height.to_dimension() },
        flex_grow: style.width.grow_factor().max(style.height.grow_factor()),
        padding: Rect::length(style.padding),
        gap: Size { width: length(style.gap), height: length(style.gap) },
        justify_content: Some(match style.main_axis {
            Align::Start => JustifyContent::FLEX_START,
            Align::Center => JustifyContent::CENTER,
            Align::End => JustifyContent::FLEX_END,
            Align::SpaceBetween => JustifyContent::SPACE_BETWEEN,
        }),
        align_items: Some(match style.cross_axis {
            Align::Start => AlignItems::FLEX_START,
            Align::Center => AlignItems::CENTER,
            Align::End => AlignItems::FLEX_END,
            // Stretching is the sensible reading of "space between" on the
            // cross axis, where there is only one item per line.
            Align::SpaceBetween => AlignItems::STRETCH,
        }),
        ..Default::default()
    }
}

fn build(tree: &mut TaffyTree<()>, node: &Node) -> Result<NodeId, taffy::TaffyError> {
    match node {
        Node::Row { style, children } | Node::Column { style, children } => {
            let direction = match node {
                Node::Row { .. } => FlexDirection::Row,
                _ => FlexDirection::Column,
            };
            let ids: Result<Vec<NodeId>, _> =
                children.iter().map(|child| build(tree, child)).collect();
            tree.new_with_children(taffy_style(style, direction), &ids?)
        }
        Node::Box { style } => tree.new_leaf(taffy_style(style, FlexDirection::Row)),
        Node::Text { text, style } => {
            let (width, height) = measure_text(text, style.size);
            tree.new_leaf(taffy::Style {
                size: Size { width: length(width), height: length(height) },
                ..Default::default()
            })
        }
    }
}

/// Walks the laid-out tree, accumulating absolute positions as it goes —
/// taffy reports each node's location relative to its parent.
fn emit(tree: &TaffyTree<()>, id: NodeId, node: &Node, x: f32, y: f32, frame: &mut Frame) {
    let Ok(layout) = tree.layout(id) else { return };
    let (x, y) = (x + layout.location.x, y + layout.location.y);
    let (width, height) = (layout.size.width, layout.size.height);

    match node {
        Node::Row { style, children } | Node::Column { style, children } => {
            if let Some(color) = style.background {
                frame.commands.push(Command::Rect { x, y, width, height, color });
            }
            let ids = tree.children(id).unwrap_or_default();
            for (child_id, child) in ids.into_iter().zip(children) {
                emit(tree, child_id, child, x, y, frame);
            }
        }
        Node::Box { style } => {
            if let Some(color) = style.background {
                frame.commands.push(Command::Rect { x, y, width, height, color });
            }
        }
        Node::Text { text, style } => {
            frame.commands.push(Command::Text {
                x,
                y,
                size: style.size,
                color: style.color,
                text: text.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Color = Color::rgb(1.0, 0.0, 0.0);
    const BLUE: Color = Color::rgb(0.0, 0.0, 1.0);

    fn boxed(width: f32, height: f32, color: Color) -> Node {
        Node::Box {
            style: Style {
                width: Sizing::Fixed(width),
                height: Sizing::Fixed(height),
                background: Some(color),
                ..Default::default()
            },
        }
    }

    fn rects(frame: &Frame) -> Vec<(f32, f32, f32, f32)> {
        frame
            .commands
            .iter()
            .filter_map(|c| match c {
                Command::Rect { x, y, width, height, .. } => Some((*x, *y, *width, *height)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_column_stacks_children_with_the_gap_between_them() {
        let tree = Node::column(
            Style { gap: 10.0, ..Default::default() },
            vec![boxed(50.0, 20.0, RED), boxed(50.0, 30.0, BLUE)],
        );
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&tree, (200.0, 200.0));

        assert_eq!(
            rects(frame),
            vec![(0.0, 0.0, 50.0, 20.0), (0.0, 30.0, 50.0, 30.0)],
            "the second box starts after the first plus the gap"
        );
    }

    #[test]
    fn a_row_lays_children_out_along_x() {
        let tree = Node::row(
            Style { gap: 8.0, ..Default::default() },
            vec![boxed(40.0, 10.0, RED), boxed(20.0, 10.0, BLUE)],
        );
        let mut layouter = Layouter::new();
        assert_eq!(
            rects(layouter.layout(&tree, (200.0, 200.0))),
            vec![(0.0, 0.0, 40.0, 10.0), (48.0, 0.0, 20.0, 10.0)]
        );
    }

    #[test]
    fn padding_insets_children() {
        let tree = Node::column(
            Style { padding: 12.0, ..Default::default() },
            vec![boxed(10.0, 10.0, RED)],
        );
        let mut layouter = Layouter::new();
        assert_eq!(rects(layouter.layout(&tree, (200.0, 200.0)))[0], (12.0, 12.0, 10.0, 10.0));
    }

    #[test]
    fn grow_takes_the_remaining_space() {
        let tree = Node::row(
            Style { width: Sizing::Fixed(100.0), height: Sizing::Fixed(10.0), ..Default::default() },
            vec![
                boxed(30.0, 10.0, RED),
                Node::Box {
                    style: Style {
                        width: Sizing::Grow,
                        height: Sizing::Fixed(10.0),
                        background: Some(BLUE),
                        ..Default::default()
                    },
                },
            ],
        );
        let mut layouter = Layouter::new();
        let laid = rects(layouter.layout(&tree, (200.0, 200.0)));
        assert_eq!(laid[1], (30.0, 0.0, 70.0, 10.0), "the grower fills what is left");
    }

    #[test]
    fn percent_is_a_fraction_of_the_parent() {
        let tree = Node::row(
            Style { width: Sizing::Fixed(200.0), height: Sizing::Fixed(50.0), ..Default::default() },
            vec![Node::Box {
                style: Style {
                    width: Sizing::Percent(0.25),
                    height: Sizing::Fixed(50.0),
                    background: Some(RED),
                    ..Default::default()
                },
            }],
        );
        let mut layouter = Layouter::new();
        assert_eq!(rects(layouter.layout(&tree, (400.0, 400.0)))[0].2, 50.0);
    }

    #[test]
    fn centring_is_one_property_not_five() {
        // §6.3's answer to "how do I centre a div".
        let tree = Node::row(
            Style {
                width: Sizing::Fixed(100.0),
                height: Sizing::Fixed(100.0),
                main_axis: Align::Center,
                cross_axis: Align::Center,
                ..Default::default()
            },
            vec![boxed(20.0, 20.0, RED)],
        );
        let mut layouter = Layouter::new();
        assert_eq!(rects(layouter.layout(&tree, (200.0, 200.0)))[0], (40.0, 40.0, 20.0, 20.0));
    }

    #[test]
    fn a_node_without_a_background_paints_nothing() {
        let tree = Node::column(Style::default(), vec![boxed(10.0, 10.0, RED)]);
        let mut layouter = Layouter::new();
        assert_eq!(layouter.layout(&tree, (100.0, 100.0)).len(), 1, "only the child paints");
    }

    #[test]
    fn text_becomes_a_text_command_at_its_laid_out_position() {
        let tree = Node::column(
            Style { padding: 5.0, ..Default::default() },
            vec![Node::text("hi", TextStyle::default())],
        );
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&tree, (100.0, 100.0));
        let Command::Text { x, y, text, .. } = &frame.commands[0] else {
            panic!("expected a text command, got {:?}", frame.commands);
        };
        assert_eq!((*x, *y), (5.0, 5.0));
        assert_eq!(text, "hi");
    }

    #[test]
    fn the_command_array_is_flat_regardless_of_nesting() {
        // Nesting is a layout concern; the output never reflects it.
        let deep = Node::column(
            Style::default(),
            vec![Node::row(
                Style::default(),
                vec![Node::column(Style::default(), vec![boxed(5.0, 5.0, RED)])],
            )],
        );
        let mut layouter = Layouter::new();
        assert_eq!(layouter.layout(&deep, (50.0, 50.0)).len(), 1);
    }

    #[test]
    fn frames_reuse_their_allocation() {
        // The per-frame arena, in practice: capacity survives, contents do not.
        let tree = Node::column(Style::default(), vec![boxed(1.0, 1.0, RED)]);
        let mut layouter = Layouter::new();
        layouter.layout(&tree, (10.0, 10.0));
        let capacity = layouter.frame.commands.capacity();
        layouter.layout(&tree, (10.0, 10.0));
        assert_eq!(layouter.frame.commands.capacity(), capacity, "no reallocation per frame");
        assert_eq!(layouter.frame.len(), 1, "and no leftovers from last frame");
    }
}
