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
//! commands. Outlines go through the same pipeline as everything else — and so
//! does the actor panel, which is why §8.4's "rendered by the platform" costs
//! a few rectangles rather than a subsystem.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use crate::scene::{Bounds, Color, Command, Frame, HitId, Node};

/// Outline colour for ordinary boxes.
const OUTLINE: Color = Color::rgba(0.20, 0.85, 0.80, 0.55);
/// Outline colour for the node under the pointer.
const HIGHLIGHT: Color = Color::rgba(1.0, 0.45, 0.20, 0.95);

/// One row of §8.4's debug overlay: what one actor is doing right now.
///
/// A *display* type, deliberately not the runtime's measurement type. The
/// renderer knows how to draw a row and nothing about wasmtime; the runtime
/// knows how to measure an actor and nothing about frames. `strand-cli` depends
/// on both and maps one to the other, which is the whole cost of keeping the
/// compositor free of the VM.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ActorStat {
    pub name: String,
    pub arena_bytes: u64,
    pub mailbox: usize,
    pub fibers: u32,
    pub handled: u64,
    pub generation: u32,
    pub alive: bool,
}

/// The slot the runtime publishes actor stats into and the compositor reads.
///
/// Reading uses `try_lock`, which is the whole design: §6.1 promises the
/// compositor waits for nobody, and that has to include the thing telling it
/// how the app is doing. A contended frame simply redraws the numbers it
/// already had — one frame of staleness, never one frame of stall.
#[derive(Clone, Default)]
pub struct StatsHandle {
    inner: Arc<Mutex<Vec<ActorStat>>>,
}

impl StatsHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called from the actor side, as often as it likes.
    pub fn publish(&self, stats: Vec<ActorStat>) {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = stats;
        }
    }

    /// Refreshes `out` if the slot is free. Returns whether it did; `false`
    /// means "keep drawing what you have", not "there is nothing".
    pub fn read_into(&self, out: &mut Vec<ActorStat>) -> bool {
        let Ok(slot) = self.inner.try_lock() else { return false };
        out.clear();
        out.extend_from_slice(&slot);
        true
    }
}

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
    ///
    /// `stats` is drawn as a panel in the top-right corner; pass an empty slice
    /// where there are no actors to report, and no panel appears.
    pub fn overlay(&self, frame: &mut Frame, viewport: (f32, f32), stats: &[ActorStat]) {
        if !self.enabled {
            return;
        }

        // Snapshot first: the outlines being appended must not be outlined.
        //
        // Each box is kept with the clip it was drawn under. The outlines go on
        // the end of the array, past every `ClipEnd`, so nothing would trim
        // them — and a row scrolled out of a list would be outlined across
        // whatever happens to sit above and below it.
        let mut clips: Vec<Bounds> = Vec::new();
        let mut boxes: Vec<(Bounds, Option<Bounds>)> = Vec::new();
        for command in &frame.commands {
            match command {
                Command::Rect { x, y, width, height, .. } => boxes.push((
                    Bounds { x: *x, y: *y, width: *width, height: *height },
                    clips.last().copied(),
                )),
                Command::ClipStart { x, y, width, height } => {
                    let region = Bounds { x: *x, y: *y, width: *width, height: *height };
                    let nested = match clips.last() {
                        // Nothing survives an empty intersection, and a
                        // zero-area region is what says so.
                        Some(outer) => outer.intersect(region).unwrap_or(Bounds {
                            x: 0.0,
                            y: 0.0,
                            width: 0.0,
                            height: 0.0,
                        }),
                        None => region,
                    };
                    clips.push(nested);
                }
                Command::ClipEnd => {
                    clips.pop();
                }
                // Text has no box of its own; outlining one would be inventing
                // geometry.
                Command::Text { .. } => {}
            }
        }

        for (rect, clip) in boxes {
            outline_within(frame, rect, clip, 1.0, OUTLINE);
        }

        if let Some(id) = self.highlight {
            if let Some(region) = frame.hits.iter().find(|region| region.id == id).copied() {
                outline(frame, region.x, region.y, region.width, region.height, 2.0, HIGHLIGHT);
            }
        }

        actor_panel(frame, viewport, stats);
    }
}

/// Panel colours. Dark and translucent, so it reads as an instrument laid over
/// the app rather than part of it.
const PANEL_BG: Color = Color::rgba(0.02, 0.02, 0.03, 0.86);
const PANEL_TEXT: Color = Color::rgb(0.86, 0.88, 0.92);
const PANEL_HEAD: Color = Color::rgb(0.45, 0.62, 0.85);
/// A dead actor's row. §5.4 says a death is a normal event, so it is coloured
/// like a fact rather than like an alarm.
const PANEL_DEAD: Color = Color::rgb(0.95, 0.45, 0.35);

