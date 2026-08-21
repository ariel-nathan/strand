//! What a UI actor receives when someone touches it (§6.1, §6.5).
//!
//! §6.1 routes input back to the actor that owns the hit node, and §6.5 makes
//! events messages like any other. So a UI actor's mailbox carries this type,
//! and `receive` matches on it the way it would match on anything else — the
//! platform adds no second event mechanism.
//!
//! ## Why the platform declares it
//!
//! The alternative was to let an actor declare its own event type and have the
//! host fill in variants whose names it recognised. That is a protocol held
//! together by spelling: rename `Click` to `Pressed` and the actor silently
//! stops receiving clicks, with nothing to catch it. Declaring the type here
//! means the checker knows it, `match` is exhaustive over it, and a typo is a
//! compile error like any other.
//!
//! ## Why it is opt-in
//!
//! Registering the type also registers `Click`, `Enter` and the rest as
//! constructors, and those are ordinary names a UI program might well want. So
//! it appears only in a module that asks for it by writing `message: Input` —
//! and a module that declares its own `type Input` keeps its own. Nothing is
//! reserved in a file that never mentions it.
//!
//! Every field is `int` or `float`, so the type satisfies §5.3's flatness rule
//! for free: input crosses into the actor's arena carrying no pointers.

/// The name a module writes to opt in.
pub const TYPE_NAME: &str = "Input";

/// The type of one variant field. Deliberately only the flat scalars — see the
/// module comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Int,
    Float,
}

#[derive(Debug, Clone, Copy)]
pub struct Variant {
    pub name: &'static str,
    pub fields: &'static [(&'static str, Field)],
}

/// The vocabulary. Adding an event is adding a row here and a match arm in the
/// host's translation from `InputEvent`.
pub const VARIANTS: &[Variant] = &[
    // A pointer press landed on the node with this id.
    Variant { name: "Click", fields: &[("id", Field::Int)] },
    // A character was typed. Carries the Unicode scalar value, because a
    // message may not hold a pointer and a string is one.
    Variant { name: "Typed", fields: &[("ch", Field::Int)] },
    Variant { name: "Backspace", fields: &[] },
    Variant { name: "Enter", fields: &[] },
    Variant { name: "Escape", fields: &[] },
    // Keyboard focus moved. `id` is 0 when nothing holds it, the same way a
    // node with no id is 0 in the frame's array.
    Variant { name: "Focus", fields: &[("id", Field::Int)] },
    // A scrollable region has been moved to `offset`. The platform measured
    // the content, so this is a position rather than a request.
    Variant { name: "Scrolled", fields: &[("id", Field::Int), ("offset", Field::Float)] },
];

pub fn variant(name: &str) -> Option<&'static Variant> {
    VARIANTS.iter().find(|variant| variant.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_a_distinct_name() {
        for (index, variant) in VARIANTS.iter().enumerate() {
            assert!(
                VARIANTS.iter().skip(index + 1).all(|other| other.name != variant.name),
                "duplicate variant `{}`",
                variant.name
            );
        }
    }

    #[test]
    fn every_field_is_flat() {
        // §5.3: a message crosses into another arena, so it may carry no
        // pointers. This type has to satisfy that like any other.
        for variant in VARIANTS {
            for (name, field) in variant.fields {
                assert!(
                    matches!(field, Field::Int | Field::Float),
                    "`{}.{name}` is not flat",
                    variant.name
                );
            }
        }
    }

    #[test]
    fn the_variants_a_host_must_translate_are_all_here() {
        // A reminder in test form: the platform delivers exactly these, so a
        // new `InputEvent` needs a row above rather than a special case.
        let names: Vec<&str> = VARIANTS.iter().map(|v| v.name).collect();
        assert_eq!(
            names,
            vec!["Click", "Typed", "Backspace", "Enter", "Escape", "Focus", "Scrolled"]
        );
    }
}
