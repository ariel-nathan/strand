//! Hot reload: recompile on a change, swap the code, keep the state (§9.3).
//!
//! The loop is the whole feature. Read the file, compile it, and send each
//! running actor its new module; the supervisor ends the current life and
//! starts the replacement with the state the old one was holding. §9.3 calls
//! Tier 1 "view reload" and Tier 2 "actor logic reload", but there is one
//! mechanism here, because there was never a way to change only the view: new
//! code means a new module, a new `Store` and a new arena, and the record the
//! running actor holds is a pointer into the memory that is about to be
//! dropped. The snapshot is what makes either tier work at all.
//!
//! Two things are deliberately *not* here:
//!
//! - **No file-watching dependency.** One file, checked twice a second for a
//!   new modified time. A notify-style watcher is a thread, a platform backend
//!   and a debounce timer for a question this answers in six lines.
//! - **No decision about whether the state fits.** This says what changed,
//!   because it is the side that still has both types; the runtime compares
//!   the two shapes and acts. A diagnosis and a decision are different jobs
//!   (§9.2), and putting the decision here would mean the runtime trusted a
//!   caller about the one thing it must not.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use strand_runtime::{Message, Registry};
use strandc::hir::Hir;

use crate::plan::{self, Plan};
use crate::snapshot::{difference, shape, Codec};

/// How often the file is looked at. Fast enough that a save feels immediate,
/// slow enough to cost nothing.
const POLL: Duration = Duration::from_millis(200);

/// What the file looked like last time. Size as well as time, because an
/// editor that writes twice within a clock tick still changes the length.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Watches `path` and reloads the app running under `registry`.
///
/// `hir` is what the running actors were compiled from. It moves forward only
/// when a swap actually happens, so a run of broken saves is measured against
/// the code that is really running rather than against the last thing typed.
pub async fn watch(path: PathBuf, hir: Hir, registry: Registry) {
    let mut running = match plan::plan(&hir) {
        Ok(plan) => Baseline { hir, wiring: String::new(), plan },
        // The app is already running, so this cannot happen; if it somehow
        // does, watching is the part to give up on.
        Err(error) => {
            eprintln!("!! not watching for changes: {error:#}");
            return;
        }
    };
    running.wiring = wiring(&running.hir, &running.plan);

    let mut seen = stamp(&path);
    println!("watching {} — save the file to reload it (§9.3)", path.display());

    loop {
        tokio::time::sleep(POLL).await;
        let now = stamp(&path);
        if now == seen || now.is_none() {
            continue;
        }
        seen = now;

        match reload(&path, &running, &registry) {
            Ok(Some(next)) => running = next,
            // Compiled, but the app is not the same app.
            Ok(None) => {}
            Err(error) => eprintln!("!! {error:#}"),
        }
    }
}

/// The app as it is actually running.
struct Baseline {
    hir: Hir,
    plan: Plan,
    wiring: String,
}

/// Compiles the file again and swaps every actor over to it.
///
/// `Ok(None)` means the file compiled but describes a different app, so
/// nothing was swapped — the actors keep running the code they have.
fn reload(path: &Path, running: &Baseline, registry: &Registry) -> Result<Option<Baseline>> {
    let source = std::fs::read_to_string(path)?;
    let hir = match strandc::compile(&path.display().to_string(), &source) {
        Ok(hir) => hir,
        Err(report) => {
            // The full miette rendering, exactly as `strand run` shows it: a
            // reload that fails to compile is a normal part of editing, and
            // the diagnostic is the product surface (§9.2).
            eprintln!("{:?}", miette::Report::new(report));
            eprintln!("!! kept the running code — fix the error and save again");
            return Ok(None);
        }
    };

    let plan = plan::plan(&hir)?;
    if let Some(reason) = changed_shape(running, &hir, &plan) {
        eprintln!(
            "!! {reason} — that is the supervision tree itself, not the code in it, \
             so it needs a fresh `strand view`"
        );
        return Ok(None);
    }

    for (index, spawn) in plan.spawns.iter().enumerate() {
        let was = &running.hir.actors[running.plan.spawns[index].actor];
        let now = &hir.actors[spawn.actor];
        let before = shape(&running.hir, &was.state);
        let after = shape(&hir, &now.state);

        match difference(&before, &after) {
            None => println!("reload  {} — state carried ({})", spawn.name, after.name()),
            Some(change) => {
                println!("reload  {} — starting fresh: {change}", spawn.name)
            }
        }
        registry.send(
            spawn.id,
            Message::Reload {
                bytes: spawn.wasm.clone(),
                state: Arc::new(Codec::new(&hir, &now.state)),
            },
        )?;
    }

    Ok(Some(Baseline { wiring: wiring(&hir, &plan), hir, plan }))
}