const PANEL_WIDTH: f32 = 330.0;
const PANEL_MARGIN: f32 = 8.0;
const PANEL_PAD: f32 = 8.0;
const ROW_HEIGHT: f32 = 15.0;
const PANEL_FONT: f32 = 11.0;

/// Column offsets within the panel. Placed rather than laid out: a row must
/// read as a table, and one string per row would not align under a
/// proportional font.
const COLUMNS: [f32; 5] = [0.0, 124.0, 190.0, 236.0, 274.0];
const HEADINGS: [&str; 5] = ["actor", "arena", "mbox", "fib", "msgs"];

/// §8.4's debug overlay: per-actor arena sizes, mailbox depths and fiber
/// counts, making §7's isolation claim something you can watch rather than
/// something you are told.
fn actor_panel(frame: &mut Frame, viewport: (f32, f32), stats: &[ActorStat]) {
    if stats.is_empty() {
        return;
    }

    // Header, one row per actor, footer.
    let rows = stats.len() as f32 + 2.0;
    let height = rows * ROW_HEIGHT + PANEL_PAD * 2.0;
    let left = (viewport.0 - PANEL_WIDTH - PANEL_MARGIN).max(PANEL_MARGIN);
    let top = PANEL_MARGIN;

    frame.commands.push(Command::Rect {
        x: left,
        y: top,
        width: PANEL_WIDTH,
        height,
        color: PANEL_BG,
    });

    let mut cell = |x: f32, y: f32, color: Color, text: String| {
        frame.commands.push(Command::Text { x, y, size: PANEL_FONT, color, text });
    };

    let origin = left + PANEL_PAD;
    let mut y = top + PANEL_PAD;
    for (column, heading) in COLUMNS.iter().zip(HEADINGS) {
        cell(origin + column, y, PANEL_HEAD, heading.to_string());
    }

    let mut live_bytes = 0u64;
    for stat in stats {
        y += ROW_HEIGHT;
        let color = if stat.alive { PANEL_TEXT } else { PANEL_DEAD };
        // A restarted actor wears its generation, because "same name, third
        // life" is the interesting fact about it.
        let name = match (stat.alive, stat.generation) {
            (true, 0) => stat.name.clone(),
            (true, n) => format!("{} ×{n}", stat.name),
            (false, _) => format!("{} (dead)", stat.name),
        };
        if stat.alive {
            live_bytes += stat.arena_bytes;
        }
        cell(origin + COLUMNS[0], y, color, name);
        cell(origin + COLUMNS[1], y, color, bytes(stat.arena_bytes));
        cell(origin + COLUMNS[2], y, color, stat.mailbox.to_string());
        cell(origin + COLUMNS[3], y, color, stat.fibers.to_string());
        cell(origin + COLUMNS[4], y, color, stat.handled.to_string());
    }

    y += ROW_HEIGHT;
    let live = stats.iter().filter(|stat| stat.alive).count();
    cell(
        origin,
        y,
        PANEL_HEAD,
        format!("{live}/{} live · {} in arenas", stats.len(), bytes(live_bytes)),
    );
}

/// Bytes at a glance. Two significant-ish figures, because the question the
/// overlay answers is "is this growing", not "by how many bytes".
fn bytes(count: u64) -> String {
    const KB: f32 = 1024.0;
    const MB: f32 = KB * 1024.0;
    match count as f32 {
        n if n >= MB => format!("{:.2}MB", n / MB),
        n if n >= KB => format!("{:.0}KB", n / KB),
        n => format!("{n:.0}B"),
    }
}

/// Four thin rects. A stroked rectangle primitive would be fewer commands, but
/// would also be a second thing the renderer has to understand.
fn outline(frame: &mut Frame, x: f32, y: f32, width: f32, height: f32, weight: f32, color: Color) {
    outline_within(frame, Bounds { x, y, width, height }, None, weight, color);
}

/// As `outline`, trimmed to the region the outlined box was drawn under.
///
/// Each edge is clipped on its own, so an outline that is half inside a scroll
/// is drawn half — which is what the content under it looks like, and the whole
/// point of an inspector is that it shows you what is there.
fn outline_within(
    frame: &mut Frame,
    rect: Bounds,
    clip: Option<Bounds>,
    weight: f32,
    color: Color,
) {
    let mut edge = |x: f32, y: f32, width: f32, height: f32| {
        let edge = Bounds { x, y, width, height };
        let Some(visible) = clip.map_or(Some(edge), |clip| clip.intersect(edge)) else {
            return;
        };
        frame.commands.push(Command::Rect {
            x: visible.x,
            y: visible.y,
            width: visible.width,
            height: visible.height,
            color,
        });
    };
    let Bounds { x, y, width, height } = rect;
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
    describe_with(root, viewport, &mut crate::scene::Approximate)
}

