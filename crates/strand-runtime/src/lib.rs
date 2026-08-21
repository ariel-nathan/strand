//! Strand actor runtime.
//!
//! An actor is a wasmtime `Store` (its memory arena), a tokio task (its
//! scheduling unit), and a mailbox (its only channel to the outside world).
//! Actors share no memory: the only way in is a `Message`.
//!
//! Two design commitments from `docs/inspiration-canon.md`:
//!
//! - **TigerBeetle.** Every scheduling-visible event is recorded on a `Trace`,
//!   and `sim` runs a scenario on a single thread with virtual time, so a run
//!   is reproducible. Determinism is designed in here rather than retrofitted
//!   once the scheduler exists.
//! - **§8.4.** That same causal log *is* the message-tracing debugger the
//!   design doc asks for. One mechanism, two uses.
//!
//! Error-type note: wasmtime 48 no longer uses `anyhow`, it has its own
//! `wasmtime::Error`. Host callbacks must hand back `wasmtime::Result`
//! (the only shape `WasmRet` is implemented for), while this crate's own
//! API speaks `anyhow::Result`. `?` bridges the two via wasmtime's
//! `anyhow` feature, which is on by default.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::sync::mpsc;
// tokio's clock, not std's: under `sim` it is virtual, so elapsed times in a
// trace are reproducible instead of wall-clock noise.
use tokio::time::Instant;
use wasmtime::error::Context as _;
use wasmtime::{Caller, Config, Engine, Extern, Instance, Linker, Module, Store};

pub mod sim;

pub type ActorId = u32;

/// Sender id for messages that originate outside any actor.
pub const HOST: ActorId = u32::MAX;

/// The typed message set. For M0/M1 this is deliberately tiny; §5.3's
/// buffer-transfer variants land in M2.
#[derive(Debug, Clone)]
pub enum Message {
    /// A payload produced by guest code, tagged with its sender.
    Blob { from: ActorId, bytes: Vec<u8> },
    /// Asks the actor to shut down cleanly after draining.
    Stop,
    /// §5.4: delivered to the parent when a child dies. Host-side supervisors
    /// act on it today; a Strand actor will receive it once M2's `actor`
    /// declarations land.
    ChildDown { child: ActorId, report: CrashReport },
}

/// One scheduling-visible event. Deliberately free of wall-clock time so two
/// runs of the same scenario compare equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Spawned { id: ActorId, name: String },
    Sent { from: ActorId, to: ActorId, len: usize },
    Delivered { to: ActorId, from: ActorId, len: usize },
    Logged { id: ActorId, text: String },
    Stopped { id: ActorId },
    /// Only the first line of the trap is kept, so traces stay comparable.
    Crashed { id: ActorId, reason: String },
    Restarted { id: ActorId, generation: u32 },
}

/// A causal log of what every actor did, in the order it happened.
#[derive(Clone, Default)]
pub struct Trace {
    events: Arc<Mutex<Vec<Event>>>,
}

