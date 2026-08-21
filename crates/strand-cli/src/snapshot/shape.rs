//! What a state record looks like, and what changed when it stops matching.
//!
//! §9.3's Tier 2 says the runtime can make sure statically that the shapes
//! match before a swap, and that Erlang's hot code load cannot. This is that
//! check. It is not a safety net around the interesting part — it *is* the
//! interesting part, because it is the reason a swap needs no `migrate` and no
//! version field.
//!
//! Two jobs, deliberately separate. The fingerprint is what travels with a
//! snapshot: one string, compared for equality by a runtime that knows nothing
//! about types (§9.4). The difference is what a person reads when the answer
//! is no — computed here, where both sides are still typed, so the message can
//! name the field that changed rather than print two long strings and leave
//! the reader to diff them.

use std::fmt;

use strandc::hir::{Hir, Ty};

/// A state type, expanded structurally.
///
/// Names are kept because a rename is a change: the bytes would still line up,
/// and the meaning would not. Everything that decides layout is here, and
/// nothing that does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    Scalar(&'static str),
    List(Box<Shape>),
    Option(Box<Shape>),
    Result(Box<Shape>, Box<Shape>),
    Record { name: String, fields: Vec<(String, Shape)> },
    Sum { name: String, variants: Vec<(String, Vec<(String, Shape)>)> },
    /// A type already being expanded further out. A record can reach itself
    /// through an `Option`, and naming it once is enough to pin the shape.
    Again(String),
}

impl Shape {
    /// What to call this type in a sentence. The full expansion is the
    /// fingerprint, and a record with nine fields makes a poor line of output.
    pub fn name(&self) -> String {
        match self {
            Shape::Record { name, .. } | Shape::Sum { name, .. } | Shape::Again(name) => {
                name.clone()
            }
            other => other.to_string(),
        }
    }
}

/// Expands `ty` into the fingerprint that travels with a snapshot.
pub fn shape(hir: &Hir, ty: &Ty) -> Shape {
    expand(hir, ty, &mut Vec::new())
}

fn expand(hir: &Hir, ty: &Ty, open: &mut Vec<String>) -> Shape {
    match ty {
        Ty::Int => Shape::Scalar("int"),
        Ty::Float => Shape::Scalar("float"),
        Ty::Bool => Shape::Scalar("bool"),
        Ty::Str => Shape::Scalar("string"),
        Ty::Unit => Shape::Scalar("unit"),
        // None of these can be part of a state record; the checker refuses
        // them long before a snapshot is taken.
        Ty::Node | Ty::Never | Ty::Error => Shape::Scalar("?"),
        Ty::List(inner) => Shape::List(Box::new(expand(hir, inner, open))),
        Ty::Option(inner) => Shape::Option(Box::new(expand(hir, inner, open))),
        Ty::Result(ok, err) => Shape::Result(
            Box::new(expand(hir, ok, open)),
            Box::new(expand(hir, err, open)),
        ),
        Ty::Record(id) => {
            let def = &hir.records[id.0 as usize];
            if open.contains(&def.name) {
                return Shape::Again(def.name.clone());
            }
            open.push(def.name.clone());
            let fields = def
                .fields
                .iter()
                .map(|(name, ty)| (name.clone(), expand(hir, ty, open)))
                .collect();
            open.pop();
            Shape::Record { name: def.name.clone(), fields }
        }
        Ty::Sum(id) => {
            let def = &hir.sums[id.0 as usize];
            if open.contains(&def.name) {
                return Shape::Again(def.name.clone());
            }
            open.push(def.name.clone());
            let variants = def
                .variants
                .iter()
                .map(|variant| {
                    let fields = variant
                        .fields
                        .iter()
                        .map(|(name, ty)| (name.clone(), expand(hir, ty, open)))
                        .collect();
                    (variant.name.clone(), fields)
                })
                .collect();
            open.pop();
            Shape::Sum { name: def.name.clone(), variants }
        }
    }
}

