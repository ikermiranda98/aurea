//! Aurea trading engine — the Rust port of Openbot's `packages/core/src/trading.ts`.
//!
//! Phase 1 hosts the shared surface ([`zeron_proto::trading`] types, the snapshot
//! watch channel the RPC layer streams) plus [`mock`], the fixture source the
//! dashboard consumes until the real engine (lane A) lands.

pub mod mock;

/// Shared protocol types re-exported for convenience.
pub use zeron_proto::trading;
pub use zeron_proto::warp;

use tokio::sync::watch;

/// A live view of the engine's snapshot — what `WatchTradingSnapshot` streams.
pub type SnapshotWatch = watch::Receiver<zeron_proto::trading::Snapshot>;

static MOCK_WATCH: std::sync::OnceLock<SnapshotWatch> = std::sync::OnceLock::new();

/// The mock snapshot watch used while no real engine is wired in (shared by
/// every subscriber, so the fixture refreshes once per tick, not per caller).
pub fn mock_watch() -> SnapshotWatch {
    MOCK_WATCH.get_or_init(mock::watch).clone()
}

/// The snapshot as it stands right now, actions included.
pub fn snapshot() -> zeron_proto::trading::Snapshot {
    mock::current()
}

/// The engine's scheduled tasks, newest first.
pub fn task_list() -> Vec<zeron_proto::trading::Task> {
    mock::current().tasks
}
