//! B7 — the close gate: closing Aurea over live trading asks first.
//!
//! Port of Openbot's `desktop/src/main/windows/trading-close.ts`. The bots are
//! children of the engine process, so they keep trading — and the positions
//! stay exposed — without the window. A window close or an app quit therefore
//! reads one snapshot first and:
//!
//! * passes silently when nothing is exposed (no running bot, no open row on
//!   any account) or when the snapshot is the demo fixture;
//! * otherwise asks once, listing the bots and the symbols;
//! * is **fail-closed**: an engine that does not answer within
//!   [`TIMEOUT`] is "could not verify", never "nothing to warn about".
//!
//! The gate reads `open_positions` (every account of the login, D4), not the
//! shown account's `positions`: a multi-account login must not close silently
//! over rows this view never named. Symbols deduplicate — the same symbol may
//! sit open on more than one account.
//!
//! An approval is good for one exit within [`APPROVAL_WINDOW`]: the window
//! close the user approved and the quit it triggers never ask twice.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use zeron_proto::trading::Snapshot;

/// The close must not hang on a stalled engine: a quick check, then ask honestly.
pub const TIMEOUT: Duration = Duration::from_millis(1_500);
/// How long one "Cerrar igualmente" covers the exits it sets off.
pub const APPROVAL_WINDOW: Duration = Duration::from_secs(10);
/// The demo fixture's version: its rows are invented, nothing real is exposed.
const DEMO_VERSION: &str = "0.1.0-mock";

/// What an exit would leave running: bot names and open symbols.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exposure {
    pub bots: Vec<String>,
    pub symbols: Vec<String>,
}

impl Exposure {
    /// The engine's whole exposure, not the shown account's.
    pub fn of(snapshot: &Snapshot) -> Self {
        if snapshot.version == DEMO_VERSION {
            return Self::default();
        }
        let bots = snapshot
            .systems
            .iter()
            .filter(|system| system.running)
            .map(|system| system.name.clone())
            .collect();
        let rows = snapshot
            .open_positions
            .as_ref()
            .unwrap_or(&snapshot.positions);
        let mut symbols: Vec<String> = Vec::new();
        for row in rows.iter().filter(|row| row.size > 0.0) {
            if !symbols.contains(&row.symbol) {
                symbols.push(row.symbol.clone());
            }
        }
        Self { bots, symbols }
    }

    pub fn is_empty(&self) -> bool {
        self.bots.is_empty() && self.symbols.is_empty()
    }
}

/// The gate's answer to one exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing exposed: close without asking.
    Clear,
    /// Ask once; the safe default is to stay open.
    Warn { title: String, detail: String },
}

/// `answer` is the snapshot read at close time, `None` when the engine did
/// not answer in time (or answered garbage).
pub fn verdict(answer: Option<&Snapshot>) -> Verdict {
    let Some(snapshot) = answer else {
        return Verdict::Warn {
            title: "No se pudo comprobar la mesa de trading".into(),
            detail: "El motor no respondió a tiempo. Puede haber bots en marcha o posiciones \
                     abiertas que seguirían expuestas al cerrar Aurea."
                .into(),
        };
    };
    let exposure = Exposure::of(snapshot);
    let names = exposure.bots.join(", ");
    let symbols = exposure.symbols.join(", ");
    match (exposure.bots.is_empty(), exposure.symbols.is_empty()) {
        (true, true) => Verdict::Clear,
        (false, false) => Verdict::Warn {
            title: "Hay bots en marcha y posiciones abiertas".into(),
            detail: format!(
                "Bots: {names}. Posiciones: {symbols}. Los bots siguen operando sin la ventana \
                 y pueden abrir más posiciones; las abiertas quedan sin vigilancia."
            ),
        },
        (false, true) => Verdict::Warn {
            title: "Hay bots de trading en marcha".into(),
            detail: format!("{names} seguirán operando sin la ventana."),
        },
        (true, false) => Verdict::Warn {
            title: "Hay posiciones abiertas".into(),
            detail: format!("{symbols} seguirán abiertas sin vigilancia al cerrar Aurea."),
        },
    }
}

