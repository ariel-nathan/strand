//! §6.2's builder DSL, end to end.
//!
//! Source in, tree out: each case compiles Strand, runs the emitted view, reads
//! the frame it left in guest memory, and asserts on the tree the host rebuilt.
//! Nothing here stubs a layer — a passing case means the syntax, the checker,
//! the emitted WASM, the flat array and the decoder all agree.

use strand_render::scene::{Command, Layouter, Node};
use strand_render::widgets::Theme;
use strand_cli::view::View;

/// Compiles a view and returns the tree it drew.
fn draw(source: &str) -> Node {
    let hir = match strandc::compile("view.str", source) {
        Ok(hir) => hir,
        Err(report) => panic!("{:?}", miette::Report::new(report)),
    };
    let wasm = strandc::codegen::emit(&hir).expect("emit failed");
    wasmparser::validate(&wasm).expect("emitted invalid WASM");
    View::new(&hir, &wasm)
        .expect("no view found")
        .frame(&Theme::default())
        .expect("the view produced no frame")
}

fn children(node: &Node) -> &[Node] {
    match node {
        Node::Row { children, .. }
        | Node::Column { children, .. }
        | Node::Scroll { children, .. } => children,
        _ => &[],
    }
}

/// Every string the tree would draw, in paint order.
fn labels(node: &Node) -> Vec<String> {
    let mut layouter = Layouter::new();
    layouter
        .layout(node, (800.0, 600.0))
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_view_builds_a_tree() {
    let tree = draw(
        r#"
        view fn main(): Node {
          column(gap: 4) {
            text("first")
            text("second")
          }
        }
        "#,
    );

    assert_eq!(labels(&tree), vec!["first", "second"], "children in the order written");
    assert_eq!(children(&tree).len(), 2);
}

#[test]
fn nesting_is_just_nesting() {
    // §6.2's argument in its smallest form: no `{}` escape hatch, no second
    // syntax mode — a child tree is written where the child goes.
    let tree = draw(
        r#"
        view fn main(): Node {
          column(gap: 4) {
            text("outer")
            row(gap: 2) {
              text("inner a")
              text("inner b")
            }
          }
        }
        "#,
    );

    assert_eq!(labels(&tree), vec!["outer", "inner a", "inner b"]);
    let top = children(&tree);
    assert_eq!(top.len(), 2, "a label and a row");
    assert_eq!(children(&top[1]).len(), 2, "and the row kept both of its own");
}

#[test]
fn conditionals_are_ordinary_ifs() {
    // The `count && <Badge/>` trap, absent by construction: a branch that does
    // not run contributes no child, and there is nothing to render as a literal.
    let source = r#"
        view fn main(): Node {
          column(gap: 4) {
            text("always")
            if SHOW {
              text("sometimes")
            }
          }
        }
    "#;

    assert_eq!(labels(&draw(&source.replace("SHOW", "true"))), vec!["always", "sometimes"]);
    assert_eq!(labels(&draw(&source.replace("SHOW", "false"))), vec!["always"]);
}

#[test]
fn an_if_else_picks_one_child() {
    let tree = draw(
        r#"
        view fn main(): Node {
          column(gap: 4) {
            if 1 > 2 { text("no") } else { text("yes") }
          }
        }
        "#,
    );
    assert_eq!(labels(&tree), vec!["yes"]);
    assert_eq!(children(&tree).len(), 1, "one branch ran, so one child exists");
}

#[test]
fn a_match_can_choose_a_child() {
    let tree = draw(
        r#"
        type Mode = | Compact | Full

        view fn main(): Node {
          column(gap: 4) {
            match Full {
              Compact => text("compact"),
              Full    => text("full"),
            }
          }
        }
        "#,
    );
    assert_eq!(labels(&tree), vec!["full"]);
}

#[test]
fn a_view_composes_by_calling_another_view() {
    // §6.2's `todoRow(t, onToggle)`: composition is a function call, so a view
    // is reusable the way any function is.
    let tree = draw(
        r#"
        type Todo = { title: string, done: bool }

        view fn row_for(index: int, todo: Todo): Node {
          row(gap: 8) {
            checkbox(id: 1000 + index, checked: todo.done, label: todo.title)
          }
        }

        view fn main(): Node {
          column(gap: 4) {
            row_for(0, Todo { title: "a", done: true })
            row_for(1, Todo { title: "b", done: false })
          }
        }
        "#,
    );

    assert_eq!(labels(&tree), vec!["a", "b"]);
    let mut layouter = Layouter::new();
    let ids: Vec<u32> =
        layouter.layout(&tree, (800.0, 600.0)).hits.iter().map(|r| r.id.0).collect();
    assert_eq!(ids, vec![1000, 1001], "each row got the id its view computed");
}

#[test]
fn props_reach_the_widget() {
    let tree = draw(
        r#"
        view fn main(): Node {
          column(gap: 4) {
            checkbox(id: 7, checked: true, label: "ticked")
            button(id: 8, label: "Add")
          }
        }
        "#,
    );

    let mut layouter = Layouter::new();
    let frame = layouter.layout(&tree, (800.0, 600.0));
    let ids: Vec<u32> = frame.hits.iter().map(|r| r.id.0).collect();
    assert_eq!(ids, vec![7, 8]);
    assert_eq!(labels(&tree), vec!["ticked", "Add"]);
}

#[test]
fn spacing_props_change_the_layout() {
    let spaced = draw(
        r#"
        view fn main(): Node {
          column(gap: 40, padding: 10) {
            text("a")
            text("b")
          }
        }
        "#,
    );

    let mut layouter = Layouter::new();
    let frame = layouter.layout(&spaced, (800.0, 600.0));
    let ys: Vec<f32> = frame
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::Text { y, .. } => Some(*y),
            _ => None,
        })
        .collect();
    assert_eq!(ys[0], 10.0, "padding: 10 inset the first label");
    assert_eq!(ys[1] - ys[0], 60.0, "a 20px line plus the 40px gap");
}

