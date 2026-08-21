//! Strand actor runtime.
//!
//! An actor is a wasmtime `Store` (its memory arena), a tokio task (its
//! scheduling unit), and a mailbox (its only channel to the outside world).
//! Actors share no memory: the only way in is a `Message`.
//!
//! Two design commitments from §17:
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

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
pub mod snapshot;

pub use snapshot::{Root, Snapshot, Snapshots};

pub type ActorId = u32;

/// Sender id for messages that originate outside any actor.
pub const HOST: ActorId = u32::MAX;

/// The typed message set. For M0/M1 this is deliberately tiny; §5.3's
/// buffer-transfer variants land in M2.
#[derive(Debug, Clone)]
pub enum Message {
    /// A payload produced by guest code, tagged with its sender and with the
    /// receiver's port — which channel of theirs it arrived on.
    ///
    /// The port is resolved before the message is sent, by the wiring the
    /// `app` block declared: the sender named a port of its own, and the
    /// address book turned that into a destination and a port there. Neither
    /// actor ever holds the other's identity.
    Blob { from: ActorId, port: u32, bytes: Vec<u8> },
    /// Asks the actor to shut down cleanly after draining.
    Stop,
    /// Newer code for this actor (§9.3), and how to read the state that goes
    /// with it. The current life ends, the replacement starts on these bytes,
    /// and the state travels across when the two shapes agree.
    ///
    /// A supervisor restart with a newer module is all hot reload is. The only
    /// thing this adds to §5.4 is which bytes to come back on.
    ///
    /// The reader travels *with* the bytes rather than being installed
    /// separately, because the two belong to each other: installing it early
    /// would read the old arena as the new type, and installing it late would
    /// read the new arena as the old one. Arriving together, it cannot be
    /// either.
    Reload { bytes: Vec<u8>, state: Arc<dyn Snapshots> },
    /// Start over on the code already running (§5.4's first strategy, asked
    /// for rather than provoked by a crash).
    ///
    /// The other half of `Reload`. A reload replaces the code and keeps the
    /// data, which leaves a string a handler once put in the state showing its
    /// old text until that handler runs again. This throws the state away, and
    /// is how a person sees an edit to such a string.
    Restart,
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
    /// A life that ended because newer code arrived. `carried` says whether
    /// the state came with it or the replacement started from `init`.
    Reloaded { id: ActorId, generation: u32, carried: bool },
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
                Event::Reloaded { id, generation, carried } => {
                    let state = if *carried { "state carried" } else { "fresh state" };
                    format!("reload  {id} (generation {generation}, {state})")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Where a UI actor's frames go.
///
/// An actor that exports `strand_view` draws itself after every message it
/// handles (§6.9). The runtime calls it and hands the bytes
/// straight over: it knows *where* the actor left a frame and nothing about
/// what a frame means. Layout, widgets and the compositor stay on the other
/// side of this trait, which is what keeps the actor runtime free of the
/// renderer.
pub trait Frames: Send + Sync {
    /// `memory` is the actor's whole arena; the frame is `count` records
    /// starting at `base`.
    fn submit(&self, memory: &[u8], base: u32, count: u32);
}

/// A live measurement of one actor, for §8.4's debug overlay.
///
/// Sampled, not logged. The overlay asks "what is true now", and a number one
/// message stale is fine — it will be right again in a millisecond. The `Trace`
/// answers the other question, "what happened", and these deliberately stay
/// separate: history is a log, liveness is a gauge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorStats {
    pub id: ActorId,
    pub name: String,
    /// Linear-memory bytes wasmtime has committed to this actor's arena. §5.1's
    /// isolation claim as a number: memory no other actor can reach, and that
    /// a restart hands back in one deallocation.
    pub arena_bytes: u64,
    /// Messages waiting to be handled right now. A depth that climbs is an
    /// actor falling behind — the thing §6.1 says costs *it* and nobody else.
    pub mailbox: usize,
    /// Guest invocations in flight. Exactly one per busy actor today: §4.4's
    /// structured spawn is not in the language yet, so an actor *is* one fiber.
    /// The column exists because the number stops being 0-or-1 the day `scope`
    /// lands, and a gauge nobody wired up is a gauge nobody trusts.
    pub fibers: u32,
    /// Messages handled since this generation started. Resets on restart, which
    /// is the point: a restarted actor is a genuinely fresh one.
    pub handled: u64,
    /// How many times this actor has been restarted (§5.4).
    pub generation: u32,
    pub alive: bool,
}

/// The counters an actor bumps as it works.
///
/// Atomics rather than a lock, because an actor must never wait on the thing
/// watching it — the overlay is an observer, and observers do not get to
/// introduce contention into what they observe.
#[derive(Debug)]
struct StatCell {
    name: String,
    arena_bytes: AtomicU64,
    mailbox: AtomicU64,
    fibers: AtomicU32,
    handled: AtomicU64,
    generation: AtomicU32,
    alive: AtomicBool,
}

impl StatCell {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            arena_bytes: AtomicU64::new(0),
            mailbox: AtomicU64::new(0),
            fibers: AtomicU32::new(0),
            handled: AtomicU64::new(0),
            generation: AtomicU32::new(0),
            alive: AtomicBool::new(false),
        }
    }

    /// A new life starts. Everything resets because everything *is* new: the
    /// arena was dropped, so carrying a byte count across a restart would
    /// report memory that no longer exists.
    fn begin(&self, generation: u32) {
        self.generation.store(generation, Ordering::Relaxed);
        self.arena_bytes.store(0, Ordering::Relaxed);
        self.mailbox.store(0, Ordering::Relaxed);
        self.fibers.store(0, Ordering::Relaxed);
        self.handled.store(0, Ordering::Relaxed);
        self.alive.store(true, Ordering::Relaxed);
    }

    /// The life ended, however it ended. The row stays — a dead actor is
    /// exactly what someone watching the overlay wants to see.
    fn end(&self) {
        self.fibers.store(0, Ordering::Relaxed);
        self.alive.store(false, Ordering::Relaxed);
    }

    fn snapshot(&self, id: ActorId) -> ActorStats {
        ActorStats {
            id,
            name: self.name.clone(),
            arena_bytes: self.arena_bytes.load(Ordering::Relaxed),
            mailbox: self.mailbox.load(Ordering::Relaxed) as usize,
            fibers: self.fibers.load(Ordering::Relaxed),
            handled: self.handled.load(Ordering::Relaxed),
            generation: self.generation.load(Ordering::Relaxed),
            alive: self.alive.load(Ordering::Relaxed),
        }
    }
}

