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

    fn is_grow(self) -> bool {
        matches!(self, Sizing::Grow)
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

/// Identifies a node that can be hit by input. Assigned by whoever builds the
/// tree — §6.2 calls these stable IDs, and stability is what lets input keep
/// targeting the same thing across rebuilt frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HitId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    /// `Some` makes this node a target for input routing (§6.1).
    pub id: Option<HitId>,
    pub width: Sizing,
    pub height: Sizing,
    pub padding: f32,
    pub gap: f32,
    /// `None` paints nothing — the node is pure layout.
    pub background: Option<Color>,
    pub main_axis: Align,
    pub cross_axis: Align,
    /// Whether a click here takes keyboard focus. Keyboards have no position,
    /// so something has to decide where their events go, and saying so per node
    /// beats a global focus ring nobody can see in the source.
    pub focusable: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            id: None,
            width: Sizing::Fit,
            height: Sizing::Fit,
            padding: 0.0,
            gap: 0.0,
            background: None,
            main_axis: Align::Start,
            cross_axis: Align::Start,
            focusable: false,
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
    /// A column that clips its content and can be scrolled through (§6.4).
    ///
    /// `offset` is how far the content has been scrolled up, in logical pixels,
    /// and it lives in the *app's* state — §6.5 puts state in the actor, and a
    /// scroll position is state like any other. The platform's part is to clamp
    /// it against the content it just measured and hand the clamped value back
    /// as an event.
    Scroll {
        style: Style,
        offset: f32,
        /// The indicator's colour, or `None` for no indicator. A typed prop
        /// colocated with the view (§6.3) rather than a colour the renderer
        /// derives from something else.
        bar: Option<Color>,
        children: Vec<Node>,
    },
}

impl Node {
    /// The style a node carries, if it has one. `Text` styles its glyphs, not
    /// its box, so it has none.
    pub fn style(&self) -> Option<&Style> {
        match self {
            Node::Row { style, .. }
            | Node::Column { style, .. }
            | Node::Box { style }
            | Node::Scroll { style, .. } => Some(style),
            Node::Text { .. } => None,
        }
    }

    /// The children a node lays out, if any.
    fn children(&self) -> &[Node] {
        match self {
            Node::Row { children, .. }
            | Node::Column { children, .. }
            | Node::Scroll { children, .. } => children,
            Node::Box { .. } | Node::Text { .. } => &[],
        }
    }

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
    /// Everything up to the matching `ClipEnd` is confined to this rectangle
    /// (§6.1 names these `clip-start`/`clip-end`). Nested clips intersect, so a
    /// scroll inside a scroll shows only what both allow.
    ClipStart { x: f32, y: f32, width: f32, height: f32 },
    ClipEnd,
}

/// Where an identified node ended up, so input can be routed to it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HitRegion {
    pub id: HitId,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Whether clicking here takes keyboard focus, carried through from
    /// `Style::focusable` so the platform can route keys without consulting the
    /// tree it has already flattened.
    pub focusable: bool,
}

impl HitRegion {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

/// A rectangle, where geometry is geometry and nothing more. Named `Bounds`
/// because taffy's prelude already owns `Rect` in this module.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bounds {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl Bounds {
    /// The overlap of two rectangles, or `None` where they do not meet. This is
    /// what makes nested clips intersect, and what stops a hit region scrolled
    /// out of sight from still being clickable.
    fn intersect(self, other: Bounds) -> Option<Bounds> {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = (self.x + self.width).min(other.x + other.width);
        let bottom = (self.y + self.height).min(other.y + other.height);
        (right > left && bottom > top)
            .then_some(Bounds { x: left, y: top, width: right - left, height: bottom - top })
    }
}

/// A scrollable region as it stood in the frame that was drawn.
///
/// The platform measures the content and reports how far it *could* scroll; the
/// app owns where it *is*. That split is what keeps the offset in the actor's
/// state (§6.5) while making it impossible to scroll into nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollExtent {
    pub id: HitId,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Where the content sits right now.
    pub offset: f32,
    /// The furthest offset that still shows content. Zero means it all fits.
    pub max_offset: f32,
}