#[test]
fn a_default_applies_where_a_prop_is_left_out() {
    let tree = draw(
        r#"
        view fn main(): Node {
          panel() {
            text("a")
          }
        }
        "#,
    );
    let mut layouter = Layouter::new();
    let frame = layouter.layout(&tree, (800.0, 600.0));
    let Command::Text { x, .. } = frame.commands[1] else { panic!("expected the label") };
    assert_eq!(x, 12.0, "panel's default padding");
}

#[test]
fn a_string_prop_carries_a_computed_value() {
    // The string comes out of an ordinary function, so it is built in the
    // guest's arena and read back through its own header (docs/abi.md §5).
    let tree = draw(
        r#"
        fn pick(done: bool): string {
          match done {
            true  => "done",
            false => "todo",
          }
        }

        view fn main(): Node {
          column(gap: 4) {
            text(pick(true))
            text(pick(false))
          }
        }
        "#,
    );
    assert_eq!(labels(&tree), vec!["done", "todo"]);
}

#[test]
fn an_empty_container_is_allowed() {
    let tree = draw(
        r#"
        view fn main(): Node {
          column(gap: 4) {
          }
        }
        "#,
    );
    assert!(children(&tree).is_empty());
    assert!(matches!(tree, Node::Column { .. }));
}

#[test]
fn a_deeply_nested_view_keeps_its_shape() {
    // Post-order with a running count is easy to get right for two levels and
    // easy to get wrong for five, so this walks down and checks each one.
    let tree = draw(
        r#"
        view fn main(): Node {
          column(gap: 1) {
            row(gap: 1) {
              column(gap: 1) {
                row(gap: 1) {
                  text("deep")
                  text("deeper")
                }
                text("beside the row")
              }
            }
            text("last")
          }
        }
        "#,
    );

    assert_eq!(labels(&tree), vec!["deep", "deeper", "beside the row", "last"]);
    let level1 = children(&tree);
    assert_eq!(level1.len(), 2, "a row and a label");
    let level2 = children(&level1[0]);
    assert_eq!(level2.len(), 1);
    let level3 = children(&level2[0]);
    assert_eq!(level3.len(), 2, "a row and a label");
    assert_eq!(children(&level3[0]).len(), 2, "two labels at the bottom");
}

#[test]
fn many_siblings_survive_the_flat_array() {
    let mut source = String::from("view fn main(): Node {\n  column(gap: 1) {\n");
    for index in 0..200 {
        source.push_str(&format!("    text(\"row {index}\")\n"));
    }
    source.push_str("  }\n}\n");

    let tree = draw(&source);
    assert_eq!(children(&tree).len(), 200);
    assert_eq!(labels(&tree).first().map(String::as_str), Some("row 0"));
    assert_eq!(labels(&tree).last().map(String::as_str), Some("row 199"));
}

#[test]
fn a_view_can_use_the_string_functions() {
    let tree = draw(
        r#"
        view fn main(): Node {
          column(gap: 4) {
            text("count: " + str(len("abc")))
            text(trim("  spaced  "))
          }
        }
        "#,
    );
    assert_eq!(labels(&tree), vec!["count: 3", "spaced"]);
}
