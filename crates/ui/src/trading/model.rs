//! Presentation model for the trading surface: derived KPIs, Spanish number
//! formatting and the small state machines the views share.
//!
//! Pure data — no GPUI types here — so it is unit-testable and reusable by the
//! (later) trading fixtures and the MCP tool views.

use chrono::{Local, TimeZone};
use zeron_proto::trading::{
    Log, LogLevel, Position, ProposalStatus, Rule, Snapshot, System, Task, TaskStatus,
    TradingProposal,
};

/// What the surface knows about its snapshot stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Phase {
    /// Subscribing / waiting for the first snapshot.
    #[default]
    Connecting,
    /// The stream is delivering snapshots.
    Live,
    /// The stream failed to open or closed.
    Failed(String),
}

/// An action awaiting the user's confirmation (money-touching or disruptive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// Close one position, by symbol.
    Close(String),
    /// Stop every bot and close everything.
    Panic,
    /// Stop one bot, by system id.
    Stop(String),
}

impl Confirm {
    pub fn title(&self) -> String {
        match self {
            Confirm::Close(symbol) => format!("¿Cerrar la posición {symbol}?"),
            Confirm::Panic => "¿Pánico: parar todos los bots y cerrar todo?".into(),
            Confirm::Stop(system) => format!("¿Parar el bot {system}?"),
        }
    }

    pub fn detail(&self) -> &'static str {
        match self {
            Confirm::Close(_) => "Se enviará una orden de cierre al broker. No se puede deshacer.",
            Confirm::Panic => {
                "Se pararán todos los bots y se cerrará toda la exposición. No se puede deshacer."
            }
            Confirm::Stop(_) => "El bot dejará de operar; sus posiciones abiertas se quedan.",
        }
    }

    pub fn accept_label(&self) -> &'static str {
        match self {
            Confirm::Close(_) => "Confirmar cierre",
            Confirm::Panic => "Ejecutar pánico",
            Confirm::Stop(_) => "Parar bot",
        }
    }

    /// Whether the action moves money (panics the buttons in danger tone).
    pub fn is_money(&self) -> bool {
        !matches!(self, Confirm::Stop(_))
    }
}

/// Header numbers for the KPI strip.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Kpis {
    pub equity: Option<f64>,
    pub pnl_day: Option<f64>,
    pub pnl_pct: Option<f64>,
    pub exposure: f64,
    pub running: usize,
    pub systems: usize,
    pub demo: bool,
}

/// Derives the strip from a snapshot (never invents numbers: nulls stay null).
pub fn kpis(snapshot: &Snapshot) -> Kpis {
    let account = snapshot.account.as_ref();
    let equity = account.and_then(|a| a.equity);
    let pnl_day = account.and_then(|a| a.pnl_day);
    let base = match (equity, pnl_day) {
        (Some(equity), Some(pnl)) if (equity - pnl).abs() > f64::EPSILON => {
            Some(pnl / (equity - pnl) * 100.0)
        }
        _ => None,
    };
    let exposure = snapshot
        .positions
        .iter()
        .map(|p| {
            p.size
                * p.current
                    .or(p.entry)
                    .filter(|v| v.is_finite())
                    .unwrap_or(0.0)
                    .abs()
        })
        .sum();
    Kpis {
        equity,
        pnl_day,
        pnl_pct: base,
        exposure,
        running: snapshot.systems.iter().filter(|s| s.running).count(),
        systems: snapshot.systems.len(),
        demo: account.is_some_and(|a| a.demo == Some(true) || a.id.starts_with("DU")),
    }
}

/// A system's display name: the pool's folder name without its `NN_` prefix and
/// with underscores turned into spaces (`06_nasdaq_pullback` → `Nasdaq pullback`).
/// The id keeps the number, so the name can be the readable part.
pub fn system_label(name: &str) -> String {
    let body = match name.split_once('_') {
        Some((prefix, rest)) if !rest.is_empty() && prefix.chars().all(|ch| ch.is_ascii_digit()) => {
            rest
        }
        _ => name,
    };
    let spaced = body.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => spaced,
    }
}

/// Positions sorted by symbol (stable, independent of the engine's order).
pub fn positions_sorted(snapshot: &Snapshot) -> Vec<&Position> {
    let mut positions: Vec<&Position> = snapshot.positions.iter().collect();
    positions.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    positions
}

/// Systems with the active one first, then running ones, then by id.
pub fn systems_sorted(snapshot: &Snapshot) -> Vec<&System> {
    let mut systems: Vec<&System> = snapshot.systems.iter().collect();
    systems.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then(b.running.cmp(&a.running))
            .then(a.id.cmp(&b.id))
    });
    systems
}