impl Trace {
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    /// Renders the log the way §8.4's message tracing should read.
    pub fn render(&self) -> String {
        self.events()
            .iter()
            .map(|event| match event {
                Event::Spawned { id, name } => format!("spawn   {id} ({name})"),
                Event::Sent { from, to, len } => format!("send    {from} -> {to} ({len}B)"),
                Event::Delivered { to, from, len } => {
                    format!("deliver {from} -> {to} ({len}B)")
                }
                Event::Logged { id, text } => format!("log     {id}: {text}"),
                Event::Stopped { id } => format!("stop    {id}"),
                Event::Crashed { id, reason } => format!("CRASH   {id}: {reason}"),
                Event::Restarted { id, generation } => {
                    format!("restart {id} (generation {generation})")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Controls how the scheduler behaves. Production leaves `chaos` off; `sim`
/// turns it on to explore a different — but still reproducible — interleaving.
#[derive(Debug)]
pub struct SimConfig {
    /// When set, actors yield for a seeded pseudo-random spell before handling
    /// each message, shaking out order-dependent bugs.
    pub chaos: bool,
    state: AtomicU64,
}

impl SimConfig {
    pub fn new(seed: u64, chaos: bool) -> Self {
        // Any non-zero state; xorshift stalls at zero.
        Self { chaos, state: AtomicU64::new(seed | 1) }
    }

    /// xorshift64*. Small, deterministic, and good enough to perturb ordering.
    fn next(&self) -> u64 {
        let mut x = self.state.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state.store(x, Ordering::Relaxed);
        x
    }
}

impl Default for SimConfig {
    fn default() -> Self {
        Self::new(0, false)
    }
}

/// Shared address book. Actors hold senders, never each other's memory.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<HashMap<ActorId, mpsc::UnboundedSender<Message>>>>,
    start: Instant,
    trace: Trace,
    config: Arc<SimConfig>,
}

impl Registry {
    pub fn new() -> Self {
        Self::with_config(Arc::new(SimConfig::default()))
    }

    pub fn with_config(config: Arc<SimConfig>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            start: Instant::now(),
            trace: Trace::new(),
            config,
        }
    }

    pub fn trace(&self) -> Trace {
        self.trace.clone()
    }

    fn register(&self, id: ActorId, name: &str) -> mpsc::UnboundedReceiver<Message> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.lock().unwrap().insert(id, tx);
        self.trace.record(Event::Spawned { id, name: name.to_string() });
        rx
    }

    pub fn send(&self, to: ActorId, msg: Message) -> Result<()> {
        self.send_from(HOST, to, msg)
    }

    /// `from` is [`HOST`] for messages originating outside any actor.
    pub fn send_from(&self, from: ActorId, to: ActorId, msg: Message) -> Result<()> {
        let tx = {
            let map = self.inner.lock().unwrap();
            map.get(&to).cloned().ok_or_else(|| anyhow!("no actor {to}"))?
        };
        if let Message::Blob { bytes, .. } = &msg {
            self.trace.record(Event::Sent { from, to, len: bytes.len() });
        }
        tx.send(msg).map_err(|_| anyhow!("actor {to} mailbox closed"))
    }

    /// Milliseconds since runtime start — makes interleaving visible in logs.
    fn elapsed_ms(&self) -> u128 {
        self.start.elapsed().as_millis()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-actor host state. This is the `T` in `Store<T>`, so it is reachable
/// from every host call the guest makes and from nowhere else.
pub struct ActorCtx {
    pub id: ActorId,
    pub name: String,
    pub registry: Registry,
}

impl ActorCtx {
    fn log(&self, line: &str) {
        println!("[{:>5}ms] {:<6} {}", self.registry.elapsed_ms(), self.name, line);
        self.registry.trace.record(Event::Logged { id: self.id, text: line.to_string() });
    }
}

/// Reads a `(ptr, len)` pair out of the calling actor's linear memory.
fn read_guest_bytes(
    caller: &mut Caller<'_, ActorCtx>,
    ptr: i32,
    len: i32,
) -> wasmtime::Result<Vec<u8>> {
    let Some(Extern::Memory(mem)) = caller.get_export("memory") else {
        wasmtime::bail!("guest exports no memory");
    };
    let (data, _) = mem.data_and_store_mut(caller);
    let (start, len) = (ptr as usize, len as usize);
    match data.get(start..start + len) {
        Some(slice) => Ok(slice.to_vec()),
        None => wasmtime::bail!("guest pointer {ptr}+{len} out of bounds"),
    }
}

/// Installs the host ABI every Strand guest links against.
fn link_host_abi(linker: &mut Linker<ActorCtx>) -> Result<()> {
    linker.func_wrap(
        "strand",
        "log",
        |mut caller: Caller<'_, ActorCtx>, ptr: i32, len: i32| -> wasmtime::Result<()> {
            let bytes = read_guest_bytes(&mut caller, ptr, len)?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            caller.data().log(&text);
            Ok(())
        },
    )?;

    linker.func_wrap(
        "strand",
        "send",
        |mut caller: Caller<'_, ActorCtx>, to: i32, ptr: i32, len: i32| -> wasmtime::Result<()> {
            let bytes = read_guest_bytes(&mut caller, ptr, len)?;
            let ctx = caller.data();
            ctx.registry
                .send_from(ctx.id, to as ActorId, Message::Blob { from: ctx.id, bytes })
                .map_err(wasmtime::Error::from_anyhow)
        },
    )?;

    // The colorless-concurrency load-bearing call: suspends the *fiber*, not
    // the OS thread.
    linker.func_wrap_async(
        "strand",
        "sleep_ms",
        |_caller: Caller<'_, ActorCtx>, (ms,): (u64,)| {
            Box::new(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                wasmtime::Result::Ok(())
            })
        },
    )?;

