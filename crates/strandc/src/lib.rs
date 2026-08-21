//! The Strand compiler (§4.6): lexer, parser, checker, WASM emitter.

pub mod analysis;
pub mod ast;
pub mod check;
pub mod codegen;
pub mod diag;
pub mod hir;
pub mod input;
pub mod lexer;
pub mod line_index;
pub mod parser;
pub mod ui;

/// Front end: source to typed IR, with every diagnostic gathered against the
/// original source so §8.2 rendering works.
pub fn compile(path: &str, src: &str) -> Result<hir::Hir, diag::Report> {
    let program = parser::parse(src).map_err(|d| diag::Report::new(path, src, vec![d]))?;
    check::check(&program).map_err(|ds| diag::Report::new(path, src, ds))
}