/// Pending tasks first, then by run time (soonest first).
pub fn tasks_sorted(snapshot: &Snapshot) -> Vec<&Task> {
    let mut tasks: Vec<&Task> = snapshot.tasks.iter().collect();
    tasks.sort_by(|a, b| {
        let pending = |t: &Task| t.status != TaskStatus::Pending;
        pending(a).cmp(&pending(b)).then(a.run_at.cmp(&b.run_at))
    });
    tasks
}

/// The desk's rules, newest first (a fired one stays where it was made).
pub fn rules_sorted(snapshot: &Snapshot) -> Vec<&Rule> {
    let mut rules: Vec<&Rule> = snapshot.rules.as_deref().unwrap_or_default().iter().collect();
    rules.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
    rules
}

/// The newest log rows first.
pub fn logs_newest(snapshot: &Snapshot, limit: usize) -> Vec<&Log> {
    let mut logs: Vec<&Log> = snapshot.logs.iter().collect();
    logs.sort_by(|a, b| b.ts.cmp(&a.ts));
    logs.truncate(limit);
    logs
}

/// Spanish number with thousands dots and a decimal comma: `10.085,30`.
pub fn number_es(value: f64, decimals: usize) -> String {
    if !value.is_finite() {
        return "—".into();
    }
    let body = format!("{:.*}", decimals, value.abs());
    let (integer, fraction) = body.split_once('.').unwrap_or((body.as_str(), ""));
    let mut grouped = String::with_capacity(integer.len() + integer.len() / 3 + 1);
    for (index, ch) in integer.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(ch);
    }
    let mut out: String = grouped.chars().rev().collect();
    if value < 0.0 {
        out.insert(0, '-');
    }
    if decimals > 0 {
        out.push(',');
        out.push_str(fraction);
    }
    out
}

/// `10.085,30 $`; an absent number renders as an em dash, never as zero.
pub fn money(value: Option<f64>) -> String {
    match value.filter(|v| v.is_finite()) {
        Some(value) => format!("{} $", number_es(value, 2)),
        None => "—".into(),
    }
}

/// `+128,50 $` / `-68,00 $` (an explicit sign for gains).
pub fn signed_money(value: Option<f64>) -> String {
    match value.filter(|v| v.is_finite()) {
        Some(value) if value > 0.0 => format!("+{} $", number_es(value, 2)),
        Some(value) => format!("{} $", number_es(value, 2)),
        None => "—".into(),
    }
}

/// `+0,15 %`.
pub fn signed_pct(value: Option<f64>) -> String {
    match value.filter(|v| v.is_finite()) {
        Some(value) if value > 0.0 => format!("+{} %", number_es(value, 2)),
        Some(value) => format!("{} %", number_es(value, 2)),
        None => "—".into(),
    }
}

/// `9.940,00` without the currency mark (for chips and tooltips).
pub fn plain(value: Option<f64>) -> String {
    value
        .filter(|v| v.is_finite())
        .map(|v| number_es(v, 2))
        .unwrap_or_else(|| "—".into())
}

/// Local wall-clock `HH:MM:SS` for an epoch-ms timestamp.
pub fn clock(ts: i64) -> String {
    Local
        .timestamp_millis_opt(ts)
        .single()
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".into())
}

/// Local `HH:MM` for an epoch-ms timestamp.
pub fn short_clock(ts: i64) -> String {
    Local
        .timestamp_millis_opt(ts)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".into())
}

/// `hace 2 min`; a timestamp in the future reads `ahora`.
pub fn relative(ts: i64, now: i64) -> String {
    let seconds = ((now - ts) as f64 / 1000.0).floor();
    if !seconds.is_finite() || seconds < 5.0 {
        return "ahora".into();
    }
    let seconds = seconds as i64;
    if seconds < 60 {
        format!("hace {seconds} s")
    } else if seconds < 3_600 {
        format!("hace {} min", seconds / 60)
    } else if seconds < 86_400 {
        format!("hace {} h", seconds / 3_600)
    } else {
        format!("hace {} d", seconds / 86_400)
    }
}

/// Normalized equity samples (0..=1) for the strip's bars, oldest first.
pub fn spark_bars(snapshot: &Snapshot, max_bars: usize) -> Vec<f32> {
    let points: Vec<f64> = snapshot
        .equity
        .iter()
        .flatten()
        .filter_map(|point| point.equity.filter(|v| v.is_finite()))
        .collect();
    if points.is_empty() || max_bars == 0 {
        return Vec::new();
    }
    let tail = &points[points.len().saturating_sub(max_bars)..];
    let min = tail.iter().copied().fold(f64::INFINITY, f64::min);
    let max = tail.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if (max - min).abs() <= f64::EPSILON {
        // A flat series is a line, not a block: every bar sits at the floor.
        return vec![0.0; tail.len()];
    }
    tail.iter()
        .map(|value| ((value - min) / (max - min)) as f32)
        .collect()
}

