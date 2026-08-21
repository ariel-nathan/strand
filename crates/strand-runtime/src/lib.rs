//! Strand actor runtime — M0 walking skeleton.
//!
//! An actor is a wasmtime `Store` (its memory arena), a tokio task (its
//! scheduling unit), and a mailbox (its only channel to the outside world).
//! Actors share no memory: the only way in is a `Message`.
//!
//! Error-type note: wasmtime 48 no longer uses `anyhow`, it has its own
//! `wasmtime::Error`. Host callbacks must hand back `wasmtime::Result`
//! (the only shape `WasmRet` is implemented for), while this crate's own
//! API speaks `anyhow::Result`. `?` bridges the two via wasmtime's
//! `anyhow` feature, which is on by default.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::sync::mpsc;
use wasmtime::error::Context as _;
use wasmtime::{Caller, Config, Engine, Extern, Instance, Linker, Module, Store};

pub type ActorId = u32;

/// The typed message set. For M0 this is deliberately tiny; §5.3's
/// buffer-transfer variants land in M2.
#[derive(Debug, Clone)]
pub enum Message {
    /// A payload produced by guest code, tagged with its sender.
    Blob { from: ActorId, bytes: Vec<u8> },
    /// Asks the actor to shut down cleanly after draining.
    Stop,
}

/// Shared address book. Actors hold senders, never each other's memory.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<HashMap<ActorId, mpsc::UnboundedSender<Message>>>>,
    start: Instant,
}

impl Registry {
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(HashMap::new())), start: Instant::now() }
    }

    fn register(&self, id: ActorId) -> mpsc::UnboundedReceiver<Message> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.lock().unwrap().insert(id, tx);
        rx
    }

    pub fn send(&self, to: ActorId, msg: Message) -> Result<()> {
        let tx = {
            let map = self.inner.lock().unwrap();
            map.get(&to).cloned().ok_or_else(|| anyhow!("no actor {to}"))?
        };
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
                .send(to as ActorId, Message::Blob { from: ctx.id, bytes })
                .map_err(wasmtime::Error::from_anyhow)
        },
    )?;

    // The colorless-concurrency load-bearing call: suspends the *fiber*, not
    // the OS thread. Everything else in M0 is scaffolding around this.
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

/// Spawns one actor: its own Store, its own instance, its own tokio task.
pub async fn spawn_actor(
    engine: &Engine,
    registry: &Registry,
    id: ActorId,
    name: &str,
    wat: &str,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let module = Module::new(engine, wat).with_context(|| format!("compiling actor `{name}`"))?;
    let mut linker = Linker::new(engine);
    link_host_abi(&mut linker)?;

    let ctx = ActorCtx { id, name: name.to_string(), registry: registry.clone() };
    let mut store = Store::new(engine, ctx);
    // Yield back to the scheduler on an epoch tick rather than trapping.
    store.set_epoch_deadline(1);
    store.epoch_deadline_async_yield_and_update(1);

    let mut mailbox = registry.register(id);
    let instance = linker.instantiate_async(&mut store, &module).await?;

    Ok(tokio::spawn(async move {
        if let Ok(main) = instance.get_typed_func::<(), ()>(&mut store, "strand_main") {
            main.call_async(&mut store, ()).await?;
        }

        let on_message = instance.get_typed_func::<(i32, i32), ()>(&mut store, "strand_on_message");
        while let Some(msg) = mailbox.recv().await {
            match msg {
                Message::Stop => break,
                Message::Blob { from, bytes } => {
                    let Ok(handler) = &on_message else { continue };
                    let ptr = write_into_guest(&mut store, &instance, &bytes, from).await?;
                    handler.call_async(&mut store, (ptr, bytes.len() as i32)).await?;
                }
            }
        }
        Ok(())
    }))
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