    Ok(())
}

/// Builds the shared engine. Async is always on in wasmtime 48; epoch
/// interruption is what will preempt a hot actor in M2.
pub fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config.epoch_interruption(true);
    Ok(Engine::new(&config).context("failed to build wasmtime engine")?)
}

/// What a supervisor does when a child dies (§5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Rebuild the actor from scratch. Its arena is gone, so state is fresh.
    Restart,
    /// Let it stay dead and surface the failure to the caller.
    Stop,
}

/// A structured account of a death (§8.4) — not stack-trace soup.
///
/// Because actors take input only through their mailbox, the message being
/// handled is a complete description of what triggered the crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashReport {
    pub actor: ActorId,
    pub name: String,
    /// How many times this actor had already been restarted.
    pub generation: u32,
    pub reason: String,
    pub handling: Option<String>,
}

impl fmt::Display for CrashReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "actor `{}` ({}) died: {}", self.name, self.actor, self.reason)?;
        if let Some(handling) = &self.handling {
            write!(f, " while handling {handling}")?;
        }
        if self.generation > 0 {
            write!(f, " [generation {}]", self.generation)?;
        }
        Ok(())
    }
}

impl std::error::Error for CrashReport {}

/// Reduces a wasmtime error to one meaningful line, so a crash report says
/// something useful and a trace stays comparable across runs.
///
/// wasmtime leads with "error while executing at wasm backtrace:" and puts the
/// actual trap further down, so the first line is the least informative part.
fn reason(error: &wasmtime::Error) -> String {
    // Debug renders the causal chain; Display shows only the top line, which
    // for a trap is the uninformative half.
    first_line(&format!("{error:?}"))
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| line.starts_with("wasm trap:"))
        .map(str::to_string)
        .unwrap_or_else(|| text.lines().next().unwrap_or_default().trim().to_string())
}

/// Identifies a sender for humans. `HOST` is the sentinel for a message that
/// did not come from an actor.
fn sender(from: ActorId) -> String {
    if from == HOST {
        "the host".to_string()
    } else {
        format!("actor {from}")
    }
}

/// Renders a message for a crash report: a printable payload reads better than
/// a byte count when you are working out what killed an actor.
fn describe(bytes: &[u8], from: ActorId) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) if text.chars().all(|c| !c.is_control()) => {
            format!("{text:?} from {}", sender(from))
        }
        _ => format!("{} bytes from {}", bytes.len(), sender(from)),
    }
}