/// As `describe`, but measuring text with the real font stack, so the geometry
/// reported is the geometry the renderer would produce. Loading the system
/// fonts costs enough to be worth doing once, which is why the caller owns it.
pub fn describe_with_fonts(root: &Node, viewport: (f32, f32)) -> String {
    let mut fonts = glyphon::FontSystem::new();
    let mut measure = crate::text::FontMeasure::new(&mut fonts);
    describe_with(root, viewport, &mut measure)
}

pub fn describe_with(
    root: &Node,
    viewport: (f32, f32),
    measure: &mut dyn crate::scene::Measure,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "viewport {:.0}x{:.0}", viewport.0, viewport.1);
    crate::scene::walk_laid_out_with(root, viewport, measure, &mut |node, depth, x, y, width, height| {
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
        Node::Scroll { .. } => "scroll",
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

    const VIEWPORT: (f32, f32) = (400.0, 300.0);

    fn stat(name: &str) -> ActorStat {
        ActorStat {
            name: name.to_string(),
            arena_bytes: 1_114_112,
            mailbox: 2,
            fibers: 1,
            handled: 34,
            generation: 0,
            alive: true,
        }
    }

    fn texts(frame: &Frame) -> Vec<String> {
        frame
            .commands
            .iter()
            .filter_map(|command| match command {
                Command::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

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
        Inspector::default().overlay(&mut frame, VIEWPORT, &[]);
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

        Inspector { enabled: true, highlight: None }.overlay(&mut frame, VIEWPORT, &[]);
        let after = frame.commands.iter().filter(|c| matches!(c, Command::Rect { .. })).count();
        assert_eq!(after, rects + rects * 4, "four edges per box, and no outlines of outlines");
    }

    #[test]
    fn highlighting_adds_one_more_outline_for_the_named_node() {
        let mut layouter = Layouter::new();
        let mut frame = layouter.layout(&sample(), (400.0, 300.0)).clone();
        let plain = {
            let mut copy = frame.clone();
            Inspector { enabled: true, highlight: None }.overlay(&mut copy, VIEWPORT, &[]);
            copy.commands.len()
        };
        Inspector { enabled: true, highlight: Some(HitId(42)) }.overlay(&mut frame, VIEWPORT, &[]);
        assert_eq!(frame.commands.len(), plain + 4, "the highlight is four more edges");
    }

    #[test]
    fn outlines_stay_inside_the_scroll_they_belong_to() {
        // Reported from the running app: with F12 on, rows scrolled out of the
        // todo list were outlined across the field above and the buttons
        // below. The outlines go on the end of the array, past every `ClipEnd`,
        // so nothing was trimming them.
        //
        // Five 30px rows in a 60px window, scrolled 30: one row is entirely
        // above the top and two are entirely below the bottom.
        let tree = Node::Scroll {
            style: Style {
                id: Some(HitId(9)),
                width: Sizing::Fixed(100.0),
                height: Sizing::Fixed(60.0),
                ..Default::default()
            },
            offset: 30.0,
            bar: None,
            children: (0..5)
                .map(|index| Node::Box {
                    style: Style {
                        id: Some(HitId(100 + index)),
                        width: Sizing::Fixed(100.0),
                        height: Sizing::Fixed(30.0),
                        background: Some(Color::rgb(1.0, 0.0, 0.0)),
                        ..Default::default()
                    },
                })
                .collect(),
        };

        let mut layouter = Layouter::new();
        let viewport = (200.0, 200.0);
        let mut frame = layouter.layout(&tree, viewport).clone();
        let before = frame.commands.len();
        Inspector { enabled: true, highlight: None }.overlay(&mut frame, viewport, &[]);

        let outlines = &frame.commands[before..];
        assert!(!outlines.is_empty(), "the visible rows are still outlined");
        for command in outlines {
            let Command::Rect { y, height, .. } = command else { continue };
            assert!(
                *y >= 0.0 && *y + *height <= 60.0,
                "an outline at y={y} height={height} escaped the 0..60 scroll"
            );
        }
    }

    #[test]
    fn an_outline_half_inside_a_scroll_is_drawn_half() {
        // Trimmed rather than dropped: the inspector's job is to show what is
        // actually there, and half a row is what is there.
        let tree = Node::Scroll {
            style: Style {
                id: Some(HitId(9)),
                width: Sizing::Fixed(100.0),
                height: Sizing::Fixed(50.0),
                ..Default::default()
            },
            offset: 0.0,
            bar: None,
            children: vec![Node::Box {
                style: Style {
                    width: Sizing::Fixed(100.0),
                    height: Sizing::Fixed(80.0),
                    background: Some(Color::rgb(1.0, 0.0, 0.0)),
                    ..Default::default()
                },
            }],
        };

        let mut layouter = Layouter::new();
        let viewport = (200.0, 200.0);
        let mut frame = layouter.layout(&tree, viewport).clone();
        let before = frame.commands.len();
        Inspector { enabled: true, highlight: None }.overlay(&mut frame, viewport, &[]);

        let outlines = &frame.commands[before..];
        assert!(!outlines.is_empty(), "the visible part is still outlined");
        for command in outlines {
            let Command::Rect { y, height, .. } = command else { continue };
            assert!(*y + *height <= 50.0, "an edge at y={y} height={height} ran past the clip");
        }
        // The 80px box's top edge survives; its bottom edge, at y = 78, does not.
        assert!(
            outlines.iter().any(|c| matches!(c, Command::Rect { y, .. } if *y == 0.0)),
            "the top edge should still be drawn"
        );
    }

    #[test]
    fn the_actor_panel_reports_every_gauge_section_8_4_asks_for() {
        let mut frame = Frame::default();
        Inspector { enabled: true, highlight: None }.overlay(
            &mut frame,
            VIEWPORT,
            &[stat("counter")],
        );

        let drawn = texts(&frame);
        assert!(drawn.contains(&"counter".to_string()), "{drawn:?}");
        // Arena size, mailbox depth and fiber count — §8.4's three gauges.
        assert!(drawn.contains(&"1.06MB".to_string()), "arena size: {drawn:?}");
        assert!(drawn.contains(&"2".to_string()), "mailbox depth: {drawn:?}");
        assert!(drawn.contains(&"34".to_string()), "messages handled: {drawn:?}");
        assert!(drawn.iter().any(|t| t.contains("1/1 live")), "footer: {drawn:?}");
    }

    #[test]
    fn no_actors_means_no_panel() {
        // An empty panel would be a claim that there is nothing to see, which
        // is different from having nothing to say.
        let mut frame = Frame::default();
        Inspector { enabled: true, highlight: None }.overlay(&mut frame, VIEWPORT, &[]);
        assert!(frame.commands.is_empty());
    }

    #[test]
    fn the_panel_only_appears_with_the_inspector() {
        let mut frame = Frame::default();
        Inspector::default().overlay(&mut frame, VIEWPORT, &[stat("counter")]);
        assert!(frame.commands.is_empty(), "F12 governs the whole overlay, panel included");
    }

    #[test]
    fn a_restarted_actor_wears_its_generation_and_a_dead_one_says_so() {
        let mut frame = Frame::default();
        let restarted = ActorStat { generation: 2, ..stat("stats") };
        let dead = ActorStat { alive: false, generation: 1, ..stat("gone") };
        Inspector { enabled: true, highlight: None }.overlay(
            &mut frame,
            VIEWPORT,
            &[restarted, dead],
        );

        let drawn = texts(&frame);
        assert!(drawn.contains(&"stats ×2".to_string()), "{drawn:?}");
        assert!(drawn.contains(&"gone (dead)".to_string()), "{drawn:?}");
        // The footer counts arenas that still exist. A dead actor's is gone.
        assert!(drawn.iter().any(|t| t.contains("1/2 live · 1.06MB")), "{drawn:?}");
    }

    #[test]
    fn the_panel_stays_inside_the_viewport_it_is_drawn_over() {
        // Found by thinking about the burn demo at a small window size: a panel
        // anchored right must not walk off the left edge of a narrow one.
        for width in [200.0_f32, 400.0, 1600.0] {
            let mut frame = Frame::default();
            Inspector { enabled: true, highlight: None }.overlay(
                &mut frame,
                (width, 300.0),
                &[stat("counter")],
            );
            let Command::Rect { x, .. } = frame.commands[0] else { panic!("panel first") };
            assert!(x >= 0.0, "panel left edge at {x} for a {width}px viewport");
            assert!(x <= width, "panel starts off-screen at {x} for {width}px");
        }
    }

    #[test]
    fn a_busy_publisher_never_stalls_the_reader() {
        // The property the handle exists for: reading is `try_lock`, so a
        // compositor frame can never be held up by whoever is publishing.
        let handle = StatsHandle::new();
        handle.publish(vec![stat("counter")]);

        let mut out = Vec::new();
        assert!(handle.read_into(&mut out));
        assert_eq!(out.len(), 1);

        // With the slot held, the reader declines rather than waiting, and the
        // caller keeps the rows it already had.
        let held = handle.inner.lock().unwrap();
        assert!(!handle.read_into(&mut out), "reading must not block");
        assert_eq!(out.len(), 1, "and must leave the last snapshot standing");
        drop(held);
    }

    #[test]
    fn byte_sizes_read_at_a_glance() {
        assert_eq!(bytes(0), "0B");
        assert_eq!(bytes(512), "512B");
        assert_eq!(bytes(64 * 1024), "64KB");
        assert_eq!(bytes(1024 * 1024), "1.00MB");
    }
}