/// Everything needed to ask an actor to draw itself, resolved once per life.
struct Painter {
    sink: Arc<dyn Frames>,
    draw: wasmtime::TypedFunc<(), ()>,
    /// Where the frame's array starts. Immutable, so it is read once.
    base: u32,
    count: wasmtime::Global,
}

impl Painter {
    /// Calls the guest's view and hands the frame it built to the sink.
    ///
    /// The bytes are read out of the actor's own memory and copied nowhere: the
    /// sink borrows them for the length of the call.
    async fn paint(
        &self,
        store: &mut Store<ActorCtx>,
        memory: Option<&wasmtime::Memory>,
    ) -> wasmtime::Result<()> {
        self.draw.call_async(&mut *store, ()).await?;

        let wasmtime::Val::I32(count) = self.count.get(&mut *store) else {
            wasmtime::bail!("`strand_node_count` is not an i32");
        };
        if let Some(memory) = memory {
            self.sink.submit(memory.data(&*store), self.base, count as u32);
        }
        Ok(())
    }
}

/// Reads an i32 global, for the two the frame ABI exports.
fn read_u32_global(
    store: &mut Store<ActorCtx>,
    instance: &Instance,
    name: &str,
) -> Option<u32> {
    match instance.get_global(&mut *store, name)?.get(&mut *store) {
        wasmtime::Val::I32(value) => Some(value as u32),
        _ => None,
    }
}

/// Reads an actor's arena size. Guest memory only grows while guest code runs,
/// so sampling after each call is exact rather than approximate.
fn sample_arena(stats: &StatCell, memory: Option<&wasmtime::Memory>, store: &Store<ActorCtx>) {
    if let Some(memory) = memory {
        stats.arena_bytes.store(memory.data_size(store) as u64, Ordering::Relaxed);
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

/// Where one actor's out port leads: whose mailbox, and which of their ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wiring {
    pub to: ActorId,
    pub port: u32,
}

/// Someone to tell when an actor dies or is replaced (§5.4).
///
/// The bytes are prepared by whoever set the watch, not here. Encoding a value
/// means knowing a type's layout, and the runtime knowing that would be a
/// second implementation of §6.8 living next to the compiler's —
/// the mistake the host encoder exists to avoid. This carries what to deliver
/// and where; it does not know what it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watch {
    /// Who to tell.
    pub watcher: ActorId,
    /// Which of the watcher's in ports carries lifecycle news.
    pub port: u32,
    /// The `Down(...)` message, already encoded.
    pub down: Vec<u8>,
    /// The `Up(...)` message, already encoded.
    pub up: Vec<u8>,
}