impl fmt::Display for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Shape::Scalar(name) => write!(f, "{name}"),
            Shape::List(inner) => write!(f, "List<{inner}>"),
            Shape::Option(inner) => write!(f, "Option<{inner}>"),
            Shape::Result(ok, err) => write!(f, "Result<{ok}, {err}>"),
            Shape::Again(name) => write!(f, "{name}…"),
            Shape::Record { name, fields } => {
                let fields: Vec<String> =
                    fields.iter().map(|(n, shape)| format!("{n}: {shape}")).collect();
                write!(f, "{name} {{ {} }}", fields.join(", "))
            }
            Shape::Sum { name, variants } => {
                let variants: Vec<String> = variants
                    .iter()
                    .map(|(n, fields)| {
                        if fields.is_empty() {
                            n.clone()
                        } else {
                            let fields: Vec<String> =
                                fields.iter().map(|(n, s)| format!("{n}: {s}")).collect();
                            format!("{n}({})", fields.join(", "))
                        }
                    })
                    .collect();
                write!(f, "{name} = | {}", variants.join(" | "))
            }
        }
    }
}

/// The first place two shapes stop agreeing, in words.
///
/// One difference, not a list: the reader needs to know *that* the state
/// cannot be carried over and *why*, and the first honest reason is enough to
/// act on. `None` means the two are the same shape and a snapshot moves
/// between them.
pub fn difference(old: &Shape, new: &Shape) -> Option<String> {
    walk(&old.name(), old, new)
}

fn walk(path: &str, old: &Shape, new: &Shape) -> Option<String> {
    match (old, new) {
        (Shape::Scalar(a), Shape::Scalar(b)) if a == b => None,
        (Shape::Again(a), Shape::Again(b)) if a == b => None,
        (Shape::List(a), Shape::List(b)) => walk(&format!("{path}'s elements"), a, b),
        (Shape::Option(a), Shape::Option(b)) => walk(path, a, b),
        (Shape::Result(a, ae), Shape::Result(b, be)) => {
            walk(path, a, b).or_else(|| walk(&format!("{path}'s error"), ae, be))
        }
        (
            Shape::Record { name: old_name, fields: old_fields },
            Shape::Record { name: new_name, fields: new_fields },
        ) => {
            if old_name != new_name {
                return Some(format!("`{path}` was `{old_name}` and is now `{new_name}`"));
            }
            members(path, "field", old_fields, new_fields)
        }
        (
            Shape::Sum { name: old_name, variants: old_variants },
            Shape::Sum { name: new_name, variants: new_variants },
        ) => {
            if old_name != new_name {
                return Some(format!("`{path}` was `{old_name}` and is now `{new_name}`"));
            }
            if old_variants.len() != new_variants.len() {
                return Some(format!(
                    "`{path}` had {} variants and now has {}",
                    old_variants.len(),
                    new_variants.len()
                ));
            }
            for ((old_case, old_fields), (new_case, new_fields)) in
                old_variants.iter().zip(new_variants)
            {
                if old_case != new_case {
                    return Some(format!(
                        "`{path}`'s variant `{old_case}` is now `{new_case}`"
                    ));
                }
                let inner = format!("{path}.{old_case}");
                if let Some(found) = members(&inner, "field", old_fields, new_fields) {
                    return Some(found);
                }
            }
            None
        }
        _ => Some(format!("`{path}` was `{old}` and is now `{new}`")),
    }
}

