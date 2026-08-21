//! Wiring the runtime's actor measurements to the compositor's overlay (§8.4).
//!
//! `strand-runtime` knows how to measure an actor and nothing about frames.
//! `strand-render` knows how to draw a row and nothing about wasmtime. This
//! module is the only place that knows both, and it is fifteen lines — which is
//! the argument for keeping them apart.

use std::time::Duration;

use strand_render::inspect::{ActorStat, StatsHandle};
use strand_runtime::{ActorStats, Registry};

/// How often the overlay's numbers refresh.
///
/// Fast enough that a mailbox filling up is something you watch happen, slow
/// enough that observing an actor costs it nothing measurable.
const INTERVAL: Duration = Duration::from_millis(50);

pub fn to_row(stats: ActorStats) -> ActorStat {
    ActorStat {
        name: stats.name,
        arena_bytes: stats.arena_bytes,
        mailbox: stats.mailbox,
        fibers: stats.fibers,
        handled: stats.handled,
        generation: stats.generation,
        alive: stats.alive,
    }
}

/// Samples `registry` into `handle` forever.
///
/// Runs as an ordinary task on the actor runtime, so it is scheduled like
/// anything else and cannot preempt the actors it is watching.
pub async fn publish(registry: Registry, handle: StatsHandle) {
    loop {
        handle.publish(registry.stats().into_iter().map(to_row).collect());
        tokio::time::sleep(INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_gauge_survives_the_crossing() {
        let row = to_row(ActorStats {
            id: 3,
            name: "ticker".into(),
            arena_bytes: 1_114_112,
            mailbox: 4,
            fibers: 1,
            handled: 91,
            generation: 2,
            alive: true,
        });

        assert_eq!(row.name, "ticker");
        assert_eq!(row.arena_bytes, 1_114_112);
        assert_eq!(row.mailbox, 4);
        assert_eq!(row.fibers, 1);
        assert_eq!(row.handled, 91);
        assert_eq!(row.generation, 2);
        assert!(row.alive);
    }
}
