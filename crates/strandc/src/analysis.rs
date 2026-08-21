//! What the checker learns about source positions, kept instead of discarded.
//!
//! The checker resolves every name and infers every expression's type, then
//! throws all of it away: scopes are popped, and the HIR it produces carries no
//! spans at all — `ExprKind::Local(3)` has lost both the name and the place it
//! was written. That is the right shape for codegen, which only needs slots.
//!
//! An editor needs the opposite: given a byte offset, what is the type here, and
//! where was this name declared. Recording those two facts as the checker
//! already computes them is far cheaper than threading spans through the whole
//! HIR, and it leaves the compilation path untouched.

use crate::hir::{Hir, Ty};
use crate::lexer::Span;

/// Position-indexed facts about one checked file.
#[derive(Debug, Default, Clone)]
pub struct Analysis {
    /// Each expression's source range and inferred type.
    pub types: Vec<(Span, Ty)>,
    /// Each name's use site paired with the span of its declaration.
    pub definitions: Vec<(Span, Span)>,
    /// A description that reads better than the expression's type.
    ///
    /// Builder calls are the case that needs it: `column(gap: 4)` has type
    /// `Node`, which is true and says nothing about what `column` takes — and
    /// unlike a function, there is no declaration to go to and read.
    pub descriptions: Vec<(Span, String)>,
}

fn contains(span: Span, offset: usize) -> bool {
    // Inclusive of `end` so a cursor resting just after the last character of a
    // name still counts as being on it, which is where editors often put it.
    span.start <= offset && offset <= span.end
}

fn width(span: Span) -> usize {
    span.end.saturating_sub(span.start)
}

impl Analysis {
    /// The type of the innermost expression covering `offset`.
    ///
    /// Expressions nest, so several spans can contain the same offset — in
    /// `a + b` the offset of `a` is inside both `a` and the whole sum. The
    /// narrowest is the one the cursor is really on.
    pub fn type_at(&self, offset: usize) -> Option<&Ty> {
        self.types
            .iter()
            .filter(|(span, _)| contains(*span, offset))
            .min_by_key(|(span, _)| width(*span))
            .map(|(_, ty)| ty)
    }

    /// Where the name at `offset` was declared.
    pub fn definition_at(&self, offset: usize) -> Option<Span> {
        self.definitions
            .iter()
            .filter(|(use_site, _)| contains(*use_site, offset))
            .min_by_key(|(use_site, _)| width(*use_site))
            .map(|(_, declared_at)| *declared_at)
    }

    /// Every use of whatever is declared or used at `offset`, including the
    /// declaration itself when it is inside the file.
    ///
    /// Works from either end: the cursor may be on a use or on the declaration.
    pub fn references_at(&self, offset: usize) -> Vec<Span> {
        let target = self.definition_at(offset).or_else(|| {
            // The cursor is on a declaration rather than a use.
            self.definitions
                .iter()
                .map(|(_, declared_at)| *declared_at)
                .filter(|declared_at| contains(*declared_at, offset))
                .min_by_key(|declared_at| width(*declared_at))
        });
        let Some(target) = target else { return Vec::new() };

        let mut out: Vec<Span> = self
            .definitions
            .iter()
            .filter(|(_, declared_at)| *declared_at == target)
            .map(|(use_site, _)| *use_site)
            .collect();
        out.push(target);
        out.sort_by_key(|span| span.start);
        out.dedup();
        out
    }

    /// The description covering `offset`, if anything there has one.
    pub fn description_at(&self, offset: usize) -> Option<&str> {
        self.descriptions
            .iter()
            .filter(|(span, _)| contains(*span, offset))
            .min_by_key(|(span, _)| width(*span))
            .map(|(_, text)| text.as_str())
    }

    /// Renders the type at `offset` the way it is written in source.
    pub fn type_label_at(&self, offset: usize, hir: &Hir) -> Option<String> {
        self.type_at(offset).map(|ty| hir.ty(ty))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: usize, end: usize) -> Span {
        Span { start, end, line: 1, col: 1 }
    }

    #[test]
    fn the_innermost_expression_wins() {
        let analysis = Analysis {
            types: vec![(span(0, 10), Ty::Int), (span(0, 1), Ty::Bool)],
            ..Default::default()
        };
        assert_eq!(analysis.type_at(0), Some(&Ty::Bool), "the narrower span");
        assert_eq!(analysis.type_at(5), Some(&Ty::Int), "only the wider one covers this");
    }

    #[test]
    fn a_cursor_just_past_a_name_still_counts() {
        let analysis =
            Analysis { types: vec![(span(4, 7), Ty::Str)], ..Default::default() };
        assert_eq!(analysis.type_at(7), Some(&Ty::Str));
        assert_eq!(analysis.type_at(8), None);
    }

    #[test]
    fn definitions_resolve_from_a_use() {
        let analysis = Analysis {
            definitions: vec![(span(20, 21), span(4, 5)), (span(30, 31), span(4, 5))],
            ..Default::default()
        };
        assert_eq!(analysis.definition_at(20), Some(span(4, 5)));
        assert_eq!(analysis.definition_at(30), Some(span(4, 5)));
        assert_eq!(analysis.definition_at(99), None);
    }

    #[test]
    fn references_are_found_from_a_use_site() {
        let analysis = Analysis {
            definitions: vec![(span(20, 21), span(4, 5)), (span(30, 31), span(4, 5))],
            ..Default::default()
        };
        let refs = analysis.references_at(20);
        assert_eq!(refs, vec![span(4, 5), span(20, 21), span(30, 31)]);
    }

    #[test]
    fn references_are_found_from_the_declaration_too() {
        let analysis = Analysis {
            definitions: vec![(span(20, 21), span(4, 5)), (span(30, 31), span(4, 5))],
            ..Default::default()
        };
        assert_eq!(analysis.references_at(4), analysis.references_at(20));
    }

    #[test]
    fn unrelated_declarations_do_not_bleed_together() {
        let analysis = Analysis {
            definitions: vec![(span(20, 21), span(4, 5)), (span(30, 31), span(9, 10))],
            ..Default::default()
        };
        assert_eq!(analysis.references_at(20), vec![span(4, 5), span(20, 21)]);
        assert_eq!(analysis.references_at(30), vec![span(9, 10), span(30, 31)]);
    }

    #[test]
    fn nothing_at_the_offset_is_not_an_error() {
        let analysis = Analysis::default();
        assert_eq!(analysis.type_at(0), None);
        assert_eq!(analysis.definition_at(0), None);
        assert!(analysis.references_at(0).is_empty());
    }
}