/// A finished frame: draw these in order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frame {
    pub commands: Vec<Command>,
    /// Identified regions, in paint order.
    pub hits: Vec<HitRegion>,
    /// Scrollable regions, in paint order.
    pub scrolls: Vec<ScrollExtent>,
}

impl Frame {
    /// Reuses the backing allocation. §6.1's per-frame arena, in the form Rust
    /// makes natural: keep the capacity, drop the contents.
    pub fn clear(&mut self) {
        self.commands.clear();
        self.hits.clear();
        self.scrolls.clear();
    }

    /// The innermost scrollable region under a point, or `None`.
    ///
    /// Found by geometry rather than by hit id, because the node under the
    /// pointer is usually a row *inside* the scroll, and the wheel belongs to
    /// the container either way.
    pub fn scroll_at(&self, x: f32, y: f32) -> Option<&ScrollExtent> {
        self.scrolls.iter().rev().find(|extent| {
            x >= extent.x
                && x < extent.x + extent.width
                && y >= extent.y
                && y < extent.y + extent.height
        })
    }

    /// Finds the node under a point. Paint order is tree order (§6.3 — no
    /// z-index), so the last region painted is the one on top, and the search
    /// runs backwards.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<HitId> {
        self.hit_region(x, y).map(|region| region.id)
    }

    /// As `hit_test`, but keeping the region — the platform needs to know
    /// whether what was clicked takes focus, not merely what it was called.
    pub fn hit_region(&self, x: f32, y: f32) -> Option<&HitRegion> {
        self.hits.iter().rev().find(|region| region.contains(x, y))
    }

    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// How text is measured during layout.
///
/// Layout has to know how big a string will be *before* anything is rendered,
/// and the only honest answer comes from the font that will render it. This is
/// a trait so `scene` need not depend on the text stack, and so tests can lay
/// out without loading fonts.
pub trait Measure {
    /// Returns the width and height a run of text will occupy.
    fn measure(&mut self, text: &str, size: f32) -> (f32, f32);
}

/// A monospace approximation, for tests and for anything laying out without a
/// font stack. Deliberately errs wide: a label that fits with room to spare
/// looks worse than one that fits exactly, and better than one that overflows.
pub struct Approximate;

const ADVANCE: f32 = 0.55;
const LINE_HEIGHT: f32 = 1.25;

impl Measure for Approximate {
    fn measure(&mut self, text: &str, size: f32) -> (f32, f32) {
        (text.chars().count() as f32 * size * ADVANCE, size * LINE_HEIGHT)
    }
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

    /// The most recently laid-out frame, for appending overlay commands to
    /// (§8.4 — the inspector is just more render commands).
    pub fn frame_mut(&mut self) -> &mut Frame {
        &mut self.frame
    }

    /// The most recently laid-out frame, for hit-testing input against what
    /// is actually on screen rather than what the app last built.
    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    /// Lays `root` out using the approximate measurer.
    pub fn layout(&mut self, root: &Node, viewport: (f32, f32)) -> &Frame {
        self.layout_with(root, viewport, &mut Approximate)
    }

    /// Lays `root` out, measuring text with `measure`.
    pub fn layout_with(
        &mut self,
        root: &Node,
        viewport: (f32, f32),
        measure: &mut dyn Measure,
    ) -> &Frame {
        self.frame.clear();

        let mut tree: TaffyTree<()> = TaffyTree::new();
        let Ok(node) = build(&mut tree, root, None, false, measure) else { return &self.frame };

        fit_root(&mut tree, node, root, viewport);

        let space = Size {
            width: AvailableSpace::Definite(viewport.0),
            height: AvailableSpace::Definite(viewport.1),
        };
        if tree.compute_layout(node, space).is_err() {
            return &self.frame;
        }

        emit(&tree, node, root, 0.0, 0.0, None, &mut self.frame);
        &self.frame
    }
}

