//! One snapshot for the desk: the engine's half and the broker's half.
//!
//! Two owners fill a [`Snapshot`]. The [`crate::TradingEngine`] knows the
//! pool — systems, bots, tasks, trades and its own log — while the
//! [`TradingService`] knows the broker: the TWS link, its accounts, the
//! positions of every account and the equity curve. Handing out only one of
//! them hides the other: the engine's state never receives TWS rows, so a
//! dashboard (or the B7 close gate) fed by it alone would read a live login
//! as flat.
//!
//! The rule:
//!
//! * **broker live** → broker fields from the service, pool fields from the
//!   engine, logs interleaved by time;
//! * **broker in demo, engine with a pool** → the engine as it is: the demo
//!   fixture's invented positions must not sit next to real bots;
//! * **broker in demo, no pool** → the demo fixture, as the desk has always
//!   shown on a machine without TWS or systems.

use tokio::sync::watch;
use zeron_proto::trading::{Programs, Snapshot};

use crate::SnapshotWatch;
use crate::service::TradingService;

/// How long a change waits for the rest of its burst before the merged desk
/// goes out (see [`TradingService::watch_merged`]).
pub const MERGE_COALESCE: std::time::Duration = std::time::Duration::from_millis(250);

/// How many log rows the merged snapshot keeps (the engine ring is 800).
const MAX_LOGS: usize = 800;

/// Merges `engine` (pool) with `broker` (TWS and MT5). The broker half is
/// always real (empty while nothing is connected), so both halves always
/// combine: the systems from the pool, the money from the links.
pub fn merge(engine: &Snapshot, broker: &Snapshot, _broker_live: bool) -> Snapshot {
    let mut logs: Vec<_> = engine.logs.iter().chain(broker.logs.iter()).cloned().collect();
    logs.sort_by_key(|log| log.ts);
    if logs.len() > MAX_LOGS {
        logs.drain(..logs.len() - MAX_LOGS);
    }
    Snapshot {
        version: engine.version.clone(),
        connected: broker.connected,
        client_in_use: broker.client_in_use,
        account: broker.account.clone(),
        systems: engine.systems.clone(),
        positions: broker.positions.clone(),
        open_positions: broker.open_positions.clone(),
        closable_positions: broker.closable_positions.clone(),
        logs,
        tasks: engine.tasks.clone(),
        rules: broker.rules.clone(),
        trades: engine.trades.clone(),
        equity: broker.equity.clone(),
        accounts: broker.accounts.clone(),
        links: broker.links.clone(),
        systems_dir: engine.systems_dir.clone(),
        programs: match (&broker.programs, &engine.programs) {
            (Some(tws), mt5) => Some(Programs {
                tws: tws.tws,
                mt5: mt5.as_ref().and_then(|p| p.mt5).or(tws.mt5),
            }),
            (None, other) => other.clone(),
        },
        proposals: broker.proposals.clone(),
    }
}

impl TradingService {
    /// `engine`'s snapshot completed with this service's broker half.
    pub fn merged_with(&self, engine: Snapshot) -> Snapshot {
        merge(&engine, &self.snapshot(), self.live())
    }