/// Runs one life of an actor: build the Store, run it, report why it ended.
///
/// Everything the actor owns is created here, so calling this again yields a
/// genuinely fresh arena — that is all "restart" means (§5.1).
async fn run_actor_once(
    engine: &Engine,
    registry: &Registry,
    id: ActorId,
    name: &str,
    wat: &str,
    generation: u32,
) -> Result<(), CrashReport> {
    let died = |reason: String, handling: Option<String>| CrashReport {
        actor: id,
        name: name.to_string(),
        generation,
        reason,
        handling,
    };

    let module = Module::new(engine, wat)
        .map_err(|e| died(format!("failed to compile: {}", reason(&e)), None))?;
    let mut linker = Linker::new(engine);
    link_host_abi(&mut linker).map_err(|e| died(format!("failed to link host ABI: {e}"), None))?;

    let ctx = ActorCtx { id, name: name.to_string(), registry: registry.clone() };
    let mut store = Store::new(engine, ctx);
    // Yield back to the scheduler on an epoch tick rather than trapping.
    store.set_epoch_deadline(1);
    store.epoch_deadline_async_yield_and_update(1);

    let mut mailbox = registry.register(id, name);
    let instance = linker.instantiate_async(&mut store, &module).await.map_err(|e| {
        died(format!("failed to instantiate: {}", reason(&e)), None)
    })?;

    let trace = registry.trace();
    let config = registry.config.clone();

    if let Ok(main) = instance.get_typed_func::<(), ()>(&mut store, "strand_main") {
        main.call_async(&mut store, ())
            .await
            .map_err(|e| died(reason(&e), Some("startup".to_string())))?;
    }

    let on_message = instance.get_typed_func::<(i32, i32), ()>(&mut store, "strand_on_message");
    while let Some(msg) = mailbox.recv().await {
        match msg {
            Message::Stop => break,
            // A host-side supervisor handles this today; guests ignore it.
            Message::ChildDown { .. } => continue,
            Message::Blob { from, bytes } => {
                if config.chaos {
                    // Seeded jitter: a different interleaving per seed, but the
                    // same one every time for a given seed.
                    let delay = config.next() % 8;
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                trace.record(Event::Delivered { to: id, from, len: bytes.len() });

                let Ok(handler) = &on_message else { continue };
                let summary = describe(&bytes, from);
                let ptr = write_into_guest(&mut store, &instance, &bytes, from)
                    .await
                    .map_err(|e| died(e.to_string(), Some(summary.clone())))?;
                handler
                    .call_async(&mut store, (ptr, bytes.len() as i32))
                    .await
                    .map_err(|e| died(reason(&e), Some(summary)))?;
            }
        }
    }

    trace.record(Event::Stopped { id });
    Ok(())
}

/// Spawns one unsupervised actor. A crash ends the task.
pub async fn spawn_actor(
    engine: &Engine,
    registry: &Registry,
    id: ActorId,
    name: &str,
    wat: &str,
) -> Result<tokio::task::JoinHandle<Result<(), CrashReport>>> {
    let engine = engine.clone();
    let registry = registry.clone();
    let (name, wat) = (name.to_string(), wat.to_string());
    Ok(tokio::spawn(
        async move { run_actor_once(&engine, &registry, id, &name, &wat, 0).await },
    ))
}

/// Spawns an actor under supervision (§5.4).
///
/// On a crash the arena is reclaimed by dropping the Store, a `ChildDown` goes
/// to the parent, and the policy decides whether a fresh instance takes its
/// place. Siblings are untouched throughout — that is the isolation claim.
pub fn spawn_supervised(
    engine: &Engine,
    registry: &Registry,
    id: ActorId,
    name: &str,
    wat: &str,
    policy: Policy,
    parent: Option<ActorId>,
) -> tokio::task::JoinHandle<Result<(), CrashReport>> {
    let engine = engine.clone();
    let registry = registry.clone();
    let (name, wat) = (name.to_string(), wat.to_string());

    tokio::spawn(async move {
        let mut generation = 0;
        loop {
            match run_actor_once(&engine, &registry, id, &name, &wat, generation).await {
                Ok(()) => return Ok(()),
                Err(report) => {
                    registry.trace.record(Event::Crashed { id, reason: report.reason.clone() });
                    // §8.4: the supervisor is a crash reporter by construction.
                    eprintln!("!! {report}");

                    if let Some(parent) = parent {
                        let _ = registry.send_from(
                            id,
                            parent,
                            Message::ChildDown { child: id, report: report.clone() },
                        );
                    }

                    if policy == Policy::Stop {
                        return Err(report);
                    }
                    generation += 1;
                    registry.trace.record(Event::Restarted { id, generation });
                }
            }
        }
    })
}

/// Copies an inbound message into the guest's arena via its own allocator.
async fn write_into_guest(
    store: &mut Store<ActorCtx>,
    instance: &Instance,
    bytes: &[u8],
    _from: ActorId,
) -> Result<i32> {
    let alloc = instance
        .get_typed_func::<i32, i32>(&mut *store, "strand_alloc")
        .context("guest exports no strand_alloc")?;
    let ptr = alloc.call_async(&mut *store, bytes.len() as i32).await?;

    let Some(Extern::Memory(mem)) = instance.get_export(&mut *store, "memory") else {
        return Err(anyhow!("guest exports no memory"));
    };
    mem.write(store, ptr as usize, bytes)?;
    Ok(ptr)
}
