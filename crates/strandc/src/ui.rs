//! The builder vocabulary (§6.2) and the layout a built tree has in memory.
//!
//! This module is deliberately the *only* description of either. The parser
//! asks it which names take a trailing block, the checker asks it what
//! arguments they take, codegen asks it where the fields go — and so does the
//! host that reads the finished array. One table, four readers, no chance of
//! the two ends disagreeing about a byte.
//!
//! ## Why the array is post-order
//!
//! A view emits nodes as it evaluates: children are finished before the
//! container that holds them, so the natural output is post-order, and the
//! container records how many of the preceding roots are its own. Rebuilding
//! the tree is then a single left-to-right pass with a stack, and no node ever
//! needs to be moved or patched after it is written.
//!
//! That is the Cap'n Proto lesson from `docs/inspiration-canon.md` applied to
//! §6.1's tree: the format the guest builds *is* the format the host reads.
//! There is no encode step, and adding one would be the bug.
//!
//! ## Why the vocabulary is the widget set
//!
//! §6.4 fixes the POC widget set, and §6.3 makes the theme a platform
//! primitive rather than something app code assembles. So a builder names a
//! widget and passes it typed props; the colours and spacing that widget uses
//! stay on the platform side, where §6.3 says design tokens belong. Raw boxes
//! with arbitrary styling are the same machinery with a wider table, and can
//! land when there is a reason to want them.

/// What a node is. The tag is written into the array, so the host reads back
/// exactly what the view asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Screen,
    Column,
    Row,
    Panel,
    Scroll,
    Text,
    Muted,
    Button,
    Checkbox,
    TextInput,
    Spacer,
}

impl NodeKind {
    pub fn tag(self) -> i32 {
        self as i32
    }

    pub fn from_tag(tag: i32) -> Option<Self> {
        use NodeKind::*;
        Some(match tag {
            0 => Screen,
            1 => Column,
            2 => Row,
            3 => Panel,
            4 => Scroll,
            5 => Text,
            6 => Muted,
            7 => Button,
            8 => Checkbox,
            9 => TextInput,
            10 => Spacer,
            _ => return None,
        })
    }
}

/// Which field of a node record a prop lands in.
///
/// Every builder writes into the same six slots. A uniform record costs a few
/// unused words per node and buys a decoder with no per-kind branching —
/// worth it for an array that is rebuilt from scratch every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Hit id. Zero means the node takes no input.
    Id,
    /// A single bool: `checked`, `focused`.
    Flag,
    /// A Strand string pointer, or zero.
    Text,
    /// A second string, for the one widget that needs two.
    Text2,
    /// `gap`, or a scroll's `offset`.
    Number,
    /// `padding`.
    Number2,
}

impl Slot {
    /// Byte offset within a node record.
    pub fn offset(self) -> u32 {
        match self {
            Slot::Id => 8,
            Slot::Flag => 12,
            Slot::Text => 16,
            Slot::Number => 20,
            Slot::Number2 => 24,
            Slot::Text2 => 28,
        }
    }

    /// Whether the slot holds an f32 rather than an i32.
    pub fn is_float(self) -> bool {
        matches!(self, Slot::Number | Slot::Number2)
    }
}

/// Offset of the node's kind tag.
pub const KIND_OFFSET: u32 = 0;
/// Offset of the count of directly-owned children.
pub const CHILD_COUNT_OFFSET: u32 = 4;
/// Bytes per node record.
pub const NODE_SIZE: u32 = 32;
/// How many nodes one frame may contain.
///
/// A fixed ceiling rather than a growing buffer, following the arena
/// philosophy `docs/inspiration-canon.md` takes from TigerBeetle: the memory a
/// frame can use is decided once, and a view that exceeds it traps rather than
/// quietly allocating. A trap is a crash report and a supervisor restart, which
/// is a great deal easier to notice than a slow leak.
pub const NODE_CAPACITY: u32 = 2048;
/// Bytes the node arena occupies.
pub const NODE_ARENA_BYTES: u32 = NODE_SIZE * NODE_CAPACITY;

