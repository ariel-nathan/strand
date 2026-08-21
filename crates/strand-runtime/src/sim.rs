//! Deterministic simulation of the actor runtime.
//!
//! §17 takes the TigerBeetle lesson literally: build
//! the scheduler so a run can be replayed exactly, before the scheduler grows
//! complicated enough that it cannot be.
//!
//! Three things make a run reproducible:
//!
//! 1. **One thread.** A `current_thread` runtime polls tasks in a fixed order;
//!    tokio's work-stealing multi-thread scheduler cannot promise that.
//! 2. **Virtual time.** With the clock paused, tokio advances it to the next
//!    timer instead of waiting, so `sleep_ms` resolves in a fixed order and a
//!    simulated second costs nothing.
//! 3. **Seeded chaos.** Any deliberate perturbation comes from one seeded
//!    generator, so a seed names an interleaving.
//!
//! What this is not: a full deterministic simulator. Real fault injection —
//! dropped messages, partitions, restarts mid-handler — is M2 work and beyond.
//! This is the substrate those need, put in place before the scheduler hardens.

use std::future::Future;
use std::sync::Arc;

use anyhow::Result;

use crate::{Registry, SimConfig, Trace};

/// How a simulated run should behave.
#[derive(Debug, Clone, Copy)]
pub struct SimOptions {
    /// Names the interleaving. The same seed replays the same run.
    pub seed: u64,
    /// Perturbs message handling to explore a different ordering.
    pub chaos: bool,
}

impl SimOptions {
    /// A plain, unperturbed run.
    pub fn new(seed: u64) -> Self {
        Self { seed, chaos: false }
    }

    pub fn chaotic(seed: u64) -> Self {
        Self { seed, chaos: true }
    }
}

impl Default for SimOptions {
    fn default() -> Self {
        Self::new(0)
    }
}

/// Runs `scenario` deterministically and returns its causal message log.
///
/// The scenario receives a `Registry` wired to the simulated scheduler; it
/// should spawn actors through it exactly as production code does, so what is
/// tested is the real runtime rather than a stand-in.
pub fn run<F, Fut>(options: SimOptions, scenario: F) -> Result<Trace>
where
    F: FnOnce(Registry) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        // Virtual time: auto-advances to the next deadline rather than sleeping.
        .start_paused(true)
        .build()?;

    let registry = Registry::with_config(Arc::new(SimConfig::new(options.seed, options.chaos)));
    let trace = registry.trace();

    runtime.block_on(scenario(registry))?;
    Ok(trace)
}