/// Compares a record's fields or a variant's, in order. Order matters as much
/// as the names do: two fields that swapped places have swapped offsets.
fn members(
    path: &str,
    kind: &str,
    old: &[(String, Shape)],
    new: &[(String, Shape)],
) -> Option<String> {
    for (index, (name, shape)) in old.iter().enumerate() {
        match new.get(index) {
            None => return Some(format!("`{path}` lost the {kind} `{name}: {shape}`")),
            Some((new_name, new_shape)) => {
                if name != new_name {
                    return Some(format!(
                        "`{path}`'s {kind} {} is `{new_name}` where it was `{name}`",
                        index + 1
                    ));
                }
                if let Some(found) = walk(&format!("{path}.{name}"), shape, new_shape) {
                    return Some(found);
                }
            }
        }
    }
    if let Some((name, shape)) = new.get(old.len()) {
        return Some(format!("`{path}` gained the {kind} `{name}: {shape}`"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shapes(source: &str) -> Shape {
        let hir = match strandc::compile("t.str", source) {
            Ok(hir) => hir,
            Err(report) => panic!("{:?}", miette::Report::new(report)),
        };
        let actor = hir.lone_actor().expect("one actor").clone();
        shape(&hir, &actor.state)
    }

    const BEFORE: &str = "\
type Model = { count: int, note: string }
actor A {
  state: Model
  in input: Input
  fn init(): Model { Model { count: 0, note: \"\" } }
  on input(state: Model, msg: Input): Model { state }
}
";

    fn changed(source: &str) -> String {
        difference(&shapes(BEFORE), &shapes(source)).expect("these differ")
    }

    #[test]
    fn the_same_source_has_the_same_shape() {
        assert_eq!(difference(&shapes(BEFORE), &shapes(BEFORE)), None);
    }

    #[test]
    fn a_body_can_change_without_the_shape_changing() {
        // This is the whole point of Tier 2: new behaviour, same record, so
        // the state moves across.
        let after = BEFORE.replace("Model { count: 0", "Model { count: 41 + 1");
        assert_eq!(difference(&shapes(BEFORE), &shapes(&after)), None);
    }

    #[test]
    fn a_new_field_is_named() {
        let after = BEFORE
            .replace("{ count: int, note: string }", "{ count: int, note: string, done: bool }")
            .replace("note: \"\" }", "note: \"\", done: false }");
        assert_eq!(changed(&after), "`Model` gained the field `done: bool`");
    }

    #[test]
    fn a_changed_field_type_says_both_types() {
        let after = BEFORE
            .replace("note: string }", "note: int }")
            .replace("note: \"\" }", "note: 0 }");
        assert_eq!(changed(&after), "`Model.note` was `string` and is now `int`");
    }

    #[test]
    fn a_field_that_became_optional_is_a_change() {
        // The bytes would even line up in one direction, which is exactly why
        // this has to be caught by the type rather than by the size.
        let after = BEFORE
            .replace("note: string }", "note: Option<string> }")
            .replace("note: \"\" }", "note: Some(\"\") }");
        assert_eq!(changed(&after), "`Model.note` was `string` and is now `Option<string>`");
    }

    #[test]
    fn reordered_fields_are_a_change() {
        // Same names, same types, different offsets. Nothing but order tells
        // these apart, and order is what the layout is made of.
        let after = BEFORE
            .replace("{ count: int, note: string }", "{ note: string, count: int }")
            .replace("Model { count: 0, note: \"\" }", "Model { note: \"\", count: 0 }");
        assert_eq!(changed(&after), "`Model`'s field 1 is `note` where it was `count`");
    }

    #[test]
    fn a_renamed_record_is_a_change() {
        let after = BEFORE.replace("Model", "State");
        assert_eq!(changed(&after), "`Model` was `Model` and is now `State`");
    }

    #[test]
    fn a_change_inside_a_list_element_is_found() {
        let before = "\
type Item = { id: int }
type Model = { items: List<Item> }
actor A {
  state: Model
  in input: Input
  fn init(): Model { Model { items: [] } }
  on input(state: Model, msg: Input): Model { state }
}
";
        let after = before.replace("{ id: int }", "{ id: int, title: string }");
        let found = difference(&shapes(before), &shapes(&after)).expect("these differ");
        assert_eq!(found, "`Model.items's elements` gained the field `title: string`");
    }

    #[test]
    fn a_fingerprint_reads_like_the_source_it_came_from() {
        assert_eq!(shapes(BEFORE).to_string(), "Model { count: int, note: string }");
    }
}
