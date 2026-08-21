//! Reading a frame out of a guest's memory (§6.1, §6.2).
//!
//! A view emits nodes as it evaluates, so what it leaves behind is a
//! post-order array: every node's children are the finished roots immediately
//! before it, and it says how many of them are its own. Rebuilding the tree is
//! therefore one left-to-right pass with a stack — no seeking, no back-patching
//! and no second representation in between.
//!
//! There is no decode step in the sense that word usually means. The bytes the
//! guest wrote are the bytes read here; the only work is turning a widget tag
//! and its props into the platform's `Node`, which is where §6.3's theme gets
//! applied. That is the Cap'n Proto lesson from `docs/inspiration-canon.md`,
//! and `strandc::ui` is the single table both ends read it from.

use anyhow::{anyhow, bail, Result};
use strand_render::scene::{Align, Node, Sizing, Style};
use strand_render::widgets::{self, Theme};
use strandc::ui::{NodeKind, Slot, CHILD_COUNT_OFFSET, KIND_OFFSET, NODE_SIZE};

/// One node record, read out of guest memory.
struct Record {
    kind: NodeKind,
    children: usize,
    id: u32,
    flag: bool,
    text: String,
    text2: String,
    number: f32,
    number2: f32,
}

fn read_u32(memory: &[u8], at: usize) -> Result<u32> {
    let bytes = memory
        .get(at..at + 4)
        .ok_or_else(|| anyhow!("frame runs past the guest's memory at {at:#x}"))?;
    Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
}

fn read_f32(memory: &[u8], at: usize) -> Result<f32> {
    Ok(f32::from_bits(read_u32(memory, at)?))
}