/// `parent` is the direction of the containing flex box, which decides what
/// growing means: along the parent's main axis a node flexes, across it a node
/// stretches. Using the node's own direction here — as this did originally —
/// makes `width: Grow` inside a column grow the node vertically.
/// The root has no parent to grow inside, so `Grow` there means "fill the
/// viewport". Without this a root that asks to grow sizes to its content and
/// leaves the rest of the window unpainted.
fn fit_root(tree: &mut TaffyTree<()>, id: NodeId, root: &Node, viewport: (f32, f32)) {
    let Some(style) = root.style() else { return };
    if !style.width.is_grow() && !style.height.is_grow() {
        return;
    }
    let Ok(mut root_style) = tree.style(id).cloned() else { return };
    if style.width.is_grow() {
        root_style.size.width = length(viewport.0);
    }
    if style.height.is_grow() {
        root_style.size.height = length(viewport.1);
    }
    let _ = tree.set_style(id, root_style);
}

/// Lays the tree out and visits every node with its depth and absolute
/// geometry. The command array is flat by design, so this is the only place
/// that can still answer "which node produced this rectangle" — the inspector
/// runs here, before flattening.
pub fn walk_laid_out(
    root: &Node,
    viewport: (f32, f32),
    visit: &mut impl FnMut(&Node, usize, f32, f32, f32, f32),
) {
    walk_laid_out_with(root, viewport, &mut Approximate, visit)
}

/// As `walk_laid_out`, measuring text with `measure`.
pub fn walk_laid_out_with(
    root: &Node,
    viewport: (f32, f32),
    measure: &mut dyn Measure,
    visit: &mut impl FnMut(&Node, usize, f32, f32, f32, f32),
) {
    let mut tree: TaffyTree<()> = TaffyTree::new();
    let Ok(id) = build(&mut tree, root, None, false, measure) else { return };
    fit_root(&mut tree, id, root, viewport);

    let space = Size {
        width: AvailableSpace::Definite(viewport.0),
        height: AvailableSpace::Definite(viewport.1),
    };
    if tree.compute_layout(id, space).is_err() {
        return;
    }
    visit_laid_out(&tree, id, root, 0.0, 0.0, 0, visit);
}

fn visit_laid_out(
    tree: &TaffyTree<()>,
    id: NodeId,
    node: &Node,
    x: f32,
    y: f32,
    depth: usize,
    visit: &mut impl FnMut(&Node, usize, f32, f32, f32, f32),
) {
    let Ok(layout) = tree.layout(id) else { return };
    let (x, y) = (x + layout.location.x, y + layout.location.y);
    visit(node, depth, x, y, layout.size.width, layout.size.height);

    let children = node.children();
    if children.is_empty() {
        return;
    }
    // Scrolled content is reported where it actually is, not where it would be
    // at rest — the inspector's whole value is that it does not flatter.
    let scrolled = match node {
        Node::Scroll { offset, .. } => scrolled_by(tree, id, *offset, layout.size.height),
        _ => 0.0,
    };
    let ids = tree.children(id).unwrap_or_default();
    for (child_id, child) in ids.into_iter().zip(children) {
        visit_laid_out(tree, child_id, child, x, y - scrolled, depth + 1, visit);
    }
}

/// How far a scroll's content has actually moved: the app's requested offset,
/// clamped against the content the platform just measured.
fn scrolled_by(tree: &TaffyTree<()>, id: NodeId, offset: f32, height: f32) -> f32 {
    offset.clamp(0.0, (content_height(tree, id) - height).max(0.0))
}

/// The height a scroll's children occupy, overflow included.
fn content_height(tree: &TaffyTree<()>, id: NodeId) -> f32 {
    let padding = tree.layout(id).map(|layout| layout.padding.bottom).unwrap_or(0.0);
    tree.children(id)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|child| tree.layout(child).ok())
        .map(|layout| layout.location.y + layout.size.height)
        .fold(0.0_f32, f32::max)
        + padding
}

