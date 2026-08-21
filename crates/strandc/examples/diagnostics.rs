//! Renders the diagnostics §8.2 asks for, so their quality is inspectable.
//!
//!     cargo run -p strandc --example diagnostics

const BAD: &str = r#"type AddError = | EmptyTitle | TooLong(max: int)

fn addTodo(title: string, len: int): Result<int, AddError> {
  let limit = 200
  limit = len
  if len > limit { return Err(TooLong(max: limit)) }
  Ok(len + "one")
}

fn describe(e: AddError): int {
  match e {
    EmptyTitle => 1,
  }
}
"#;

fn main() {
    match strandc::compile("todo.str", BAD) {
        Ok(_) => println!("compiled cleanly (unexpected for this example)"),
        Err(report) => {
            println!("{:?}", miette::Report::new(report));
        }
    }
}