/// Shared address book. Actors hold senders, never each other's memory.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<HashMap<ActorId, mpsc::UnboundedSender<Message>>>>,
    start: Instant,
    trace: Trace,
    /// Keyed by id and ordered, so the overlay's rows keep their places
    /// between frames instead of dancing.
    stats: Arc<Mutex<BTreeMap<ActorId, Arc<StatCell>>>>,
    /// Where each UI actor's frames go. On the registry rather than in
    /// `spawn_supervised`'s arguments because "where does this actor's output
    /// go" is the address book's question, and it is already the address book.
    frames: Arc<Mutex<HashMap<ActorId, Arc<dyn Frames>>>>,
    /// Each actor's out ports, indexed by port number. The same question as
    /// `frames` and so the same answer: an actor names a port, and the address
    /// book is what turns a name into a destination.
    outbound: Arc<Mutex<HashMap<ActorId, Vec<Option<Wiring>>>>>,
    /// Who to tell when a given actor dies or is replaced. Keyed by the actor
    /// being watched, because that is what the supervisor has in hand.
    watchers: Arc<Mutex<HashMap<ActorId, Vec<Watch>>>>,
    /// How to read each actor's state (§9.3). Here for the same reason
    /// `frames` is: it is a per-actor capability the supervisor needs and the
    /// actor itself never sees.
    states: Arc<Mutex<HashMap<ActorId, Arc<dyn Snapshots>>>>,
    /// Mailboxes created before their actor runs, waiting to be picked up.
    ///
    /// Without these, "can I send to you yet" depends on which task the
    /// scheduler happened to start first — and an actor that sends from `init`
    /// would work or not according to that. Reserving every mailbox before any
    /// actor runs turns a race into an ordering.
    pending: Arc<Mutex<HashMap<ActorId, mpsc::UnboundedReceiver<Message>>>>,
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
            stats: Arc::new(Mutex::new(BTreeMap::new())),
            frames: Arc::new(Mutex::new(HashMap::new())),
            outbound: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            states: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            config,
        }
    }

    pub fn trace(&self) -> Trace {
        self.trace.clone()
    }

    /// The cell an actor reports into. Created on first spawn and reused across
    /// restarts, so a crashed child's row keeps its place rather than vanishing
    /// and reappearing — watching the generation tick is the demonstration.
    fn stat_cell(&self, id: ActorId, name: &str) -> Arc<StatCell> {
        let mut cells = self.stats.lock().unwrap();
        cells.entry(id).or_insert_with(|| Arc::new(StatCell::new(name))).clone()
    }

    /// Says where actor `id`'s frames should go.
    ///
    /// Set before spawning: the actor draws itself once at startup, and a sink
    /// registered afterwards would miss that first frame.
    pub fn route_frames(&self, id: ActorId, sink: Arc<dyn Frames>) {
        self.frames.lock().unwrap().insert(id, sink);
    }

    fn frames_for(&self, id: ActorId) -> Option<Arc<dyn Frames>> {
        self.frames.lock().unwrap().get(&id).cloned()
    }

    /// Says where actor `id`'s out ports lead. Index is the port number the
    /// guest was compiled against; `None` is a port that was left unwired.
    ///
    /// Set before spawning, for the same reason `route_frames` is: an actor
    /// may send from `init`, and a wiring installed afterwards would have been
    /// installed too late.
    pub fn route_out(&self, id: ActorId, ports: Vec<Option<Wiring>>) {
        self.outbound.lock().unwrap().insert(id, ports);
    }

    fn wiring_for(&self, id: ActorId, port: u32) -> Option<Wiring> {
        self.outbound.lock().unwrap().get(&id)?.get(port as usize).copied().flatten()
    }

    /// Says who to tell when actor `id` dies or is replaced (§5.4).
    pub fn route_watchers(&self, id: ActorId, watchers: Vec<Watch>) {
        self.watchers.lock().unwrap().insert(id, watchers);
    }

    /// Says how to read actor `id`'s state (§9.3).
    ///
    /// Without one, the actor still runs: it crashes without a snapshot in its
    /// report, and a reload starts it from `init`. Hot reload is a thing the
    /// host offers an actor, not a thing the actor has to know about.
    pub fn route_state(&self, id: ActorId, codec: Arc<dyn Snapshots>) {
        self.states.lock().unwrap().insert(id, codec);
    }

    fn state_for(&self, id: ActorId) -> Option<Arc<dyn Snapshots>> {
        self.states.lock().unwrap().get(&id).cloned()
    }

    /// Puts messages back into a fresh mailbox after a reload.
    ///
    /// Deliberately not traced: each of these was recorded as sent when it was
    /// sent, and recording it a second time would make the causal log describe
    /// a message nobody produced.
    fn requeue(&self, to: ActorId, msg: Message) {
        let tx = self.inner.lock().unwrap().get(&to).cloned();
        if let Some(tx) = tx {
            let _ = tx.send(msg);
        }
    }

    /// Tells everyone watching `id` that it just went down, or came back.
    ///
    /// A watcher that has itself died is simply not there to be told, which is
    /// why this ignores send failures: an undeliverable notification is not a
    /// failure of the actor it is about.
    fn announce(&self, id: ActorId, alive: bool) {
        let watchers = self.watchers.lock().unwrap().get(&id).cloned().unwrap_or_default();
        for watch in watchers {
            let bytes = if alive { watch.up.clone() } else { watch.down.clone() };
            let _ = self.send_from(
                id,
                watch.watcher,
                Message::Blob { from: id, port: watch.port, bytes },
            );
        }
    }

    /// A snapshot of every actor this registry has ever spawned, live or dead
    /// (§8.4's debug overlay). Ordered by id.
    pub fn stats(&self) -> Vec<ActorStats> {
        let cells = self.stats.lock().unwrap();
        cells.iter().map(|(id, cell)| cell.snapshot(*id)).collect()
    }

    /// Creates an actor's mailbox before the actor exists to read it.
    ///
    /// Everything addressed to it queues until it starts, so a peer sending
    /// during its own startup does not have to know whether the far end is up
    /// yet — and a restarted actor's replacement can be written to during the
    /// gap between the old one dying and the new one instantiating.
    pub fn reserve(&self, id: ActorId) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.lock().unwrap().insert(id, tx);
        self.pending.lock().unwrap().insert(id, rx);
    }

    /// Reserves only if nobody has: a caller that laid out the whole tree in
    /// advance has already done this, and redoing it would throw away whatever
    /// had been queued since.
    fn reserve_once(&self, id: ActorId) {
        if !self.pending.lock().unwrap().contains_key(&id) {
            self.reserve(id);
        }
    }

    fn register(&self, id: ActorId, name: &str) -> mpsc::UnboundedReceiver<Message> {
        let reserved = self.pending.lock().unwrap().remove(&id);
        let rx = reserved.unwrap_or_else(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            self.inner.lock().unwrap().insert(id, tx);
            rx
        });
        self.trace.record(Event::Spawned { id, name: name.to_string() });
        rx
    }

    pub fn send(&self, to: ActorId, msg: Message) -> Result<()> {
        self.send_from(HOST, to, msg)
    }

    /// Delivers what an actor put on one of its out ports.
    ///
    /// An unwired port is a compile error (the checker refuses an `out` with
    /// no wire), so reaching this with nothing to deliver to means the host
    /// built a tree the compiler did not describe — worth saying rather than
    /// dropping.
    fn send_on_port(&self, from: ActorId, port: u32, bytes: Vec<u8>) -> Result<()> {
        let wiring = self
            .wiring_for(from, port)
            .ok_or_else(|| anyhow!("actor {from} has nothing wired to out port {port}"))?;
        self.send_from(from, wiring.to, Message::Blob { from, port: wiring.port, bytes })
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
    /// Buffers this actor currently owns (§5.3). Sending one removes it, which
    /// is what makes the transfer real rather than advisory.
    buffers: HashMap<u32, Vec<u8>>,
    next_buffer: u32,
}

