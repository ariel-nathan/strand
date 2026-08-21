//! The typed state snapshot (§9.3), end to end and without a window.
//!
//! What these hold to account is the claim hot reload rests on: that an
//! actor's state can be read out of one arena and rebuilt in another, with the
//! new arena at a different address, by a module compiled from different
//! source — and that the view drawn afterwards is the one the old state
//! deserves. If any offset in `strandc::layout` disagreed with what codegen
//! wrote, a restored todo would come back with the wrong title, the wrong
//! flag, or not at all.
//!
//! The refusal matters as much as the restore. §9.3 says the runtime can check
//! statically that two state shapes match before a swap, and that Erlang's hot
//! code load cannot. The last two tests are that check.

use anyhow::Result;
use strand_cli::snapshot::{difference, shape, Codec};
use strand_cli::view::View;
use strand_render::inspect::describe_with_fonts;
use strand_render::widgets::Theme;
use strandc::hir::{Hir, Ty};

const VIEWPORT: (f32, f32) = (600.0, 480.0);

fn example(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("strand")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

fn compile(source: &str) -> Hir {
    match strandc::compile("t.str", source) {
        Ok(hir) => hir,
        Err(report) => panic!("{:?}", miette::Report::new(report)),
    }
}

/// The actor that draws, and everything about it the tests need.
struct Ui {
    hir: Hir,
    wasm: Vec<u8>,
    state: Ty,
    input_port: u32,
    input_ty: Ty,
}

fn ui(source: &str) -> Ui {
    let hir = compile(source);
    let index = hir
        .actors
        .iter()
        .position(|actor| actor.view.is_some())
        .expect("this file has an actor that draws");
    let actor = &hir.actors[index];
    // By type, not by name (§6.11).
    let input_port = actor
        .inbox
        .iter()
        .position(|port| matches!(&port.ty, Ty::Sum(id) if hir.sums[id.0 as usize].name == "Input"))
        .expect("it hears input") as u32;
    Ui {
        wasm: strandc::codegen::emit_actor(&hir, index).expect("it emits"),
        state: actor.state.clone(),
        input_ty: actor.inbox[input_port as usize].ty.clone(),
        input_port,
        hir,
    }
}

impl Ui {
    fn codec(&self) -> Codec {
        Codec::new(&self.hir, &self.state)
    }

    fn shape(&self) -> String {
        shape(&self.hir, &self.state).to_string()
    }

    /// A fresh instance, with the state its own `init` built.
    fn started(&self) -> Result<View> {
        View::new(&self.hir, &self.wasm)
    }

    fn send(&self, view: &mut View, spec: &str) {
        let bytes = strand_cli::encode::encode(&self.hir, &self.input_ty, spec)
            .unwrap_or_else(|e| panic!("encoding {spec}: {e:#}"));
        view.deliver(self.input_port, &bytes).unwrap_or_else(|e| panic!("{spec}: {e:#}"));
    }

    /// Types a title and adds it, the way a person would.
    fn add(&self, view: &mut View, title: &str) {
        for character in title.chars() {
            self.send(view, &format!("Typed {}", character as u32));
        }
        self.send(view, "Enter");
    }
}

fn drawn(view: &mut View) -> String {
    let tree = view.frame(&Theme::default()).expect("a frame");
    describe_with_fonts(&tree, VIEWPORT)
}

/// The demo app: a list of records holding strings and flags, a draft string,
/// a float scroll offset. Everything §6.3 to §6.6 describes, in one record.
fn demo() -> Ui {
    ui(&example("todo_demo.str"))
}

/// Every representation the layout rules describe, in one state record: a
/// string, an int, a float, a bool, a bare-tag sum, a boxed sum with a record
/// and a float in it, an `Option`, a `Result` whose error is a record, and a
/// list of records.
///
/// No example file is written this way, because no application would be. The
/// walker has to be right about all of it anyway, and a fixture that exercises
/// every branch beats nine that each exercise one.
const SHAPES: &str = r#"// Every representation §6.2 to §6.6 describes, in one state record.

type Tint = | Red | Green

type Note = { text: string, weight: int }

type Slot =
  | Filled(note: Note, at: float)
  | Blank

type Model = {
  label: string,
  count: int,
  ratio: float,
  on: bool,
  tint: Tint,
  slot: Slot,
  maybe: Option<string>,
  outcome: Result<int, Note>,
  notes: List<Note>,
  // Two words per element (§6.6), so its stride is not its length.
  marks: List<Option<int>>,
}

fn tintName(t: Tint): string {
  match t {
    Red => "red",
    Green => "green",
  }
}

fn slotName(s: Slot): string {
  match s {
    Filled(note, at) => note.text + "/" + str(note.weight),
    Blank => "-",
  }
}

fn maybeName(m: Option<string>): string {
  match m {
    Some(text) => text,
    None => "-",
  }
}

fn markName(m: Option<int>): string {
  match m {
    Some(n) => str(n),
    None => "-",
  }
}

fn outcomeName(r: Result<int, Note>): string {
  match r {
    Ok(n) => str(n),
    Err(note) => note.text,
  }
}

actor Shapes {
  state: Model
  in input: Input

  fn init(): Model {
    Model {
      label: "",
      count: 0,
      ratio: 0.0,
      on: false,
      tint: Red,
      slot: Blank,
      maybe: None,
      outcome: Ok(0),
      notes: [],
      marks: [],
    }
  }

  on input(state: Model, msg: Input): Model {
    match msg {
      Typed(ch) => Model {
        ...state,
        label: state.label + char(ch),
        count: state.count + 1,
        notes: push(state.notes, Note { text: state.label + char(ch), weight: state.count }),
        marks: push(push(state.marks, Some(state.count)), None),
      },
      Enter => Model {
        ...state,
        ratio: 0.25,
        on: !state.on,
        tint: Green,
        slot: Filled(note: Note { text: state.label, weight: 3 }, at: 1.5),
        maybe: Some(state.label),
        outcome: Err(Note { text: "refused", weight: 7 }),
      },
      Escape => Model { ...state, maybe: None, outcome: Ok(9), slot: Blank },
      _ => state,
    }
  }

  view fn draw(state: Model): Node {
    screen(gap: 4, padding: 8) {
      text("label " + state.label)
      text("count " + str(state.count))
      text("tint " + tintName(state.tint))
      text("slot " + slotName(state.slot))
      text("maybe " + maybeName(state.maybe))
      text("outcome " + outcomeName(state.outcome))
      text("notes " + str(len(state.notes)))
      for note in state.notes {
        text(note.text + " " + str(note.weight))
      }
      for mark in state.marks {
        text("mark " + markName(mark))
      }
    }
  }
}
"#;

/// One sum, one narrow variant and one wide one. §6.3 says a value's size is
/// a property of its type rather than of its tag, and nothing else in these
/// fixtures can tell the difference.
const LEVELS: &str = r#"type Level = | Low | High(n: int, ratio: float)

type Model = { level: Level }

fn levelName(l: Level): string {
  match l {
    Low => "low",
    High(n, ratio) => "high " + str(n),
  }
}

actor Levels {
  state: Model
  in input: Input

  fn init(): Model { Model { level: Low } }

  on input(state: Model, msg: Input): Model {
    match msg {
      Enter => Model { level: High(n: 1, ratio: 0.5) },
      Escape => Model { level: Low },
      _ => state,
    }
  }

  view fn draw(state: Model): Node {
    screen(gap: 4, padding: 8) { text(levelName(state.level)) }
  }
}
"#;

#[test]
fn a_narrow_variant_takes_the_same_room_as_a_wide_one() {
    // §6.3's rule, which is what lets a length be a constant of the type — for
    // `send` on the wire, and for a snapshot in an image. A narrow variant
    // written at its own width would also be the §6.3 hazard: read back at the
    // widest width at the end of an arena, it runs off the end of memory.
    let app = ui(LEVELS);
    let mut low = app.started().expect("an instance");
    let narrow = low.snapshot(&app.codec()).expect("a snapshot");

    let mut high = app.started().expect("another instance");
    app.send(&mut high, "Enter");
    let wide = high.snapshot(&app.codec()).expect("a snapshot");

    assert_eq!(
        narrow.bytes.len(),
        wide.bytes.len(),
        "`Low` and `High` are the same size, because both are a `Level`"
    );
    assert_ne!(narrow, wide, "the same size, not the same value");
}

#[test]
fn a_narrow_variant_restores_as_itself() {
    let app = ui(LEVELS);
    let mut high = app.started().expect("an instance");
    app.send(&mut high, "Enter");
    let wide = high.snapshot(&app.codec()).expect("a snapshot");

    let mut low = app.started().expect("another instance");
    assert_eq!(drawn(&mut low), drawn(&mut app.started().expect("a third")));
    low.restore(&wide).expect("the shapes agree");
    assert!(drawn(&mut low).contains("high 1"));
}

#[test]
fn every_representation_survives_the_round_trip() {
    let app = ui(SHAPES);
    let mut first = app.started().expect("an instance");
    for spec in ["Typed 104", "Typed 105", "Enter"] {
        app.send(&mut first, spec);
    }
    let before = drawn(&mut first);
    let snapshot = first.snapshot(&app.codec()).expect("a snapshot");

    let mut second = app.started().expect("a second instance");
    // Different traffic, so the arena is a different size and the image
    // cannot land where it started.
    app.send(&mut second, "Typed 122");
    second.restore(&snapshot).expect("the shapes agree");

    assert_eq!(drawn(&mut second), before, "the restored state draws the same tree");

    // Stronger than the tree: a snapshot is canonical — offsets from its own
    // start — so two equal states have equal images, including the fields no
    // view draws. `at: float` inside the boxed variant is only checked here.
    let again = second.snapshot(&app.codec()).expect("a second snapshot");
    assert_eq!(again, snapshot, "capture, restore and capture again is the identity");
}

#[test]
fn a_none_and_an_ok_carry_no_pointer() {
    // The payload slot of an `Option`/`Result` is a pointer only for some
    // tags. Reading `None`'s zero as an address is the bug this shape exists
    // to catch, and it would trap on the arena's first page rather than come
    // back wrong.
    let app = ui(SHAPES);
    let mut running = app.started().expect("an instance");
    app.send(&mut running, "Typed 104");
    app.send(&mut running, "Escape"); // maybe: None, outcome: Ok(9)
    let snapshot = running.snapshot(&app.codec()).expect("a snapshot");
    let before = drawn(&mut running);

    let mut replacement = app.started().expect("another instance");
    replacement.restore(&snapshot).expect("the shapes agree");
    assert_eq!(drawn(&mut replacement), before);
}

#[test]
fn a_snapshot_restored_into_another_instance_draws_the_same_tree() {
    let app = demo();
    let mut first = app.started().expect("an instance");
    app.add(&mut first, "carry me across");
    app.send(&mut first, "Click 1001"); // toggle the first todo
    let before = drawn(&mut first);

    let snapshot = first.snapshot(&app.codec()).expect("a snapshot");

    let mut second = app.started().expect("a second instance");
    assert_ne!(drawn(&mut second), before, "a fresh actor starts from `init`");

    second.restore(&snapshot).expect("the state fits");
    assert_eq!(
        drawn(&mut second),
        before,
        "every string, flag, float and list element survived the move"
    );
}

#[test]
fn the_new_arena_is_at_a_different_address() {
    // The restore is not a memcpy of one memory into another: the second
    // instance has already run its own `init`, so the image lands past what
    // that allocated and every pointer in it has to move. If relocation were
    // wrong this is the test that would catch it.
    let app = demo();
    let mut first = app.started().expect("an instance");
    app.add(&mut first, "moved");
    let snapshot = first.snapshot(&app.codec()).expect("a snapshot");
    let before = drawn(&mut first);

    let mut second = app.started().expect("a second instance");
    // Give the second arena a different amount of traffic, so the bump
    // pointer cannot happen to line up.
    app.add(&mut second, "something else entirely");
    app.add(&mut second, "and another");
    second.restore(&snapshot).expect("the state fits");

    assert_eq!(drawn(&mut second), before);
}

#[test]
fn newer_code_draws_the_older_state() {
    // Tier 2 in one assertion: the behaviour changed, the record did not, and
    // what the user had typed is still there afterwards.
    let app = demo();
    let edited = ui(&example("todo_demo.str").replace("\"crash stats\"", "\"break stats\""));
    assert_eq!(app.shape(), edited.shape(), "the state record was not touched");

    let mut running = app.started().expect("an instance");
    app.add(&mut running, "survive the reload");
    let snapshot = running.snapshot(&app.codec()).expect("a snapshot");

    let mut replacement = edited.started().expect("the new module");
    replacement.restore(&snapshot).expect("the shapes agree");
    let after = drawn(&mut replacement);

    assert!(after.contains("survive the reload"), "the state came across:\n{after}");
    assert!(after.contains("break stats"), "the new code is what draws it:\n{after}");
}

#[test]
fn an_edited_state_record_refuses_the_snapshot_and_says_what_changed() {
    // The interesting case, not the safety net. The old bytes would still
    // *fit* — the new field is at the end — and reading them as the new type
    // would leave `pinned` holding whatever the arena had there. Refusing is
    // the whole of §9.3's argument.
    let app = demo();
    let edited = ui(&example("todo_demo.str")
        .replace("  burning: bool,\n}", "  burning: bool,\n  pinned: int,\n}")
        .replace("      burning: false,\n    }", "      burning: false,\n      pinned: 0,\n    }"));

    let mut running = app.started().expect("an instance");
    app.add(&mut running, "not going anywhere");
    let snapshot = running.snapshot(&app.codec()).expect("a snapshot");

    assert!(!snapshot.fits(&edited.shape()), "a new field is a new shape");
    let changed = difference(&shape(&app.hir, &app.state), &shape(&edited.hir, &edited.state))
        .expect("they differ");
    assert_eq!(changed, "`Model` gained the field `pinned: int`");
}

#[test]
fn a_state_that_only_changed_inside_a_list_element_is_still_refused() {
    let app = demo();
    let edited = ui(&example("todo_demo.str")
        .replace("type Todo = { id: int, title: string, done: bool }", "type Todo = { id: int, title: string, done: bool, at: float }")
        .replace("done: true }", "done: true, at: 0.0 }")
        .replace("done: false }", "done: false, at: 0.0 }")
        .replace("Todo { id: state.nextId, title: clean, done: false }", "Todo { id: state.nextId, title: clean, done: false, at: 0.0 }"));

    let mut running = app.started().expect("an instance");
    let snapshot = running.snapshot(&app.codec()).expect("a snapshot");

    assert!(!snapshot.fits(&edited.shape()));
    let changed = difference(&shape(&app.hir, &app.state), &shape(&edited.hir, &edited.state))
        .expect("they differ");
    assert_eq!(changed, "`Model.todos's elements` gained the field `at: float`");
}

#[test]
fn a_snapshot_is_the_state_and_not_the_arena() {
    // Proof that this is a walk of the reachable graph rather than a copy of
    // linear memory: the demo's arena is tens of kilobytes — the frame array
    // alone is 64 — and its state is a few hundred bytes.
    let app = demo();
    let mut running = app.started().expect("an instance");
    app.add(&mut running, "one todo");
    let snapshot = running.snapshot(&app.codec()).expect("a snapshot");
    assert!(
        snapshot.bytes.len() < 1024,
        "the image is {} bytes, which is an arena and not a state",
        snapshot.bytes.len()
    );
    assert!(!snapshot.relocations.is_empty(), "a Model is nothing but pointers");
}