static APPROVED_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Records the user's "close anyway" (or a silent clear) for the next exit.
pub fn approve() {
    *APPROVED_AT.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
}

/// Consumes a fresh approval. A stale one is dropped and reads as none.
pub fn take_approval() -> bool {
    let mut approved = APPROVED_AT.lock().unwrap_or_else(|p| p.into_inner());
    matches!(approved.take(), Some(at) if at.elapsed() <= APPROVAL_WINDOW)
}

/// Forgets any approval (the user said no: the next exit asks again).
pub fn revoke() {
    *APPROVED_AT.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::trading::{Position, PositionSide, System};

    fn system(name: &str) -> System {
        System {
            id: format!("01_{name}"),
            name: name.into(),
            running: false,
            active: false,
            pid: None,
            started_at: None,
            symbol: "MNQ".into(),
            timeframe: "1m".into(),
            pnl_day: None,
            client_id: None,
            close_hour: None,
            last_activity_at: None,
            description: None,
            rules: None,
        }
    }

    fn snapshot(version: &str) -> Snapshot {
        Snapshot {
            version: version.into(),
            connected: true,
            client_in_use: None,
            account: None,
            systems: vec![system("Sorro")],
            positions: Vec::new(),
            open_positions: Some(Vec::new()),
            closable_positions: None,
            logs: Vec::new(),
            tasks: Vec::new(),
            trades: None,
            equity: None,
            accounts: None,
            links: None,
            systems_dir: None,
            programs: None,
        }
    }

    fn live() -> Snapshot {
        snapshot("nativo-1")
    }

    fn row(symbol: &str, size: f64) -> Position {
        Position {
            symbol: symbol.into(),
            side: PositionSide::Long,
            size,
            entry: None,
            current: None,
            pnl: None,
            pnl_pct: None,
        }
    }

    #[test]
    fn nothing_exposed_closes_without_asking() {
        assert_eq!(verdict(Some(&live())), Verdict::Clear);
    }

    #[test]
    fn the_demo_fixture_never_blocks_a_close() {
        let mut demo = snapshot(DEMO_VERSION);
        demo.systems[0].running = true;
        demo.positions = vec![row("EURUSD", 1.0)];
        assert_eq!(verdict(Some(&demo)), Verdict::Clear);
    }

    #[test]
    fn no_answer_is_fail_closed() {
        assert!(matches!(verdict(None), Verdict::Warn { .. }));
    }

    #[test]
    fn exposure_reads_every_account_and_deduplicates_symbols() {
        let mut snapshot = live();
        // The shown account is flat; a sibling account holds MNQ twice-listed.
        snapshot.open_positions = Some(vec![row("MNQZ6", 1.0), row("MNQZ6", 2.0), row("ES", 0.0)]);
        let exposure = Exposure::of(&snapshot);
        assert_eq!(exposure.symbols, vec!["MNQZ6"]);
        let Verdict::Warn { title, detail } = verdict(Some(&snapshot)) else {
            panic!("an open row on any account must warn");
        };
        assert_eq!(title, "Hay posiciones abiertas");
        assert!(detail.contains("MNQZ6"));
    }

    #[test]
    fn bots_and_positions_read_as_one_mixed_warning() {
        let mut snapshot = live();
        snapshot.systems[0].running = true;
        let name = snapshot.systems[0].name.clone();
        snapshot.open_positions = Some(vec![row("EURUSD", 1.0)]);
        let Verdict::Warn { title, detail } = verdict(Some(&snapshot)) else {
            panic!("must warn");
        };
        assert_eq!(title, "Hay bots en marcha y posiciones abiertas");
        assert!(detail.contains(&name) && detail.contains("EURUSD"));
    }

    #[test]
    fn an_approval_covers_one_exit() {
        revoke();
        assert!(!take_approval());
        approve();
        assert!(take_approval());
        assert!(!take_approval());
    }
}