/// Whether the file still describes the app that is running, and if not, what
/// about it moved.
///
/// Code may change freely; the tree may not. Every actor's mailbox, wiring and
/// frame route was installed once, before anything ran (§6.13's ordering
/// rules) — swapping a module cannot re-run that, so an app whose wiring
/// changed is a different app wearing the same name.
fn changed_shape(running: &Baseline, hir: &Hir, plan: &Plan) -> Option<String> {
    if plan.spawns.len() != running.plan.spawns.len() {
        return Some(format!(
            "this file now runs {} actors where it ran {}",
            plan.spawns.len(),
            running.plan.spawns.len()
        ));
    }
    if plan.ui != running.plan.ui || plan.input_port != running.plan.input_port {
        return Some("a different actor draws the window now".to_string());
    }
    let next = wiring(hir, plan);
    if next != running.wiring {
        return Some("the actors are wired differently now".to_string());
    }
    None
}

/// Everything about an app except the code: who runs, what each of them can be
/// told and can say, and which port meets which.
fn wiring(hir: &Hir, plan: &Plan) -> String {
    let mut out = String::new();
    for spawn in &plan.spawns {
        let actor = &hir.actors[spawn.actor];
        out.push_str(&format!("{}={} ", spawn.name, actor.name));
        for (kind, ports) in [("in", &actor.inbox), ("out", &actor.outbox)] {
            for port in ports {
                out.push_str(&format!("{kind} {}:{} ", port.name, shape(hir, &port.ty)));
            }
        }
        for wire in &spawn.out {
            match wire {
                Some(wiring) => out.push_str(&format!("-> {}.{} ", wiring.to, wiring.port)),
                None => out.push_str("-> nowhere "),
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled(source: &str) -> (Hir, Plan) {
        let hir = match strandc::compile("t.str", source) {
            Ok(hir) => hir,
            Err(report) => panic!("{:?}", miette::Report::new(report)),
        };
        let plan = plan::plan(&hir).expect("a plan");
        (hir, plan)
    }

    fn baseline(source: &str) -> Baseline {
        let (hir, plan) = compiled(source);
        Baseline { wiring: wiring(&hir, &plan), hir, plan }
    }

    const APP: &str = "\
type Count = { total: int }
type Bump = | Tick

actor Source {
  state: Count
  in  input: Input
  out bumps: Bump
  fn init(): Count { Count { total: 0 } }
  on input(state: Count, msg: Input): Count {
    send(bumps, Tick)
    state
  }
  view fn draw(state: Count): Node { text(\"hi\") }
}

actor Sink {
  state: Count
  in bumps: Bump
  fn init(): Count { Count { total: 0 } }
  on bumps(state: Count, msg: Bump): Count { Count { total: state.total + 1 } }
}

app Pair {
  source = Source
  sink = Sink
  source.bumps -> sink.bumps
}
";

    fn refusal(edited: &str) -> Option<String> {
        let running = baseline(APP);
        let (hir, plan) = compiled(edited);
        changed_shape(&running, &hir, &plan)
    }

    #[test]
    fn a_body_change_is_not_a_shape_change() {
        // The case that has to work, or hot reload does nothing.
        let edited = APP.replace("state.total + 1", "state.total + 2");
        assert_eq!(refusal(&edited), None);
    }

    #[test]
    fn a_state_record_change_is_still_a_reload() {
        // A different state shape is not a different *app*: the actors and
        // wires are the same, so the swap happens and the state does not
        // travel. Refusing here would be refusing the case §9.3 Tier 3 is
        // about, which is a fresh `init` rather than a stop.
        let edited = APP
            .replace("type Count = { total: int }", "type Count = { total: int, peak: int }")
            .replace("Count { total: 0 }", "Count { total: 0, peak: 0 }")
            .replace("Count { total: state.total + 1 }", "Count { total: state.total + 1, peak: 0 }");
        assert_eq!(refusal(&edited), None);
    }

    #[test]
    fn an_actor_appearing_is_refused() {
        let edited = APP.replace(
            "app Pair {\n  source = Source\n  sink = Sink",
            "app Pair {\n  source = Source\n  sink = Sink\n  spare = Sink",
        );
        assert_eq!(refusal(&edited).as_deref(), Some("this file now runs 3 actors where it ran 2"));
    }

    #[test]
    fn a_rewire_is_refused() {
        // The wires were turned into a routing table before anything ran, and
        // swapping a module does not touch it. Two instances of `Sink` so that
        // only the destination changes: same actors, same ports, other end.
        let three = APP.replace("  sink = Sink", "  sink = Sink
  spare = Sink");
        let edited = three.replace("source.bumps -> sink.bumps", "source.bumps -> spare.bumps");

        let running = baseline(&three);
        let (hir, plan) = compiled(&edited);
        assert_eq!(
            changed_shape(&running, &hir, &plan).as_deref(),
            Some("the actors are wired differently now")
        );
    }

    #[test]
    fn a_new_port_is_refused() {
        let edited = APP
            .replace("  in bumps: Bump\n", "  in bumps: Bump\n  in extra: int\n")
            .replace(
                "  on bumps(state: Count, msg: Bump): Count { Count { total: state.total + 1 } }",
                "  on bumps(state: Count, msg: Bump): Count { Count { total: state.total + 1 } }\n  on extra(state: Count, msg: int): Count { state }",
            );
        assert_eq!(refusal(&edited).as_deref(), Some("the actors are wired differently now"));
    }
}