/// How long a answered proposal stays in the chat's inbox. Long enough to
/// read the verdict, short enough that the strip does not turn into a log.
pub const INBOX_ANSWERED_MS: i64 = 90_000;

/// The chat's proposal inbox: every pending request (oldest first, the queue
/// the person answers in order) followed by the freshest verdict, when it
/// landed within [`INBOX_ANSWERED_MS`].
pub fn inbox_proposals(snapshot: &Snapshot, now: i64) -> Vec<&TradingProposal> {
    let rows = snapshot.proposals.as_deref().unwrap_or_default();
    let mut pending: Vec<&TradingProposal> = rows
        .iter()
        .filter(|p| p.status == ProposalStatus::Pending)
        .collect();
    pending.sort_by_key(|p| p.created_at);
    let answered = rows
        .iter()
        .filter(|p| p.status != ProposalStatus::Pending)
        .filter(|p| {
            p.resolved_at
                .is_some_and(|at| at <= now && now - at <= INBOX_ANSWERED_MS)
        })
        .max_by_key(|p| p.resolved_at.unwrap_or(0));
    pending.into_iter().chain(answered).collect()
}

/// Human label for a log level (Spanish).
pub fn level_label(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Info => "info",
        LogLevel::Warning => "aviso",
        LogLevel::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use zeron_proto::trading::*;

    fn account(equity: Option<f64>, pnl_day: Option<f64>) -> Account {
        Account {
            id: "DU123456".into(),
            kind: "IBKR · demo".into(),
            cash: Some(10_000.0),
            equity,
            pnl_day,
            pnl_realized: None,
            available_funds: None,
            demo: None,
            buying_power: None,
            margin_used: None,
            excess_liquidity: None,
            updated_at: Some(0),
            link: Some("127.0.0.1:7497".into()),
        }
    }

    fn position(symbol: &str, size: f64, current: Option<f64>) -> Position {
        Position {
            symbol: symbol.into(),
            side: PositionSide::Long,
            size,
            entry: Some(100.0),
            current,
            pnl: Some(1.0),
            pnl_pct: Some(0.1),
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            version: "test".into(),
            connected: true,
            client_in_use: None,
            account: Some(account(Some(10_100.0), Some(100.0))),
            systems: vec![
                System {
                    id: "02_b".into(),
                    name: "B".into(),
                    running: false,
                    active: false,
                    pid: None,
                    started_at: None,
                    symbol: "NQ".into(),
                    timeframe: "M5".into(),
                    pnl_day: None,
                    capital: None,
                    pnl_pct: None,
                    client_id: None,
                    close_hour: None,
                    last_activity_at: None,
                    description: None,
                    rules: None::<BTreeMap<String, String>>,
                },
                System {
                    id: "01_a".into(),
                    name: "A".into(),
                    running: true,
                    active: true,
                    pid: Some(1),
                    started_at: Some(0),
                    symbol: "EURUSD".into(),
                    timeframe: "M15".into(),
                    pnl_day: Some(1.0),
                    capital: Some(10_000.0),
                    pnl_pct: Some(0.01),
                    client_id: None,
                    close_hour: None,
                    last_activity_at: None,
                    description: None,
                    rules: None,
                },
            ],
            positions: vec![
                position("NQ", 1.0, Some(200.0)),
                position("EURUSD", 0.5, None),
            ],
            open_positions: None,
            closable_positions: None,
            logs: vec![],
            tasks: vec![],
            rules: None,
            trades: None,
            equity: Some(vec![
                EquityPoint {
                    ts: 1,
                    equity: Some(10_000.0),
                    pnl_day: Some(0.0),
                },
                EquityPoint {
                    ts: 2,
                    equity: Some(10_050.0),
                    pnl_day: Some(50.0),
                },
            ]),
            accounts: None,
            links: None,
            systems_dir: None,
            programs: None,
            proposals: None,
        }
    }

    #[test]
    fn spanish_numbers_group_and_comma() {
        assert_eq!(number_es(1_234_567.891, 2), "1.234.567,89");
        assert_eq!(number_es(-42.5, 2), "-42,50");
        assert_eq!(number_es(999.0, 0), "999");
        assert_eq!(number_es(f64::NAN, 2), "—");
    }

    #[test]
    fn money_never_invents_zero() {
        assert_eq!(money(None), "—");
        assert_eq!(money(Some(10_085.3)), "10.085,30 $");
        assert_eq!(signed_money(Some(128.5)), "+128,50 $");
        assert_eq!(signed_money(Some(-68.0)), "-68,00 $");
        assert_eq!(signed_pct(Some(0.15)), "+0,15 %");
        assert_eq!(signed_pct(None), "—");
    }

    #[test]
    fn kpis_derive_exposure_and_demo() {
        let kpis = kpis(&snapshot());
        assert!(kpis.demo, "DU prefix means demo");
        assert_eq!(kpis.running, 1);
        assert_eq!(kpis.systems, 2);
        // 1.0 × 200 + 0.5 × 100 (entry fallback)
        assert_eq!(kpis.exposure, 250.0);
        assert!((kpis.pnl_pct.unwrap_or_default() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn system_labels_read_like_names_not_folders() {
        assert_eq!(system_label("06_nasdaq_pullback"), "Nasdaq pullback");
        assert_eq!(system_label("11_usdjpy_ruptura_bollinger"), "Usdjpy ruptura bollinger");
        // A name without the numeric prefix is left as it is.
        assert_eq!(system_label("A"), "A");
        assert_eq!(system_label("nasdaq"), "Nasdaq");
        assert_eq!(system_label(""), "");
    }

    #[test]
    fn orderings_put_attention_first() {
        let snapshot = snapshot();
        assert_eq!(systems_sorted(&snapshot)[0].id, "01_a");
        assert_eq!(positions_sorted(&snapshot)[0].symbol, "EURUSD");
    }

    #[test]
    fn rules_are_newest_first_and_tolerate_a_fixture_without_them() {
        let rule = |id: &str, created_at: i64| Rule {
            id: id.into(),
            name: id.into(),
            condition: RuleCondition::SystemStopped {
                system_id: "01_a".into(),
            },
            proposal: None,
            reason: "avisar".into(),
            status: RuleStatus::Watching,
            created_at,
            expires_at: None,
            fired_at: None,
            note: None,
        };
        let with_rules = Snapshot {
            rules: Some(vec![rule("old", 10), rule("new", 20)]),
            ..snapshot()
        };
        let ids: Vec<&str> = rules_sorted(&with_rules).iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["new", "old"]);
        assert!(rules_sorted(&snapshot()).is_empty());
    }

    #[test]
    fn spark_bars_normalize_the_tail() {
        let bars = spark_bars(&snapshot(), 48);
        assert_eq!(bars, vec![0.0, 1.0]);
        let flat = Snapshot {
            equity: Some(vec![EquityPoint {
                ts: 1,
                equity: Some(5.0),
                pnl_day: None,
            }]),
            ..snapshot()
        };
        assert_eq!(spark_bars(&flat, 48), vec![0.0]);
    }

    #[test]
    fn relative_time_speaks_spanish() {
        let now = 1_000_000_000;
        assert_eq!(relative(now, now), "ahora");
        assert_eq!(relative(now - 30_000, now), "hace 30 s");
        assert_eq!(relative(now - 120_000, now), "hace 2 min");
        assert_eq!(relative(now - 7_200_000, now), "hace 2 h");
        assert_eq!(relative(now - 172_800_000, now), "hace 2 d");
    }

    fn proposal(id: &str, status: ProposalStatus, created: i64, resolved: Option<i64>) -> TradingProposal {
        TradingProposal {
            id: id.into(),
            action: ProposalAction::CloseAll,
            reason: "probando".into(),
            chat_id: None,
            status,
            created_at: created,
            resolved_at: resolved,
            result: None,
        }
    }

    #[test]
    fn the_inbox_queues_pending_and_keeps_only_a_fresh_verdict() {
        let now = 10_000_000;
        let queue = Snapshot {
            proposals: Some(vec![
                proposal("b", ProposalStatus::Pending, 20, None),
                proposal("a", ProposalStatus::Pending, 10, None),
                proposal("vieja", ProposalStatus::Done, 5, Some(now - INBOX_ANSWERED_MS - 1)),
                proposal("nueva", ProposalStatus::Rejected, 30, Some(now - 5_000)),
            ]),
            ..snapshot()
        };
        let rows = inbox_proposals(&queue, now);
        let ids: Vec<&str> = rows.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "nueva"], "oldest pending first, one verdict");

        // Without a fresh answer (or before the clock catches up) only the queue shows.
        let waiters = Snapshot {
            proposals: Some(vec![
                proposal("a", ProposalStatus::Done, 1, Some(now + 60_000)),
            ]),
            ..snapshot()
        };
        assert!(inbox_proposals(&waiters, now).is_empty());
    }
}