/// A Strand string: `{ i32 len, bytes }` (`docs/abi.md` §5). A null pointer is
/// "no text", which is how an unwritten string prop arrives.
fn read_str(memory: &[u8], ptr: u32) -> Result<String> {
    if ptr == 0 {
        return Ok(String::new());
    }
    let at = ptr as usize;
    let len = read_u32(memory, at)? as usize;
    let bytes = memory
        .get(at + 4..at + 4 + len)
        .ok_or_else(|| anyhow!("string at {ptr:#x} claims {len} bytes it does not have"))?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn read_record(memory: &[u8], at: usize) -> Result<Record> {
    let tag = read_u32(memory, at + KIND_OFFSET as usize)? as i32;
    let kind = NodeKind::from_tag(tag)
        .ok_or_else(|| anyhow!("frame holds an unknown node kind {tag}"))?;
    Ok(Record {
        kind,
        children: read_u32(memory, at + CHILD_COUNT_OFFSET as usize)? as usize,
        id: read_u32(memory, at + Slot::Id.offset() as usize)?,
        flag: read_u32(memory, at + Slot::Flag.offset() as usize)? != 0,
        text: read_str(memory, read_u32(memory, at + Slot::Text.offset() as usize)?)?,
        text2: read_str(memory, read_u32(memory, at + Slot::Text2.offset() as usize)?)?,
        number: read_f32(memory, at + Slot::Number.offset() as usize)?,
        number2: read_f32(memory, at + Slot::Number2.offset() as usize)?,
    })
}

/// Rebuilds the tree a view emitted.
///
/// One pass, one stack. A container takes the last `children` roots off the
/// stack and pushes itself in their place, so the stack holds exactly the roots
/// nobody has claimed yet — the same counter the guest kept while building.
pub fn decode(theme: &Theme, memory: &[u8], base: u32, count: u32) -> Result<Node> {
    let mut roots: Vec<Node> = Vec::new();

    for index in 0..count as usize {
        let record = read_record(memory, base as usize + index * NODE_SIZE as usize)?;
        if record.children > roots.len() {
            bail!(
                "node {index} claims {} children but only {} are unclaimed — the \
                 frame is not in post-order",
                record.children,
                roots.len()
            );
        }
        let children = roots.split_off(roots.len() - record.children);
        roots.push(build(theme, &record, children));
    }

    match roots.len() {
        1 => Ok(roots.pop().expect("just checked")),
        0 => bail!("the view drew nothing"),
        n => bail!("the view left {n} roots; a view returns one Node"),
    }
}

/// Turns one record into the platform's node.
///
/// This is where §6.3's theme is applied: the view named a widget and passed it
/// typed props, and the colours it wears come from the platform, not from
/// anything the app assembled.
fn build(theme: &Theme, record: &Record, children: Vec<Node>) -> Node {
    let hit = strand_render::scene::HitId(record.id);
    let spacing =
        |style: Style| Style { gap: record.number, padding: record.number2, ..style };

    match record.kind {
        NodeKind::Screen => Node::column(
            spacing(Style {
                width: Sizing::Grow,
                height: Sizing::Grow,
                background: Some(theme.surface),
                ..Default::default()
            }),
            children,
        ),
        // A column fills the width it is given and takes the height it needs:
        // the shape app layout wants nine times out of ten, and §6.3's sizing
        // vocabulary is what will make the tenth sayable.
        NodeKind::Column => {
            Node::column(spacing(Style { width: Sizing::Grow, ..Default::default() }), children)
        }
        NodeKind::Row => Node::row(
            spacing(Style {
                width: Sizing::Grow,
                cross_axis: Align::Center,
                ..Default::default()
            }),
            children,
        ),
        NodeKind::Panel => Node::column(
            spacing(Style {
                width: Sizing::Grow,
                background: Some(theme.raised),
                ..Default::default()
            }),
            children,
        ),
        NodeKind::Scroll => {
            widgets::scroll(theme, hit, record.number, Sizing::Grow, children)
        }
        NodeKind::Text => widgets::label(theme, record.text.clone()),
        NodeKind::Muted => widgets::muted_label(theme, record.text.clone()),
        NodeKind::Button => widgets::button(theme, hit, record.text.clone()),
        NodeKind::Checkbox => {
            widgets::checkbox(theme, hit, record.flag, record.text.clone())
        }
        NodeKind::TextInput => {
            widgets::text_input(theme, hit, &record.text, &record.text2, record.flag)
        }
        // Grows in whichever direction its parent runs, which is what a spacer
        // is for: pushing the rest of the row or column to the far end.
        NodeKind::Spacer => Node::Box {
            style: Style { width: Sizing::Grow, height: Sizing::Grow, ..Default::default() },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strandc::ui::NODE_CAPACITY;

    /// Writes a node record the way codegen does, so the decoder is tested
    /// against the layout rather than against itself.
    struct Writer {
        memory: Vec<u8>,
        count: u32,
    }

    const BASE: u32 = 4096;

    impl Writer {
        fn new() -> Self {
            Self { memory: vec![0; 65_536], count: 0 }
        }

        fn put_u32(&mut self, at: usize, value: u32) {
            self.memory[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }

        fn string(&mut self, at: u32, text: &str) -> u32 {
            self.put_u32(at as usize, text.len() as u32);
            self.memory[at as usize + 4..at as usize + 4 + text.len()]
                .copy_from_slice(text.as_bytes());
            at
        }

        fn node(&mut self, kind: NodeKind, children: usize) -> usize {
            let at = BASE as usize + self.count as usize * NODE_SIZE as usize;
            self.put_u32(at + KIND_OFFSET as usize, kind.tag() as u32);
            self.put_u32(at + CHILD_COUNT_OFFSET as usize, children as u32);
            self.count += 1;
            at
        }

        fn slot(&mut self, at: usize, slot: Slot, value: u32) {
            self.put_u32(at + slot.offset() as usize, value);
        }

        fn decode(&self) -> Result<Node> {
            super::decode(&Theme::default(), &self.memory, BASE, self.count)
        }
    }

    #[test]
    fn a_leaf_decodes_to_one_node() {
        let mut writer = Writer::new();
        let text = writer.string(1024, "hello");
        let at = writer.node(NodeKind::Text, 0);
        writer.slot(at, Slot::Text, text);

        let tree = writer.decode().expect("one leaf is a tree");
        assert!(matches!(&tree, Node::Text { text, .. } if text == "hello"));
    }

    #[test]
    fn post_order_rebuilds_the_tree_in_source_order() {
        // column { text("a") text("b") } — children first, parent last, and the
        // children must come back the way they were written.
        let mut writer = Writer::new();
        let a = writer.string(1024, "a");
        let b = writer.string(1088, "b");
        let first = writer.node(NodeKind::Text, 0);
        writer.slot(first, Slot::Text, a);
        let second = writer.node(NodeKind::Text, 0);
        writer.slot(second, Slot::Text, b);
        writer.node(NodeKind::Column, 2);

        let tree = writer.decode().expect("a column of two");
        let Node::Column { children, .. } = &tree else { panic!("expected a column: {tree:?}") };
        assert_eq!(children.len(), 2);
        assert!(matches!(&children[0], Node::Text { text, .. } if text == "a"));
        assert!(matches!(&children[1], Node::Text { text, .. } if text == "b"));
    }

    #[test]
    fn nesting_survives_the_flattening() {
        // row { text("a") column { text("b") } }
        let mut writer = Writer::new();
        let a = writer.string(1024, "a");
        let b = writer.string(1088, "b");
        let outer_first = writer.node(NodeKind::Text, 0);
        writer.slot(outer_first, Slot::Text, a);
        let inner = writer.node(NodeKind::Text, 0);
        writer.slot(inner, Slot::Text, b);
        writer.node(NodeKind::Column, 1);
        writer.node(NodeKind::Row, 2);

        let tree = writer.decode().expect("a row of two");
        let Node::Row { children, .. } = &tree else { panic!("expected a row") };
        assert_eq!(children.len(), 2);
        assert!(matches!(&children[0], Node::Text { .. }));
        let Node::Column { children: inner, .. } = &children[1] else {
            panic!("expected a nested column")
        };
        assert_eq!(inner.len(), 1);
    }

    #[test]
    fn props_reach_the_widget_they_were_written_on() {
        let mut writer = Writer::new();
        let label = writer.string(1024, "Add");
        let at = writer.node(NodeKind::Button, 0);
        writer.slot(at, Slot::Id, 7);
        writer.slot(at, Slot::Text, label);

        let tree = writer.decode().expect("a button");
        let mut layouter = strand_render::scene::Layouter::new();
        let frame = layouter.layout(&tree, (200.0, 60.0));
        assert_eq!(frame.hits.len(), 1);
        assert_eq!(frame.hits[0].id.0, 7, "the id the view wrote is the id input routes to");
    }

    #[test]
    fn spacing_props_reach_layout() {
        let mut writer = Writer::new();
        let a = writer.string(1024, "a");
        let leaf = writer.node(NodeKind::Text, 0);
        writer.slot(leaf, Slot::Text, a);
        let at = writer.node(NodeKind::Column, 1);
        writer.put_u32(at + Slot::Number2.offset() as usize, 20.0f32.to_bits());

        let tree = writer.decode().expect("a padded column");
        let mut layouter = strand_render::scene::Layouter::new();
        let frame = layouter.layout(&tree, (200.0, 60.0));
        let strand_render::scene::Command::Text { x, y, .. } = frame.commands[0] else {
            panic!("expected the label")
        };
        assert_eq!((x, y), (20.0, 20.0), "padding: 20 inset the child");
    }

    #[test]
    fn an_empty_frame_is_an_error_rather_than_a_blank_window() {
        let writer = Writer::new();
        let message = writer.decode().unwrap_err().to_string();
        assert!(message.contains("drew nothing"), "{message}");
    }

    #[test]
    fn a_frame_that_is_not_post_order_is_refused() {
        // A container claiming more children than exist means the array was
        // built by something that is not this compiler. Better to say so than
        // to render whatever happens to be lying around.
        let mut writer = Writer::new();
        writer.node(NodeKind::Column, 3);
        let message = writer.decode().unwrap_err().to_string();
        assert!(message.contains("post-order"), "{message}");
    }

    #[test]
    fn several_roots_are_refused() {
        let mut writer = Writer::new();
        writer.node(NodeKind::Spacer, 0);
        writer.node(NodeKind::Spacer, 0);
        let message = writer.decode().unwrap_err().to_string();
        assert!(message.contains("one Node"), "{message}");
    }

    #[test]
    fn a_frame_running_past_memory_is_refused() {
        let writer = Writer { memory: vec![0; 32], count: 1 };
        assert!(super::decode(&Theme::default(), &writer.memory, BASE, 1).is_err());
    }

    #[test]
    fn the_arena_holds_what_it_says_it_holds() {
        // The guest traps past this many nodes, so the host must be able to
        // read a frame that fills the arena exactly.
        let mut writer = Writer::new();
        writer.memory = vec![0; (BASE + NODE_CAPACITY * NODE_SIZE + 64) as usize];
        for _ in 0..NODE_CAPACITY - 1 {
            writer.node(NodeKind::Spacer, 0);
        }
        writer.node(NodeKind::Column, NODE_CAPACITY as usize - 1);
        let tree = writer.decode().expect("a full arena still decodes");
        let Node::Column { children, .. } = &tree else { panic!("expected a column") };
        assert_eq!(children.len(), NODE_CAPACITY as usize - 1);
    }
}
