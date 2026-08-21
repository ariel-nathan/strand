//! What an actor is told when a peer of its dies or comes back (§5.4).
//!
//! §5.4 says a panic delivers a typed `ChildDown` to the supervisor. In the
//! POC the supervisor is the host, and until now that was where the story
//! stopped — a guest could not learn that anything had happened. §7's demo
//! needs it to: the Stats actor is crashed on purpose, and what reviewers are
//! meant to see is the UI showing a failure boundary for a beat and then the
//! counts coming back.
//!
//! ```text
//! Down(port: int)     the peer feeding this port of mine has died
//! Up(port: int)       a fresh one has taken its place
//! ```
//!
//! ## Why the peer is named by a port rather than by an actor
//!
//! Because there is no other name available, and that is the point rather than
//! a limitation. An actor holds no addresses (`docs/abi.md` §7), so "who died"
//! can only be said in terms the receiver already has: `port` is the index of
//! *its own* `in` port that the departed peer was wired to. An actor with one
//! peer can ignore the payload; an actor with several can tell them apart
//! without ever learning who they are.
//!
//! Honest gap: comparing against that index means writing the number, because
//! there is no way yet to spell "the index of my `tally` port". With one peer
//! it does not come up, and inventing the syntax before something needs it
//! would be guessing at the shape.
//!
//! ## Why it is opt-in, and declared here
//!
//! The same two reasons as `input.rs`. Registering the type also registers
//! `Down` and `Up` as constructors, which are ordinary names a program might
//! want; and matching the host's notion of the type against a name the user
//! chose would be a protocol held together by spelling.

use crate::input::{Field, Variant};

/// The name a module writes to opt in.
pub const TYPE_NAME: &str = "Lifecycle";

/// The vocabulary. Both fields are `int`, so §7's flatness rule holds for free.
pub const VARIANTS: &[Variant] = &[
    // The peer wired to this `in` port of mine is gone. Its arena went with
    // it, so whatever it had computed is gone too.
    Variant { name: "Down", fields: &[("port", Field::Int)] },
    // The supervisor put a fresh one in its place. It starts from `init`, so
    // anything it needs to know has to be told to it again.
    Variant { name: "Up", fields: &[("port", Field::Int)] },
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
        // §7: this crosses into another arena like any message, so it may
        // carry no pointers.
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
    fn a_death_and_a_return_are_shaped_alike() {
        // The UI treats them as one event with a direction, and a payload that
        // differed between them would make that harder for no reason.
        let down = variant("Down").expect("Down");
        let up = variant("Up").expect("Up");
        assert_eq!(down.fields.len(), up.fields.len());
        assert_eq!(down.fields[0].0, up.fields[0].0);
    }
}