    /// A stream of merged snapshots: it republishes whenever either half
    /// changes, and stops once every receiver is gone. Outside a tokio
    /// runtime it holds the first merge only.
    ///
    /// A burst (TWS sends account values and portfolio rows by the dozen) is
    /// coalesced: after a change it waits [`MERGE_COALESCE`] and merges the
    /// latest of both halves once, and an unchanged merge is not re-sent.
    /// Every send costs a JSON round trip and a full window redraw.
    pub fn watch_merged(&self, mut engine: SnapshotWatch) -> SnapshotWatch {
        let mut broker = self.watch();
        let first = self.merged_with(engine.borrow_and_update().clone());
        broker.borrow_and_update();
        let (tx, rx) = watch::channel(first);
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return rx;
        };
        let service = self.clone();
        handle.spawn(async move {
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    changed = engine.changed() => if changed.is_err() { break },
                    changed = broker.changed() => if changed.is_err() { break },
                }
                tokio::time::sleep(MERGE_COALESCE).await;
                let engine_half = engine.borrow_and_update().clone();
                broker.borrow_and_update();
                crate::engine::publish_if_changed(&tx, service.merged_with(engine_half));
            }
        });
        rx
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn without_a_link_the_pool_shows_and_no_money_is_invented() {
        let mut engine = crate::mock::current();
        engine.account = None;
        engine.positions.clear();
        let broker = crate::service::TradingService::new().snapshot();
        let merged = merge(&engine, &broker, false);
        assert_eq!(merged.systems.len(), engine.systems.len());
        assert!(merged.account.is_none());
        assert!(merged.positions.is_empty());
    }

    use super::*;
    use zeron_proto::trading::{Log, LogLevel, Position, PositionSide, ProgramState};

    fn position(symbol: &str) -> Position {
        Position {
            symbol: symbol.into(),
            side: PositionSide::Long,
            size: 1.0,
            entry: None,
            current: None,
            pnl: None,
            pnl_pct: None,
        }
    }

    fn log(ts: i64, message: &str) -> Log {
        Log {
            ts,
            level: LogLevel::Info,
            system: "motor".into(),
            message: message.into(),
            message_key: None,
            message_params: None,
            event: None,
        }
    }

    fn engine_with_pool() -> Snapshot {
        let mut engine = crate::mock::current();
        engine.version = "nativo-1".into();
        engine.positions.clear();
        engine.open_positions = Some(Vec::new());
        engine.closable_positions = Some(Vec::new());
        engine.systems_dir = Some("C:/pool".into());
        engine.logs = vec![log(10, "engine")];
        engine.programs = Some(Programs {
            tws: ProgramState::Closed,
            mt5: Some(ProgramState::Running),
        });
        engine
    }

    #[test]
    fn a_live_broker_brings_its_positions_and_the_engine_its_systems() {
        let engine = engine_with_pool();
        let mut broker = crate::mock::current();
        broker.systems.clear();
        broker.positions = vec![position("EURUSD")];
        broker.open_positions = Some(vec![position("EURUSD"), position("MNQZ6")]);
        broker.closable_positions = Some(vec![position("EURUSD")]);
        broker.logs = vec![log(5, "tws"), log(20, "tws later")];
        broker.programs = Some(Programs {
            tws: ProgramState::Running,
            mt5: None,
        });

        let merged = merge(&engine, &broker, true);
        assert_eq!(merged.version, "nativo-1");
        assert_eq!(merged.systems, engine.systems);
        assert_eq!(merged.open_positions.as_ref().map(Vec::len), Some(2));
        assert_eq!(merged.closable_positions.as_ref().map(Vec::len), Some(1));
        assert_eq!(merged.systems_dir.as_deref(), Some("C:/pool"));
        let order: Vec<_> = merged.logs.iter().map(|l| l.ts).collect();
        assert_eq!(order, vec![5, 10, 20]);
        let programs = merged.programs.unwrap();
        assert_eq!(programs.tws, ProgramState::Running);
        assert_eq!(programs.mt5, Some(ProgramState::Running));
    }

    #[tokio::test]
    async fn the_merged_watch_follows_the_engine_half() {
        let service = TradingService::new();
        let (engine_tx, engine_rx) = watch::channel(engine_with_pool());
        let mut merged = service.watch_merged(engine_rx);
        assert_eq!(merged.borrow_and_update().version, "nativo-1");

        let mut next = engine_with_pool();
        next.version = "nativo-2".into();
        engine_tx.send_replace(next);
        tokio::time::timeout(std::time::Duration::from_secs(2), merged.changed())
            .await
            .expect("a merged update")
            .unwrap();
        assert_eq!(merged.borrow().version, "nativo-2");
    }
}