impl ActorCtx {
    fn new(id: ActorId, name: String, registry: Registry) -> Self {
        Self { id, name, registry, buffers: HashMap::new(), next_buffer: 1 }
    }

    /// Takes a buffer out of this actor's hands. Returns `None` if the actor
    /// never owned it, or has already given it away.
    fn take_buffer(&mut self, handle: u32) -> Option<Vec<u8>> {
        self.buffers.remove(&handle)
    }
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

    // The guest names one of its own out ports; the registry knows where that
    // leads. Nothing in the guest can name another actor, so an actor cannot
    // reach one the `app` block did not wire it to — the capability is the
    // wiring (§9.2's direction, arrived at by having no addresses rather than
    // by checking them).
    linker.func_wrap(
        "strand",
        "send",
        |mut caller: Caller<'_, ActorCtx>, port: i32, ptr: i32, len: i32| -> wasmtime::Result<()> {
            let bytes = read_guest_bytes(&mut caller, ptr, len)?;
            let ctx = caller.data();
            ctx.registry
                .send_on_port(ctx.id, port as u32, bytes)
                .map_err(wasmtime::Error::from_anyhow)
        },
    )?;

    // §4.3's second tier. A panic is not an error value and there is nothing
    // to catch it: raising here unwinds out of the guest, the Store is dropped,
    // and the arena goes with it (§5.1). The supervisor gets a crash report
    // whose reason is what the guest wrote, which is the whole point of it
    // being a host call rather than a bare `unreachable`.
    linker.func_wrap(
        "strand",
        "panic",
        |mut caller: Caller<'_, ActorCtx>, ptr: i32, len: i32| -> wasmtime::Result<()> {
            let bytes = read_guest_bytes(&mut caller, ptr, len)?;
            wasmtime::bail!("{}", String::from_utf8_lossy(&bytes))
        },
    )?;

    // §5.3 ownership transfer. The sender copies bytes into a host-owned
    // buffer and gets a handle; sending it moves the allocation out of the
    // sender's table, so any later use of that handle traps. Data races are
    // unrepresentable rather than discouraged.
    linker.func_wrap(
        "strand",
        "buffer_create",
        |mut caller: Caller<'_, ActorCtx>, ptr: i32, len: i32| -> wasmtime::Result<i32> {
            let bytes = read_guest_bytes(&mut caller, ptr, len)?;
            let ctx = caller.data_mut();
            let handle = ctx.next_buffer;
            ctx.next_buffer += 1;
            ctx.buffers.insert(handle, bytes);
            Ok(handle as i32)
        },
    )?;

    linker.func_wrap(
        "strand",
        "buffer_send",
        |mut caller: Caller<'_, ActorCtx>, to: i32, handle: i32| -> wasmtime::Result<()> {
            let ctx = caller.data_mut();
            let Some(bytes) = ctx.take_buffer(handle as u32) else {
                wasmtime::bail!(
                    "buffer {handle} is not owned by this actor — it was already transferred"
                );
            };
            let from = ctx.id;
            ctx.registry
                .send_from(from, to as ActorId, Message::Blob { from, port: 0, bytes })
                .map_err(wasmtime::Error::from_anyhow)
        },
    )?;

    linker.func_wrap(
        "strand",
        "buffer_len",
        |caller: Caller<'_, ActorCtx>, handle: i32| -> wasmtime::Result<i32> {
            match caller.data().buffers.get(&(handle as u32)) {
                Some(bytes) => Ok(bytes.len() as i32),
                None => wasmtime::bail!(
                    "buffer {handle} is not owned by this actor — it was already transferred"
                ),
            }
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
    /// What the actor believed when it died (§9.4).
    ///
    /// The state global still holds the last state a handler returned — a
    /// handler that trapped never got to store its result — so this is the
    /// actor as it was going into the message that killed it, not a
    /// half-written one. `None` when nothing told the runtime how to read it.
    pub state: Option<Snapshot>,
}

