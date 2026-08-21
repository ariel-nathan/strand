//! The handful of string operations a UI cannot be written without.
//!
//! §4.6 defers the stdlib past M1, and this is not it: it is the smallest set
//! that lets a text field hold what you type and a count reach the screen.
//! Appending a character, dropping one, measuring, trimming, and turning a
//! number into something drawable — every one of those is reachable from
//! §7's demo script, and none of them can be written in the language as it
//! stands.
//!
//! ## Free functions, not methods
//!
//! §4.5 writes `title.trim().isEmpty()`. Method-call syntax needs a receiver to
//! resolve against, which needs either traits or a name-to-type table, and both
//! are larger decisions than this file should force. So these are free
//! functions today — `isEmpty(trim(title))` — and method syntax can arrive
//! later as sugar that resolves to exactly these, without any of them changing.
//!
//! ## Guest functions, not host calls
//!
//! Unlike `log`, none of these leave the actor. They are emitted into the
//! module as ordinary WASM alongside the bump allocator, so a program that uses
//! them still instantiates with no imports at all, and `strand run` needs no
//! runtime under it.

use crate::hir::Helper;

/// The types these functions speak. Only scalars and strings: nothing here
/// needs the full `Ty`, and keeping the table const-constructible is worth more
/// than generality it would not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    Str,
    Bool,
}

/// How a call is realised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    /// A generated WASM helper, called directly.
    Helper(Helper),
    /// `isEmpty(s)` is `len(s) == 0`, built by the checker out of pieces it
    /// already has. A helper of its own would be a second way to ask the same
    /// question.
    LengthIsZero,
}

#[derive(Debug, Clone, Copy)]
pub struct Fun {
    pub name: &'static str,
    pub params: &'static [Kind],
    pub ret: Kind,
    pub body: Body,
    /// One line, for hover and for the "did you mean" half of a diagnostic.
    pub doc: &'static str,
}

impl Fun {
    /// How the function reads as a declaration.
    pub fn signature(&self) -> String {
        let name = |kind: &Kind| match kind {
            Kind::Int => "int",
            Kind::Str => "string",
            Kind::Bool => "bool",
        };
        let params: Vec<String> = self
            .params
            .iter()
            .zip(self.param_names())
            .map(|(kind, label)| format!("{label}: {}", name(kind)))
            .collect();
        format!("fn {}({}): {}", self.name, params.join(", "), name(&self.ret))
    }

    /// Parameter names, so a signature reads like source rather than like a
    /// type list.
    fn param_names(&self) -> &'static [&'static str] {
        match self.name {
            "char" => &["code"],
            "str" => &["value"],
            _ => &["s"],
        }
    }
}

pub const FUNCTIONS: &[Fun] = &[
    Fun {
        name: "str",
        params: &[Kind::Int],
        ret: Kind::Str,
        body: Body::Helper(Helper::StrFromInt),
        doc: "The number written out in decimal.",
    },
    Fun {
        name: "char",
        params: &[Kind::Int],
        ret: Kind::Str,
        body: Body::Helper(Helper::StrFromChar),
        doc: "One character, from the Unicode scalar value an `Input::Typed` carries.",
    },
    Fun {
        name: "len",
        params: &[Kind::Str],
        ret: Kind::Int,
        body: Body::Helper(Helper::StrCharCount),
        doc: "How many characters — not bytes, so `é` counts once.",
    },
    Fun {
        name: "isEmpty",
        params: &[Kind::Str],
        ret: Kind::Bool,
        body: Body::LengthIsZero,
        doc: "Whether there is nothing in it.",
    },
    Fun {
        name: "trim",
        params: &[Kind::Str],
        ret: Kind::Str,
        body: Body::Helper(Helper::StrTrim),
        doc: "The same text without leading or trailing whitespace.",
    },
    Fun {
        name: "dropLast",
        params: &[Kind::Str],
        ret: Kind::Str,
        body: Body::Helper(Helper::StrDropLast),
        doc: "The same text with its last character removed — what Backspace does.",
    },
];

pub fn lookup(name: &str) -> Option<&'static Fun> {
    FUNCTIONS.iter().find(|fun| fun.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_function_has_a_distinct_name() {
        for (index, fun) in FUNCTIONS.iter().enumerate() {
            assert!(
                FUNCTIONS.iter().skip(index + 1).all(|other| other.name != fun.name),
                "duplicate function `{}`",
                fun.name
            );
        }
    }

    #[test]
    fn a_signature_names_its_parameters() {
        assert_eq!(lookup("str").expect("str").signature(), "fn str(value: int): string");
        assert_eq!(lookup("len").expect("len").signature(), "fn len(s: string): int");
        assert_eq!(lookup("char").expect("char").signature(), "fn char(code: int): string");
    }

    #[test]
    fn every_signature_names_every_parameter() {
        // A signature missing a name would silently drop a parameter.
        for fun in FUNCTIONS {
            assert_eq!(
                fun.params.len(),
                fun.param_names().len(),
                "`{}` has {} parameters but {} names",
                fun.name,
                fun.params.len(),
                fun.param_names().len()
            );
        }
    }
}