/// `scrolls` makes this node clip its content instead of being stretched by it.
/// `rigid` says this node is a scroll's direct child: flexbox would otherwise
/// squeeze the content down to fit the very box it is meant to overflow, and a
/// scroll whose content always fits is not a scroll.
fn taffy_style(
    style: &Style,
    direction: FlexDirection,
    parent: Option<FlexDirection>,
    scrolls: bool,
    rigid: bool,
) -> taffy::Style {
    let (grows_along, grows_across) = match parent {
        Some(FlexDirection::Row) | Some(FlexDirection::RowReverse) => {
            (style.width.is_grow(), style.height.is_grow())
        }
        Some(FlexDirection::Column) | Some(FlexDirection::ColumnReverse) => {
            (style.height.is_grow(), style.width.is_grow())
        }
        // The root flexes inside nothing; `layout` sizes it to the viewport.
        None => (false, false),
    };

    taffy::Style {
        display: Display::Flex,
        flex_direction: direction,
        size: Size { width: style.width.to_dimension(), height: style.height.to_dimension() },
        flex_grow: if grows_along { 1.0 } else { 0.0 },
        flex_shrink: if rigid { 0.0 } else { 1.0 },
        overflow: taffy::Point {
            x: if scrolls { taffy::Overflow::Hidden } else { taffy::Overflow::Visible },
            y: if scrolls { taffy::Overflow::Hidden } else { taffy::Overflow::Visible },
        },
        align_self: grows_across.then_some(AlignItems::STRETCH),
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

fn build(
    tree: &mut TaffyTree<()>,
    node: &Node,
    parent: Option<FlexDirection>,
    rigid: bool,
    measure: &mut dyn Measure,
) -> Result<NodeId, taffy::TaffyError> {
    match node {
        Node::Row { style, children }
        | Node::Column { style, children }
        | Node::Scroll { style, children, .. } => {
            let direction = match node {
                Node::Row { .. } => FlexDirection::Row,
                _ => FlexDirection::Column,
            };
            let scrolls = matches!(node, Node::Scroll { .. });
            let ids: Result<Vec<NodeId>, _> = children
                .iter()
                .map(|child| build(tree, child, Some(direction), scrolls, measure))
                .collect();
            tree.new_with_children(taffy_style(style, direction, parent, scrolls, rigid), &ids?)
        }
        Node::Box { style } => {
            tree.new_leaf(taffy_style(style, FlexDirection::Row, parent, false, rigid))
        }
        Node::Text { text, style } => {
            let (width, height) = measure.measure(text, style.size);
            tree.new_leaf(taffy::Style {
                size: Size { width: length(width), height: length(height) },
                flex_shrink: if rigid { 0.0 } else { 1.0 },
                ..Default::default()
            })
        }
    }
}

/// How wide a scroll indicator is, in logical pixels.
const SCROLLBAR: f32 = 4.0;
/// The shortest a scroll thumb may get. Past this it stops meaning "you are
/// here" and starts meaning "there is a lot".
const MIN_THUMB: f32 = 24.0;

/// Walks the laid-out tree, accumulating absolute positions as it goes —
/// taffy reports each node's location relative to its parent.
///
/// `clip` is the region an ancestor scroll has confined this subtree to, and it
/// is why hit regions are recorded here rather than by the caller: a row
/// scrolled out of sight must stop being clickable at exactly the moment it
/// stops being visible.
fn emit(
    tree: &TaffyTree<()>,
    id: NodeId,
    node: &Node,
    x: f32,
    y: f32,
    clip: Option<Bounds>,
    frame: &mut Frame,
) {
    let Ok(layout) = tree.layout(id) else { return };
    let (x, y) = (x + layout.location.x, y + layout.location.y);
    let (width, height) = (layout.size.width, layout.size.height);

    let paint = |frame: &mut Frame, style: &Style| {
        if let Some(color) = style.background {
            frame.commands.push(Command::Rect { x, y, width, height, color });
        }
        if let Some(hit) = style.id {
            let region = Bounds { x, y, width, height };
            if let Some(visible) = clip.map_or(Some(region), |clip| clip.intersect(region)) {
                frame.hits.push(HitRegion {
                    id: hit,
                    x: visible.x,
                    y: visible.y,
                    width: visible.width,
                    height: visible.height,
                    focusable: style.focusable,
                });
            }
        }
    };

    match node {
        Node::Row { style, children } | Node::Column { style, children } => {
            paint(frame, style);
            let ids = tree.children(id).unwrap_or_default();
            for (child_id, child) in ids.into_iter().zip(children) {
                emit(tree, child_id, child, x, y, clip, frame);
            }
        }
        Node::Box { style } => paint(frame, style),
        Node::Text { text, style } => {
            frame.commands.push(Command::Text {
                x,
                y,
                size: style.size,
                color: style.color,
                text: text.clone(),
            });
        }
        Node::Scroll { style, offset, bar, children } => {
            paint(frame, style);

            let content = content_height(tree, id);
            let max_offset = (content - height).max(0.0);
            let offset = offset.clamp(0.0, max_offset);
            // Only an identified scroll can be told about a wheel, so only an
            // identified one is worth reporting.
            if let Some(hit) = style.id {
                frame.scrolls.push(ScrollExtent {
                    id: hit,
                    x,
                    y,
                    width,
                    height,
                    offset,
                    max_offset,
                });
            }

            let region = Bounds { x, y, width, height };
            let Some(visible) = clip.map_or(Some(region), |clip| clip.intersect(region)) else {
                // Entirely hidden by an ancestor: nothing inside can be seen or
                // clicked, so nothing inside is emitted at all.
                return;
            };

            frame.commands.push(Command::ClipStart {
                x: visible.x,
                y: visible.y,
                width: visible.width,
                height: visible.height,
            });
            let ids = tree.children(id).unwrap_or_default();
            for (child_id, child) in ids.into_iter().zip(children) {
                emit(tree, child_id, child, x, y - offset, Some(visible), frame);
            }
            frame.commands.push(Command::ClipEnd);

            // After the clip closes, so the indicator sits over its own content
            // rather than being trimmed by it.
            if let (Some(color), true) = (bar, max_offset > 0.0) {
                let thumb = (height / content * height).clamp(MIN_THUMB.min(height), height);
                let travel = height - thumb;
                frame.commands.push(Command::Rect {
                    x: x + width - SCROLLBAR,
                    y: y + offset / max_offset * travel,
                    width: SCROLLBAR,
                    height: thumb,
                    color: *color,
                });
            }
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
    fn a_growing_root_fills_the_viewport() {
        // Found by a screenshot: the root has no parent to flex inside, so
        // without special handling it sized to its content and left the rest
        // of the window unpainted.
        let tree = Node::column(
            Style {
                width: Sizing::Grow,
                height: Sizing::Grow,
                background: Some(RED),
                ..Default::default()
            },
            vec![boxed(10.0, 10.0, BLUE)],
        );
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&tree, (640.0, 480.0));
        assert_eq!(rects(frame)[0], (0.0, 0.0, 640.0, 480.0));
    }

    #[test]
    fn growing_across_the_parent_axis_stretches_rather_than_flexes() {
        // `width: Grow` inside a column is a cross-axis request: span the
        // parent's width. It must not make the node taller.
        let tree = Node::column(
            Style { width: Sizing::Fixed(200.0), height: Sizing::Fixed(100.0), ..Default::default() },
            vec![
                Node::Box {
                    style: Style {
                        width: Sizing::Grow,
                        height: Sizing::Fixed(20.0),
                        background: Some(RED),
                        ..Default::default()
                    },
                },
                boxed(10.0, 10.0, BLUE),
            ],
        );
        let mut layouter = Layouter::new();
        let laid = rects(layouter.layout(&tree, (400.0, 400.0)));
        assert_eq!(laid[0], (0.0, 0.0, 200.0, 20.0), "full width, unchanged height");
        assert_eq!(laid[1].1, 20.0, "and the sibling still follows it");
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

    fn hittable(width: f32, height: f32, id: u32) -> Node {
        Node::Box {
            style: Style {
                id: Some(HitId(id)),
                width: Sizing::Fixed(width),
                height: Sizing::Fixed(height),
                background: Some(RED),
                ..Default::default()
            },
        }
    }

    #[test]
    fn only_identified_nodes_are_hit_targets() {
        let tree = Node::column(
            Style::default(),
            vec![boxed(50.0, 20.0, RED), hittable(50.0, 20.0, 7)],
        );
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&tree, (100.0, 100.0));

        assert_eq!(frame.hits.len(), 1, "the anonymous box is not a target");
        assert_eq!(frame.hit_test(10.0, 30.0), Some(HitId(7)));
        assert_eq!(frame.hit_test(10.0, 5.0), None, "the anonymous box swallows nothing");
    }

    #[test]
    fn a_point_outside_everything_hits_nothing() {
        let tree = Node::column(Style::default(), vec![hittable(10.0, 10.0, 1)]);
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&tree, (100.0, 100.0));
        assert_eq!(frame.hit_test(50.0, 50.0), None);
    }

    #[test]
    fn the_topmost_node_wins() {
        // Paint order is tree order (§6.3, no z-index), so a later sibling
        // drawn over an earlier one also receives the input.
        let tree = Node::row(
            Style {
                id: Some(HitId(1)),
                width: Sizing::Fixed(100.0),
                height: Sizing::Fixed(100.0),
                background: Some(BLUE),
                ..Default::default()
            },
            vec![hittable(40.0, 40.0, 2)],
        );
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&tree, (200.0, 200.0));

        assert_eq!(frame.hit_test(10.0, 10.0), Some(HitId(2)), "the child is on top");
        assert_eq!(frame.hit_test(80.0, 80.0), Some(HitId(1)), "outside it, the parent");
    }

    #[test]
    fn hit_regions_follow_layout() {
        let tree = Node::column(
            Style { padding: 20.0, ..Default::default() },
            vec![hittable(30.0, 30.0, 3)],
        );
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&tree, (100.0, 100.0));
        assert_eq!(frame.hit_test(25.0, 25.0), Some(HitId(3)));
        assert_eq!(frame.hit_test(15.0, 15.0), None, "the padding is not the child");
    }

    /// A scroll of `height` px holding `count` rows of 30px each.
    fn scrolling(count: usize, height: f32, offset: f32) -> Node {
        Node::Scroll {
            style: Style {
                id: Some(HitId(9)),
                width: Sizing::Fixed(100.0),
                height: Sizing::Fixed(height),
                ..Default::default()
            },
            offset,
            bar: None,
            children: (0..count).map(|i| hittable(100.0, 30.0, 100 + i as u32)).collect(),
        }
    }

    fn clips(frame: &Frame) -> Vec<(f32, f32, f32, f32)> {
        frame
            .commands
            .iter()
            .filter_map(|c| match c {
                Command::ClipStart { x, y, width, height } => Some((*x, *y, *width, *height)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_scroll_brackets_its_content_in_a_clip() {
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&scrolling(5, 60.0, 0.0), (200.0, 200.0));

        assert_eq!(clips(frame), vec![(0.0, 0.0, 100.0, 60.0)], "clipped to its own box");
        assert!(
            matches!(frame.commands.last(), Some(Command::ClipEnd)),
            "and the clip closes: {:?}",
            frame.commands.last()
        );
    }

    #[test]
    fn content_taller_than_the_scroll_reports_room_to_move() {
        // Five 30px rows in 60px of space: 90px of overflow, and flexbox must
        // not have quietly squeezed them to fit.
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&scrolling(5, 60.0, 0.0), (200.0, 200.0));

        let extent = frame.scrolls[0];
        assert_eq!(extent.id, HitId(9));
        assert_eq!(extent.max_offset, 90.0);
        assert_eq!(extent.offset, 0.0);
    }

    #[test]
    fn content_that_fits_has_nowhere_to_scroll() {
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&scrolling(2, 200.0, 0.0), (200.0, 300.0));
        assert_eq!(frame.scrolls[0].max_offset, 0.0);
    }

    #[test]
    fn scrolling_moves_the_content_up_by_the_offset() {
        let mut layouter = Layouter::new();
        let at_rest = layouter.layout(&scrolling(5, 60.0, 0.0), (200.0, 200.0)).clone();
        let scrolled = layouter.layout(&scrolling(5, 60.0, 45.0), (200.0, 200.0)).clone();

        let first_row = |frame: &Frame| frame.hits.iter().find(|r| r.id == HitId(100)).copied();
        // The first row starts at the top and ends up 45px higher — which, for
        // a 30px row, means clipped out of existence.
        assert_eq!(at_rest.hits[0].y, 0.0);
        assert!(first_row(&scrolled).is_none(), "scrolled past, so no longer clickable");

        // The row that *is* visible has moved up by the offset.
        let third = scrolled.hits.iter().find(|r| r.id == HitId(102)).expect("row 2 is in view");
        assert_eq!(third.y, 60.0 - 45.0, "60px down the content, 45px scrolled");
    }

    #[test]
    fn a_row_scrolled_out_of_sight_stops_being_clickable() {
        // The bug this prevents: content clipped visually but still hit-tested,
        // so clicking empty space below a list toggles something invisible.
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&scrolling(5, 60.0, 0.0), (200.0, 200.0));

        assert_eq!(frame.hit_test(50.0, 10.0), Some(HitId(100)), "row 0 is in view");
        assert_eq!(frame.hit_test(50.0, 50.0), Some(HitId(101)), "row 1 is half in view");
        assert_eq!(frame.hit_test(50.0, 70.0), None, "row 2 is past the clip");
    }

    #[test]
    fn a_half_visible_row_is_clickable_only_where_it_shows() {
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&scrolling(5, 50.0, 0.0), (200.0, 200.0));

        // Row 1 spans 30..60, the scroll ends at 50.
        let row = frame.hits.iter().find(|r| r.id == HitId(101)).expect("row 1 shows");
        assert_eq!((row.y, row.height), (30.0, 20.0), "trimmed to the visible part");
        assert_eq!(frame.hit_test(50.0, 45.0), Some(HitId(101)));
        assert_eq!(frame.hit_test(50.0, 55.0), None);
    }

    #[test]
    fn an_offset_past_the_end_is_clamped_rather_than_obeyed() {
        // The app owns the offset (§6.5) but the platform measured the content,
        // so a value that would scroll into nothing is pulled back and the
        // clamped number is what gets reported.
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&scrolling(5, 60.0, 10_000.0), (200.0, 200.0));

        assert_eq!(frame.scrolls[0].offset, 90.0, "clamped to the last full screen");
        let last = frame.hits.iter().find(|r| r.id == HitId(104)).expect("the last row shows");
        assert_eq!(last.y + last.height, 60.0, "and it sits against the bottom");
    }

    #[test]
    fn nested_clips_intersect() {
        // §6.1's rule: a scroll inside a scroll shows only what both allow.
        let inner = Node::Scroll {
            style: Style {
                id: Some(HitId(8)),
                width: Sizing::Fixed(100.0),
                height: Sizing::Fixed(200.0),
                ..Default::default()
            },
            offset: 0.0,
            bar: None,
            children: vec![hittable(100.0, 300.0, 7)],
        };
        let outer = Node::Scroll {
            style: Style {
                id: Some(HitId(9)),
                width: Sizing::Fixed(100.0),
                height: Sizing::Fixed(50.0),
                ..Default::default()
            },
            offset: 0.0,
            bar: None,
            children: vec![inner],
        };

        let mut layouter = Layouter::new();
        let frame = layouter.layout(&outer, (200.0, 200.0));
        assert_eq!(
            clips(frame),
            vec![(0.0, 0.0, 100.0, 50.0), (0.0, 0.0, 100.0, 50.0)],
            "the inner 200px-tall clip is cut down by the outer 50px one"
        );
    }

    #[test]
    fn an_indicator_appears_only_when_there_is_more_to_see() {
        let mut layouter = Layouter::new();
        let with_bar = |count, height, offset| Node::Scroll {
            style: Style {
                id: Some(HitId(9)),
                width: Sizing::Fixed(100.0),
                height: Sizing::Fixed(height),
                ..Default::default()
            },
            offset,
            bar: Some(RED),
            children: (0..count).map(|i| hittable(100.0, 30.0, 100 + i as u32)).collect(),
        };

        let bar_of = |frame: &Frame| {
            frame
                .commands
                .iter()
                .filter_map(|c| match c {
                    Command::Rect { x, y, width, color, .. } if *color == RED && *width == 4.0 => {
                        Some((*x, *y))
                    }
                    _ => None,
                })
                .next()
        };

        let fits = layouter.layout(&with_bar(1, 100.0, 0.0), (200.0, 200.0)).clone();
        assert!(bar_of(&fits).is_none(), "nothing to indicate when it all fits");

        let top = layouter.layout(&with_bar(10, 60.0, 0.0), (200.0, 200.0)).clone();
        let (x, y) = bar_of(&top).expect("an overflowing scroll shows its position");
        assert_eq!(x, 96.0, "against the right edge of the 100px box");
        assert_eq!(y, 0.0, "at the top when the offset is zero");

        let bottom = layouter.layout(&with_bar(10, 60.0, 240.0), (200.0, 200.0)).clone();
        let (_, y) = bar_of(&bottom).expect("still shown when scrolled");
        assert!(y > 0.0, "and it has travelled down");
    }

    #[test]
    fn a_scroll_hidden_by_an_ancestor_emits_nothing_at_all() {
        // Not merely invisible: skipping the subtree is what keeps a clipped
        // frame cheap rather than merely correct.
        let hidden = Node::Scroll {
            style: Style {
                id: Some(HitId(9)),
                width: Sizing::Fixed(100.0),
                height: Sizing::Fixed(40.0),
                ..Default::default()
            },
            offset: 0.0,
            bar: None,
            children: vec![hittable(100.0, 30.0, 5)],
        };
        let outer = Node::Scroll {
            style: Style {
                id: Some(HitId(1)),
                width: Sizing::Fixed(100.0),
                height: Sizing::Fixed(40.0),
                ..Default::default()
            },
            // Scrolled far enough that the inner scroll — which sits 40px into
            // the content and is 40px tall — has passed entirely above the top.
            offset: 80.0,
            bar: None,
            children: vec![
                hittable(100.0, 40.0, 4),
                hidden,
                hittable(100.0, 200.0, 6),
            ],
        };

        let mut layouter = Layouter::new();
        let frame = layouter.layout(&outer, (200.0, 200.0));
        assert!(frame.hits.iter().all(|r| r.id != HitId(5)), "its content is gone");
        assert_eq!(clips(frame).len(), 1, "and it opened no clip of its own");
    }

    #[test]
    fn scroll_regions_are_found_by_geometry_not_by_what_is_on_top() {
        // The wheel belongs to the container, even though the node under the
        // pointer is one of its rows.
        let mut layouter = Layouter::new();
        let frame = layouter.layout(&scrolling(5, 60.0, 0.0), (200.0, 200.0));

        assert_eq!(frame.hit_test(50.0, 10.0), Some(HitId(100)), "a row is on top");
        assert_eq!(frame.scroll_at(50.0, 10.0).map(|e| e.id), Some(HitId(9)), "the scroll gets it");
        assert!(frame.scroll_at(150.0, 10.0).is_none(), "and nothing outside it does");
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