impl CrashReport {
    fn with_state(mut self, state: Option<Snapshot>) -> Self {
        self.state = state;
        self
    }
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
        if let Some(state) = &self.state {
            write!(f, " · carrying its state ({} bytes)", state.bytes.len())?;
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
    // Debug renders the causal chain. wasmtime's outer message is always the
    // uninformative "error while executing at wasm backtrace:", and the part
    // worth reporting — a trap, or a host function's own message — sits under
    // "Caused by:".
    let rendered = format!("{error:?}");
    if let Some(rest) = rendered.split_once("Caused by:") {
        if let Some(cause) = rest.1.lines().map(str::trim).find(|line| !line.is_empty()) {
            // Multi-cause chains number their entries: "0: the real message".
            let cause = cause.split_once(": ").map_or(cause, |(index, text)| {
                if index.trim().chars().all(|c| c.is_ascii_digit()) { text } else { cause }
            });
            return cause.to_string();
        }
    }
    first_line(&rendered)
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

/// Why a life ended, when it ended on its own terms.
enum Exit {
    /// Told to stop. Nothing takes its place.
    Stopped,
    /// Newer code arrived (§9.3), with whatever the actor's state was at that
    /// moment and whatever was still queued behind the reload.
    /// Asked to start over on the same code, with the state let go.
    Restart { pending: Vec<Message> },
    Reload {
        bytes: Vec<u8>,
        /// How to read the *replacement's* state, once it is running.
        reader: Arc<dyn Snapshots>,
        /// What this life was holding when the reload arrived.
        state: Option<Snapshot>,
        pending: Vec<Message>,
    },
}

// A `CrashReport` is large, because §9.4 asks it to carry a state snapshot.
// Boxing it would save a copy on the one path that runs when an actor dies,
// and cost a `Box` at every call site that reads one.
#[allow(clippy::result_large_err)]
/// Runs one life of an actor: build the Store, run it, report why it ended.
///
/// Everything the actor owns is created here, so calling this again yields a
/// genuinely fresh arena — that is all "restart" means (§5.1). `restore` is
/// what makes it a *reload* rather than a restart: the arena is still new, and
/// the state that goes into it came from the one before.
async fn run_actor_once(
    engine: &Engine,
    registry: &Registry,
    id: ActorId,
    name: &str,
    module_bytes: &[u8],
    generation: u32,
    restore: Option<&Snapshot>,
) -> Result<Exit, CrashReport> {
    // Bracketed rather than sprinkled through the body: `run_life` fails out of
    // a dozen places, and every one of them ends a life.
    let stats = registry.stat_cell(id, name);
    stats.begin(generation);
    let outcome =
        run_life(engine, registry, id, name, module_bytes, generation, &stats, restore).await;
    stats.end();
    outcome
}

/// Everything still queued, so a life that ends on purpose can hand its
/// mailbox to the next one. A message already sent is not the caller's
/// mistake, and dropping it would make a reload cost whatever was in flight.
fn drain(mailbox: &mut mpsc::UnboundedReceiver<Message>) -> Vec<Message> {
    let mut pending = Vec::new();
    while let Ok(queued) = mailbox.try_recv() {
        pending.push(queued);
    }
    pending
}

/// Reads the actor's state, if anyone told the runtime how (§9.3).
///
/// Called on the way out of a life, whether that life ended in a trap or in a
/// reload — the state global holds the last state a handler returned either
/// way, because a handler that traps never stores its result.
fn capture_state(
    codec: Option<&Arc<dyn Snapshots>>,
    name: &str,
    store: &mut Store<ActorCtx>,
    instance: &Instance,
    memory: Option<&wasmtime::Memory>,
) -> Option<Snapshot> {
    let codec = codec?;
    let memory = memory?;
    let root = instance.get_global(&mut *store, "strand_state")?.get(&mut *store);
    match codec.capture(memory.data(&*store), root) {
        Ok(snapshot) => Some(snapshot),
        // Not fatal: the actor is already ending, and a report without a
        // snapshot beats a report that never arrives.
        Err(error) => {
            eprintln!("!! could not read `{name}`'s state: {error:#}");
            None
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::result_large_err)]
async fn run_life(
    engine: &Engine,
    registry: &Registry,
    id: ActorId,
    name: &str,
    module_bytes: &[u8],
    generation: u32,
    stats: &StatCell,
    restore: Option<&Snapshot>,
) -> Result<Exit, CrashReport> {
    let died = |reason: String, handling: Option<String>| CrashReport {
        actor: id,
        name: name.to_string(),
        generation,
        reason,
        handling,
        state: None,
    };
    let codec = registry.state_for(id);

    let module = Module::new(engine, module_bytes)
        .map_err(|e| died(format!("failed to compile: {}", reason(&e)), None))?;
    let mut linker = Linker::new(engine);
    link_host_abi(&mut linker).map_err(|e| died(format!("failed to link host ABI: {e}"), None))?;

    let ctx = ActorCtx::new(id, name.to_string(), registry.clone());
    let mut store = Store::new(engine, ctx);
    // Yield back to the scheduler on an epoch tick rather than trapping.
    store.set_epoch_deadline(1);
    store.epoch_deadline_async_yield_and_update(1);

    let mut mailbox = registry.register(id, name);
    // Announced here rather than by the supervisor, because here is where it
    // becomes true: this life has a mailbox, so a peer answering `Up` has
    // somewhere to put the answer. It fires for the first life as well as for
    // a replacement — an actor coming up for the first time is the same news,
    // and it saves every peer from having to send a speculative hello.
    registry.announce(id, true);
    let instance = linker.instantiate_async(&mut store, &module).await.map_err(|e| {
        died(format!("failed to instantiate: {}", reason(&e)), None)
    })?;

    let trace = registry.trace();
    let config = registry.config.clone();
    // Held for the actor's whole life: this handle is how the overlay learns
    // the arena's size without reaching into another actor's memory.
    let memory = instance.get_memory(&mut store, "memory");

    // A UI actor draws itself after every message (§6.5: the runtime re-invokes
    // the view). An actor that exports none of this is unaffected — the whole
    // path is `None` and never runs.
    let painter = registry.frames_for(id).and_then(|sink| {
        let draw = instance.get_typed_func::<(), ()>(&mut store, "strand_view").ok()?;
        let base = read_u32_global(&mut store, &instance, "strand_nodes")?;
        let count = instance.get_global(&mut store, "strand_node_count")?;
        Some(Painter { sink, draw, base, count })
    });

    match restore {
        // A reload: the state came from the life before, so `init` would
        // overwrite the very thing being carried across. §9.3's check has
        // already said this image belongs to this module's state type.
        Some(snapshot) => {
            snapshot::restore_async(snapshot, &mut store, &instance).await.map_err(|e| {
                died(format!("could not restore the state: {e:#}"), Some("reload".to_string()))
            })?;
        }
        None => {
            if let Ok(main) = instance.get_typed_func::<(), ()>(&mut store, "strand_main") {
                stats.fibers.store(1, Ordering::Relaxed);
                let started = main.call_async(&mut store, ()).await;
                stats.fibers.store(0, Ordering::Relaxed);
                sample_arena(stats, memory.as_ref(), &store);
                started.map_err(|e| died(reason(&e), Some("startup".to_string())))?;
            }
        }
    }
    sample_arena(stats, memory.as_ref(), &store);

    // The first frame, so a window has something to show before anyone touches
    // it.
    if let Some(painter) = &painter {
        if let Err(e) = painter.paint(&mut store, memory.as_ref()).await {
            let state =
                capture_state(codec.as_ref(), name, &mut store, &instance, memory.as_ref());
            return Err(died(reason(&e), Some("first frame".to_string())).with_state(state));
        }
    }

    let on_message =
        instance.get_typed_func::<(i32, i32, i32), ()>(&mut store, "strand_on_message");
    while let Some(msg) = mailbox.recv().await {
        // What is still queued behind the message just taken.
        stats.mailbox.store(mailbox.len() as u64, Ordering::Relaxed);
        match msg {
            Message::Stop => break,
            // A host-side supervisor handles this today; guests ignore it.
            Message::ChildDown { .. } => continue,
            // Newer code. Read the state before the arena holding it goes, and
            // take everything still queued along, so a reload costs no message
            // that was already on its way.
            Message::Restart => return Ok(Exit::Restart { pending: drain(&mut mailbox) }),
            Message::Reload { bytes, state: reader } => {
                let state =
                    capture_state(codec.as_ref(), name, &mut store, &instance, memory.as_ref());
                return Ok(Exit::Reload {
                    bytes,
                    reader,
                    state,
                    pending: drain(&mut mailbox),
                });
            }
            Message::Blob { from, port, bytes } => {
                if config.chaos {
                    // Seeded jitter: a different interleaving per seed, but the
                    // same one every time for a given seed.
                    let delay = config.next() % 8;
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                trace.record(Event::Delivered { to: id, from, len: bytes.len() });

                let Ok(handler) = &on_message else { continue };
                let summary = describe(&bytes, from);
                let ptr = match write_into_guest(&mut store, &instance, &bytes, from).await {
                    Ok(ptr) => ptr,
                    Err(e) => {
                        let state = capture_state(
                            codec.as_ref(), name, &mut store, &instance, memory.as_ref(),
                        );
                        return Err(died(e.to_string(), Some(summary)).with_state(state));
                    }
                };

                stats.fibers.store(1, Ordering::Relaxed);
                let handled =
                    handler.call_async(&mut store, (port as i32, ptr, bytes.len() as i32)).await;
                stats.fibers.store(0, Ordering::Relaxed);
                // Sampled whether the call returned or trapped: a crash report
                // is more useful next to the arena size that produced it.
                sample_arena(stats, memory.as_ref(), &store);
                stats.handled.fetch_add(1, Ordering::Relaxed);
                // §9.4's crash report, with the state the actor held going
                // into the message that killed it. The handler never stored
                // its result, so what is in the arena is the last good one.
                if let Err(e) = handled {
                    let state = capture_state(
                        codec.as_ref(), name, &mut store, &instance, memory.as_ref(),
                    );
                    return Err(died(reason(&e), Some(summary)).with_state(state));
                }

                // State has moved on, so the view has too.
                if let Some(painter) = &painter {
                    if let Err(e) = painter.paint(&mut store, memory.as_ref()).await {
                        let state = capture_state(
                            codec.as_ref(), name, &mut store, &instance, memory.as_ref(),
                        );
                        return Err(died(reason(&e), Some(summary)).with_state(state));
                    }
                    sample_arena(stats, memory.as_ref(), &store);
                }
            }
        }
    }

    trace.record(Event::Stopped { id });
    Ok(Exit::Stopped)
}

/// Spawns one unsupervised actor. A crash ends the task.
///
/// `module_bytes` is either a compiled module or `.wat` source — wasmtime
/// accepts both, so a Strand-compiled actor and a hand-written fixture host
/// the same way.
pub async fn spawn_actor(
    engine: &Engine,
    registry: &Registry,
    id: ActorId,
    name: &str,
    module_bytes: &[u8],
) -> Result<tokio::task::JoinHandle<Result<(), CrashReport>>> {
    let engine = engine.clone();
    let registry = registry.clone();
    let (name, bytes) = (name.to_string(), module_bytes.to_vec());
    Ok(tokio::spawn(async move {
        run_actor_once(&engine, &registry, id, &name, &bytes, 0, None).await.map(|_| ())
    }))
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
    module_bytes: &[u8],
    policy: Policy,
    parent: Option<ActorId>,
) -> tokio::task::JoinHandle<Result<(), CrashReport>> {
    let engine = engine.clone();
    let registry = registry.clone();
    let (name, mut bytes) = (name.to_string(), module_bytes.to_vec());

    tokio::spawn(async move {
        let mut generation = 0;
        // What the next life starts from: `None` is `init`, `Some` is the
        // state the life before it was holding (§9.3).
        let mut restore: Option<Snapshot> = None;
        registry.reserve_once(id);
        loop {
            let outcome =
                run_actor_once(&engine, &registry, id, &name, &bytes, generation, restore.as_ref())
                    .await;
            restore = None;
            match outcome {
                Ok(Exit::Stopped) => return Ok(()),
                // Hot reload is a supervisor restart with newer code, which is
                // why it lands here rather than in a machine of its own.
                // Same code, no state: §5.4's fresh restart, on request.
                Ok(Exit::Restart { pending }) => {
                    generation += 1;
                    registry.trace.record(Event::Restarted { id, generation });
                    registry.reserve(id);
                    for message in pending {
                        registry.requeue(id, message);
                    }
                }
                Ok(Exit::Reload { bytes: next, reader, state, pending }) => {
                    // The one check §9.3 turns on: the image is a value of the
                    // state type the *old* module had, and it may only be read
                    // as the new one's if those are the same shape. When they
                    // are not, `init` is the honest answer — reinterpreting
                    // the bytes would be the bug.
                    let carried = state.as_ref().is_some_and(|state| state.fits(reader.shape()));
                    restore = if carried { state } else { None };
                    // Now, and not before: until this point the actor was
                    // still the old one, and its state still had the old
                    // shape.
                    registry.route_state(id, reader);
                    bytes = next;
                    generation += 1;
                    registry.trace.record(Event::Reloaded { id, generation, carried });
                    // A fresh mailbox, then everything that was queued behind
                    // the reload put back into it: new code, same conversation.
                    registry.reserve(id);
                    for message in pending {
                        registry.requeue(id, message);
                    }
                }
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
                    // §5.4, from the guest's side: anyone this actor was wired
                    // to hears that it is gone. Announced before the decision
                    // to restart, because "down" is true either way and a
                    // watcher should not have to wait on the policy to learn
                    // it.
                    registry.announce(id, false);

                    if policy == Policy::Stop {
                        return Err(report);
                    }
                    generation += 1;
                    registry.trace.record(Event::Restarted { id, generation });
                    // A fresh mailbox for the replacement, so that anything
                    // sent during the gap is waiting rather than refused. The
                    // `Up` that goes with it is announced by the new life
                    // itself, once it has picked this up.
                    registry.reserve(id);
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
