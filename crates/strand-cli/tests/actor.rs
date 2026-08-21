//! An actor declared in Strand, compiled, and hosted by the real runtime.
//!
//! Two halves: the semantics (does `receive` fold state correctly) checked by
//! driving the module directly, and the integration (does the runtime host it
//! like any other actor) checked through `spawn_supervised`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use strand_runtime::sim::{self, SimOptions};
use strand_runtime::{engine, spawn_supervised, Event, Message, Policy, Registry, HOST};
use strandc::hir::Hir;
use wasmtime::{Engine, Instance, Store, Val};

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("examples").join("strand").join(name)
}

fn compile(name: &str) -> (Hir, Vec<u8>) {
    let path = example(name);
    let source = std::fs::read_to_string(&path).expect("reading example");
    let hir = match strandc::compile(&path.display().to_string(), &source) {
        Ok(hir) => hir,
        Err(report) => panic!("{:?}", miette::Report::new(report)),
    };
    let wasm = strandc::codegen::emit(&hir).expect("emit failed");
    wasmparser::validate(&wasm).expect("emitted invalid WASM");
    (hir, wasm)
}

/// Drives the actor's exported ABI by hand and reads back its state record.
struct Driver {
    store: Store<()>,
    instance: Instance,
}

impl Driver {
    fn new(wasm: &[u8]) -> Self {
        let engine = Engine::default();
        let module = wasmtime::Module::new(&engine, wasm).expect("wasmtime rejected the module");
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[]).expect("instantiation failed");

        let main = instance
            .get_typed_func::<(), ()>(&mut store, "strand_main")
            .expect("no strand_main export");
        main.call(&mut store, ()).expect("init trapped");
        Self { store, instance }
    }

    /// Writes a message into the guest arena the way the runtime does, then
    /// invokes the handler.
    fn send(&mut self, text: &str) {
        let alloc = self
            .instance
            .get_typed_func::<i32, i32>(&mut self.store, "strand_alloc")
            .expect("no strand_alloc export");
        let ptr = alloc.call(&mut self.store, text.len() as i32).expect("alloc trapped");
        let memory = self.instance.get_memory(&mut self.store, "memory").expect("no memory");
        memory.write(&mut self.store, ptr as usize, text.as_bytes()).expect("write failed");

        let on_message = self
            .instance
            .get_typed_func::<(i32, i32), ()>(&mut self.store, "strand_on_message")
            .expect("no strand_on_message export");
        on_message.call(&mut self.store, (ptr, text.len() as i32)).expect("receive trapped");
    }

    /// Reads `{ total, seen }` out of the exported state pointer.
    fn state(&mut self) -> (i64, i64) {
        let global = self
            .instance
            .get_global(&mut self.store, "strand_state")
            .expect("no strand_state export");
        let Val::I32(ptr) = global.get(&mut self.store) else {
            panic!("state should be a pointer");
        };
        let memory = self.instance.get_memory(&mut self.store, "memory").expect("no memory");
        let data = memory.data(&self.store);
        let at = |offset: usize| {
            i64::from_le_bytes(data[ptr as usize + offset..ptr as usize + offset + 8].try_into().unwrap())
        };
        // Fields occupy one 8-byte slot each, in declaration order.
        (at(0), at(8))
    }
}

#[test]
fn init_produces_the_starting_state() {
    let (_, wasm) = compile("counter.str");
    let mut driver = Driver::new(&wasm);
    assert_eq!(driver.state(), (0, 0));
}

#[test]
fn receive_folds_the_state_across_messages() {
    let (_, wasm) = compile("counter.str");
    let mut driver = Driver::new(&wasm);

    driver.send("inc");
    driver.send("inc");
    driver.send("inc");
    assert_eq!(driver.state(), (3, 3), "three increments, three messages seen");

    driver.send("dec");
    assert_eq!(driver.state(), (2, 4));

    driver.send("nonsense");
    assert_eq!(driver.state(), (2, 5), "an unknown message still counts as seen");

    driver.send("reset");
    assert_eq!(driver.state(), (0, 6));
}

#[test]
fn the_module_declares_its_actor() {
    let (hir, _) = compile("counter.str");
    let actor = hir.actor.expect("counter.str declares an actor");
    assert_eq!(actor.name, "Counter");
}

#[test]
fn the_runtime_hosts_a_strand_actor() {
    // The integration that matters: a compiled Strand actor is spawned,
    // supervised, and fed messages exactly like a hand-written .wat fixture.
    let (_, wasm) = compile("counter.str");

    let trace = sim::run(SimOptions::new(1), move |registry: Registry| async move {
        let engine = engine()?;
        let handle =
            spawn_supervised(&engine, &registry, 0, "counter", &wasm, Policy::Restart, None);

        tokio::time::sleep(Duration::from_millis(20)).await;
        for text in ["inc", "inc", "reset"] {
            let _ =
                registry.send(0, Message::Blob { from: HOST, bytes: text.as_bytes().to_vec() });
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let _ = registry.send(0, Message::Stop);
        let _ = handle.await;
        Ok(())
    })
    .expect("simulation failed");

    let events = trace.events();
    assert!(
        events.contains(&Event::Spawned { id: 0, name: "counter".to_string() }),
        "the Strand actor should spawn:\n{}",
        trace.render()
    );
    let delivered = events
        .iter()
        .filter(|e| matches!(e, Event::Delivered { to: 0, .. }))
        .count();
    assert_eq!(delivered, 3, "all three messages should reach it:\n{}", trace.render());
    assert!(
        !events.iter().any(|e| matches!(e, Event::Crashed { .. })),
        "a well-behaved actor should not crash:\n{}",
        trace.render()
    );
    assert!(
        events.contains(&Event::Stopped { id: 0 }),
        "it should shut down cleanly:\n{}",
        trace.render()
    );
}