/// The type a prop argument must have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropTy {
    Int,
    Float,
    Bool,
    Str,
}

#[derive(Debug, Clone, Copy)]
pub struct Prop {
    pub name: &'static str,
    pub ty: PropTy,
    pub slot: Slot,
    /// `None` means the prop must be written. Numbers carry their default
    /// here; unwritten ids, flags and strings are zero, which the host reads
    /// as "no id", "false" and "no text".
    pub default: Option<f32>,
}

impl Prop {
    /// `gap: int = 0`, the way it would be written as a parameter.
    pub fn render(&self) -> String {
        let ty = match self.ty {
            PropTy::Int => "int",
            PropTy::Float => "float",
            PropTy::Bool => "bool",
            PropTy::Str => "string",
        };
        match (self.default, self.ty) {
            (None, _) => format!("{}: {ty}", self.name),
            (Some(default), PropTy::Int) => format!("{}: {ty} = {}", self.name, default as i64),
            (Some(default), PropTy::Bool) => {
                format!("{}: {ty} = {}", self.name, default != 0.0)
            }
            (Some(default), _) => format!("{}: {ty} = {default:?}", self.name),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Builder {
    pub name: &'static str,
    pub kind: NodeKind,
    pub props: &'static [Prop],
    /// Whether this builder takes a trailing block of children.
    pub container: bool,
}

impl Builder {
    /// How the builder reads as a declaration.
    ///
    /// A builder has no declaration in the file to go to, so this is the only
    /// way to find out what it takes — which makes it the answer for hover, and
    /// for the two diagnostics that have to say what was expected. One
    /// renderer, so an editor and the compiler cannot describe the same builder
    /// differently.
    pub fn signature(&self) -> String {
        let props: Vec<String> = self.props.iter().map(Prop::render).collect();
        format!(
            "{}({}){} -> Node",
            self.name,
            props.join(", "),
            if self.container { " { … }" } else { "" }
        )
    }
}

const fn required(name: &'static str, ty: PropTy, slot: Slot) -> Prop {
    Prop { name, ty, slot, default: None }
}

const fn optional(name: &'static str, ty: PropTy, slot: Slot, default: f32) -> Prop {
    Prop { name, ty, slot, default: Some(default) }
}

/// `gap` and `padding`, the two spacing props every container takes.
///
/// Ints, because §6.3's unit is the logical pixel and §6.2 writes
/// `column(gap: 4)`. A prop that is fractional in its own right — a scroll's
/// offset — is a float, and nothing coerces silently between them.
macro_rules! spacing {
    ($gap:expr, $padding:expr) => {
        &[
            optional("gap", PropTy::Int, Slot::Number, $gap),
            optional("padding", PropTy::Int, Slot::Number2, $padding),
        ]
    };
}

/// The vocabulary. Adding a widget is adding a row here and a match arm in the
/// host's decoder — nothing in the parser, the checker or codegen.
pub const BUILDERS: &[Builder] = &[
    Builder {
        name: "screen",
        kind: NodeKind::Screen,
        props: spacing!(8.0, 12.0),
        container: true,
    },
    Builder { name: "column", kind: NodeKind::Column, props: spacing!(0.0, 0.0), container: true },
    Builder { name: "row", kind: NodeKind::Row, props: spacing!(0.0, 0.0), container: true },
    Builder { name: "panel", kind: NodeKind::Panel, props: spacing!(8.0, 12.0), container: true },
    Builder {
        name: "scroll",
        kind: NodeKind::Scroll,
        props: &[
            required("id", PropTy::Int, Slot::Id),
            optional("offset", PropTy::Float, Slot::Number, 0.0),
        ],
        container: true,
    },
    Builder {
        name: "text",
        kind: NodeKind::Text,
        props: &[required("value", PropTy::Str, Slot::Text)],
        container: false,
    },
    Builder {
        name: "muted",
        kind: NodeKind::Muted,
        props: &[required("value", PropTy::Str, Slot::Text)],
        container: false,
    },
    Builder {
        name: "button",
        kind: NodeKind::Button,
        props: &[
            required("id", PropTy::Int, Slot::Id),
            required("label", PropTy::Str, Slot::Text),
        ],
        container: false,
    },
    Builder {
        name: "checkbox",
        kind: NodeKind::Checkbox,
        props: &[
            required("id", PropTy::Int, Slot::Id),
            required("checked", PropTy::Bool, Slot::Flag),
            required("label", PropTy::Str, Slot::Text),
        ],
        container: false,
    },
    Builder {
        name: "textInput",
        kind: NodeKind::TextInput,
        props: &[
            required("id", PropTy::Int, Slot::Id),
            required("value", PropTy::Str, Slot::Text),
            optional("focused", PropTy::Bool, Slot::Flag, 0.0),
            required("prompt", PropTy::Str, Slot::Text2),
        ],
        container: false,
    },
    Builder { name: "spacer", kind: NodeKind::Spacer, props: &[], container: false },
];

pub fn lookup(name: &str) -> Option<&'static Builder> {
    BUILDERS.iter().find(|builder| builder.name == name)
}

/// Whether a name is a builder at all — the parser routes these to the builder
/// path rather than to an ordinary call.
pub fn is_builder(name: &str) -> bool {
    lookup(name).is_some()
}

/// Whether a following `{` opens a block of children rather than starting a new
/// statement. Only containers have children, so only containers claim one.
pub fn takes_children(name: &str) -> bool {
    lookup(name).is_some_and(|builder| builder.container)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tag_round_trips() {
        for builder in BUILDERS {
            assert_eq!(NodeKind::from_tag(builder.kind.tag()), Some(builder.kind));
        }
    }

    #[test]
    fn every_builder_has_a_distinct_name_and_kind() {
        for (index, builder) in BUILDERS.iter().enumerate() {
            assert!(
                BUILDERS.iter().skip(index + 1).all(|other| other.name != builder.name),
                "duplicate builder `{}`",
                builder.name
            );
            assert!(
                BUILDERS.iter().skip(index + 1).all(|other| other.kind != builder.kind),
                "two builders share a kind: `{}`",
                builder.name
            );
        }
    }

    #[test]
    fn no_builder_writes_the_same_slot_twice() {
        // Two props landing in one slot would silently overwrite each other.
        for builder in BUILDERS {
            for (index, prop) in builder.props.iter().enumerate() {
                assert!(
                    builder.props.iter().skip(index + 1).all(|other| other.slot != prop.slot),
                    "`{}` writes {:?} twice",
                    builder.name,
                    prop.slot
                );
            }
        }
    }

    #[test]
    fn every_slot_fits_inside_a_record() {
        for slot in [Slot::Id, Slot::Flag, Slot::Text, Slot::Text2, Slot::Number, Slot::Number2] {
            assert!(slot.offset() + 4 <= NODE_SIZE, "{slot:?} runs past the record");
        }
        const { assert!(CHILD_COUNT_OFFSET + 4 <= NODE_SIZE) };
        const { assert!(KIND_OFFSET == 0) };
    }

    #[test]
    fn a_signature_reads_as_a_declaration() {
        let signature = lookup("column").expect("column exists").signature();
        assert_eq!(signature, "column(gap: int = 0, padding: int = 0) { … } -> Node");

        let leaf = lookup("button").expect("button exists").signature();
        assert_eq!(leaf, "button(id: int, label: string) -> Node", "a leaf opens no block");

        let scroll = lookup("scroll").expect("scroll exists").signature();
        assert!(scroll.contains("offset: float = 0.0"), "{scroll}");
    }

    #[test]
    fn only_containers_take_a_trailing_block() {
        assert!(takes_children("column"));
        assert!(takes_children("scroll"));
        assert!(!takes_children("text"), "a leaf has no children to open a block for");
        assert!(is_builder("text"), "but it is still a builder");
        assert!(!is_builder("todoRow"), "and a user function is neither");
    }
}
