//! Watch rules: pure condition checks over a desk snapshot, plus their SQLite
//! rows. A rule never runs an action; when its condition holds the service
//! creates a proposal (or only marks the rule), and the person decides.

use rusqlite::{params, Connection};
use zeron_proto::trading::{Rule, RuleCondition, RuleStatus, Snapshot};

/// A rule watches for a day unless the person says otherwise.
pub const DEFAULT_EXPIRES_MINUTES: f64 = 24.0 * 60.0;
/// A month is the longest a rule may watch.
pub const MAX_EXPIRES_MINUTES: f64 = 30.0 * 24.0 * 60.0;

fn status_str(status: RuleStatus) -> &'static str {
    match status {
        RuleStatus::Watching => "watching",
        RuleStatus::Fired => "fired",
        RuleStatus::Cancelled => "cancelled",
        RuleStatus::Expired => "expired",
    }
}

fn status_from_str(text: &str) -> RuleStatus {
    match text {
        "fired" => RuleStatus::Fired,
        "cancelled" => RuleStatus::Cancelled,
        "expired" => RuleStatus::Expired,
        _ => RuleStatus::Watching,
    }
}

fn es_signed_pct(value: f64) -> String {
    format!("{value:+.2}").replace('.', ",")
}

fn es_signed_money(value: f64) -> String {
    format!("{value:+.2}").replace('.', ",")
}

/// A threshold pair needs at least one side and, when both are present, the
/// floor cannot be above the ceiling.
fn check_bounds(at_most: Option<f64>, at_least: Option<f64>) -> Result<(), String> {
    if at_most.is_none() && at_least.is_none() {
        return Err("una regla necesita al menos un umbral".into());
    }
    for value in [at_most, at_least].into_iter().flatten() {
        if !value.is_finite() {
            return Err("el umbral debe ser un número".into());
        }
    }
    if let (Some(most), Some(least)) = (at_most, at_least)
        && least > most
    {
        return Err("el umbral mínimo no puede superar al máximo".into());
    }
    Ok(())
}

/// Whether a rule's condition can be watched at all.
pub fn validate(condition: &RuleCondition) -> Result<(), String> {
    match condition {
        RuleCondition::SystemPnlPct {
            at_most, at_least, ..
        }
        | RuleCondition::SystemPnlUsd {
            at_most, at_least, ..
        }
        | RuleCondition::AccountPnlPct { at_most, at_least }
        | RuleCondition::PositionPnlUsd {
            at_most, at_least, ..
        } => check_bounds(*at_most, *at_least),
        RuleCondition::SystemIdle { minutes, .. } => {
            if *minutes == 0 {
                return Err("los minutos sin actividad deben ser mayores que cero".into());
            }
            Ok(())
        }
        RuleCondition::SystemStopped { .. } => Ok(()),
    }
}

/// How many rules may watch at once.
pub const MAX_WATCHING: usize = 30;

/// Whether the condition reads the broker half (account, positions) rather
/// than the systems pool. Each half is only trusted when it is real: never
/// against the demo fixture, whose made-up figures must not fire anything.
pub fn reads_broker(condition: &RuleCondition) -> bool {
    matches!(
        condition,
        RuleCondition::AccountPnlPct { .. } | RuleCondition::PositionPnlUsd { .. }
    )
}

/// The value falls inside the thresholds the rule declared.
fn within(value: f64, at_most: Option<f64>, at_least: Option<f64>) -> bool {
    at_most.is_none_or(|most| value <= most) && at_least.is_none_or(|least| value >= least)
}

/// The system's display name, or its id when the snapshot no longer has it.
fn system_name<'a>(snapshot: &'a Snapshot, id: &'a str) -> &'a str {
    snapshot
        .systems
        .iter()
        .find(|system| system.id == id)
        .map(|system| system.name.as_str())
        .unwrap_or(id)
}

/// The day's P&L as a percent of the account's opening equity.
fn account_pnl_pct(snapshot: &Snapshot) -> Option<f64> {
    let account = snapshot.account.as_ref()?;
    let pnl = account.pnl_day?;
    let equity = account.equity?;
    let opening = equity - pnl;
    if opening <= 0.0 {
        return None;
    }
    Some(pnl / opening * 100.0)
}

/// If the condition holds, a line saying what the desk observed; otherwise
/// `None` (the rule keeps watching).
pub fn condition_met(condition: &RuleCondition, snapshot: &Snapshot, now_ms: i64) -> Option<String> {
    match condition {
        RuleCondition::SystemPnlPct {
            system_id,
            at_most,
            at_least,
        } => {
            let system = snapshot
                .systems
                .iter()
                .find(|system| &system.id == system_id)?;
            let pct = system.pnl_pct?;
            within(pct, *at_most, *at_least).then(|| {
                format!(
                    "{} va {} % hoy",
                    system_name(snapshot, system_id),
                    es_signed_pct(pct)
                )
            })
        }
        RuleCondition::SystemPnlUsd {
            system_id,
            at_most,
            at_least,
        } => {
            let system = snapshot
                .systems
                .iter()
                .find(|system| &system.id == system_id)?;
            let pnl = system.pnl_day?;
            within(pnl, *at_most, *at_least).then(|| {
                format!(
                    "{} va {} $ hoy",
                    system_name(snapshot, system_id),
                    es_signed_money(pnl)
                )
            })
        }
        RuleCondition::SystemStopped { system_id } => {
            let system = snapshot
                .systems
                .iter()
                .find(|system| &system.id == system_id)?;
            (!system.running).then(|| format!("{} está parado", system.name))
        }
        RuleCondition::SystemIdle { system_id, minutes } => {
            let system = snapshot
                .systems
                .iter()
                .find(|system| &system.id == system_id)?;
            let quiet = system
                .last_activity_at
                .map(|last| now_ms.saturating_sub(last))
                // Never seen writing means quiet since the desk started.
                .unwrap_or(i64::MAX);
            (quiet >= (*minutes as i64).saturating_mul(60_000)).then(|| {
                let shown = if quiet == i64::MAX {
                    format!("{minutes} min o más")
                } else {
                    format!("{} min", quiet / 60_000)
                };
                format!("{} lleva {shown} sin actividad", system.name)
            })
        }
        RuleCondition::AccountPnlPct { at_most, at_least } => {
            let pct = account_pnl_pct(snapshot)?;
            within(pct, *at_most, *at_least)
                .then(|| format!("la cuenta va {} % hoy", es_signed_pct(pct)))
        }
        RuleCondition::PositionPnlUsd {
            symbol,
            at_most,
            at_least,
        } => {
            let position = snapshot
                .positions
                .iter()
                .find(|position| position.symbol == *symbol)?;
            let pnl = position.pnl?;
            within(pnl, *at_most, *at_least).then(|| {
                format!(
                    "la posición de {symbol} va {} $",
                    es_signed_money(pnl)
                )
            })
        }
    }
}

/// Every rule, newest first.
pub fn list(conn: &Connection) -> rusqlite::Result<Vec<Rule>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, condition, proposal, reason, status, created_at, \
         expires_at, fired_at, note FROM trading_rule \
         ORDER BY created_at DESC, id DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        let condition: String = row.get(2)?;
        let proposal: Option<String> = row.get(3)?;
        let condition = serde_json::from_str(&condition).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        let proposal = proposal
            .map(|text| {
                serde_json::from_str(&text).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            })
            .transpose()?;
        let status: String = row.get(5)?;
        Ok(Rule {
            id: row.get(0)?,
            name: row.get(1)?,
            condition,
            proposal,
            reason: row.get(4)?,
            status: status_from_str(&status),
            created_at: row.get(6)?,
            expires_at: row.get(7)?,
            fired_at: row.get(8)?,
            note: row.get(9)?,
        })
    })?;
    rows.collect()
}

/// Inserts a new rule or replaces the stored one with the same id.
pub fn upsert(conn: &Connection, rule: &Rule) -> rusqlite::Result<()> {
    let condition = serde_json::to_string(&rule.condition)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    let proposal = rule
        .proposal
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    conn.execute(
        "INSERT INTO trading_rule \
         (id, name, condition, proposal, reason, status, created_at, expires_at, fired_at, note, time_created, time_updated) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?7, ?7) \
         ON CONFLICT(id) DO UPDATE SET name=?2, condition=?3, proposal=?4, reason=?5, \
         status=?6, expires_at=?8, fired_at=?9, note=?10, time_updated=?7",
        params![
            rule.id,
            rule.name,
            condition,
            proposal,
            rule.reason,
            status_str(rule.status),
            rule.created_at,
            rule.expires_at,
            rule.fired_at,
            rule.note,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::trading::{Account, Position, PositionSide, System};

    fn system(id: &str, running: bool, pnl_pct: Option<f64>) -> System {
        System {
            id: id.into(),
            name: format!("Sistema {id}"),
            running,
            active: running,
            pid: None,
            started_at: None,
            symbol: "EURUSD".into(),
            timeframe: "M15".into(),
            pnl_day: pnl_pct.map(|pct| pct * 100.0),
            capital: Some(10_000.0),
            pnl_pct,
            client_id: None,
            close_hour: None,
            last_activity_at: Some(1_000_000),
            description: None,
            rules: None,
        }
    }

    fn snapshot(systems: Vec<System>) -> Snapshot {
        Snapshot {
            version: "test".into(),
            connected: true,
            client_in_use: None,
            account: Some(Account {
                id: "A".into(),
                kind: "demo".into(),
                cash: None,
                equity: Some(10_100.0),
                pnl_day: Some(100.0),
                pnl_realized: None,
                available_funds: None,
                demo: Some(true),
                buying_power: None,
                margin_used: None,
                excess_liquidity: None,
                updated_at: None,
                link: None,
            }),
            systems,
            positions: vec![Position {
                symbol: "NQ".into(),
                side: PositionSide::Long,
                size: 1.0,
                entry: None,
                current: None,
                pnl: Some(-75.0),
                pnl_pct: None,
            }],
            open_positions: None,
            closable_positions: None,
            logs: Vec::new(),
            tasks: Vec::new(),
            rules: None,
            trades: None,
            equity: None,
            accounts: None,
            links: None,
            systems_dir: None,
            programs: None,
            proposals: None,
        }
    }

    #[test]
    fn percent_and_money_bounds_fire_only_when_they_hold() {
        let snap = snapshot(vec![system("01", true, Some(-2.5))]);
        let now = 1_000_000;
        assert_eq!(
            condition_met(
                &RuleCondition::SystemPnlPct {
                    system_id: "01".into(),
                    at_most: Some(-2.0),
                    at_least: None,
                },
                &snap,
                now
            )
            .as_deref(),
            Some("Sistema 01 va -2,50 % hoy")
        );
        assert!(
            condition_met(
                &RuleCondition::SystemPnlPct {
                    system_id: "01".into(),
                    at_most: Some(-3.0),
                    at_least: None,
                },
                &snap,
                now
            )
            .is_none()
        );
        assert!(
            condition_met(
                &RuleCondition::SystemPnlUsd {
                    system_id: "01".into(),
                    at_most: None,
                    at_least: Some(0.0),
                },
                &snap,
                now
            )
            .is_none(),
            "a losing system never reaches a positive floor"
        );
        assert!(
            condition_met(
                &RuleCondition::SystemPnlPct {
                    system_id: "99".into(),
                    at_most: Some(-1.0),
                    at_least: None,
                },
                &snap,
                now
            )
            .is_none(),
            "an unknown system does not fire"
        );
    }

    #[test]
    fn stopped_idle_account_and_position_conditions_read_the_snapshot() {
        let snap = snapshot(vec![system("01", false, Some(0.0))]);
        let now = 1_000_000 + 31 * 60_000;
        assert_eq!(
            condition_met(
                &RuleCondition::SystemStopped {
                    system_id: "01".into()
                },
                &snap,
                now
            )
            .as_deref(),
            Some("Sistema 01 está parado")
        );
        assert_eq!(
            condition_met(
                &RuleCondition::SystemIdle {
                    system_id: "01".into(),
                    minutes: 30,
                },
                &snap,
                now
            )
            .as_deref(),
            Some("Sistema 01 lleva 31 min sin actividad")
        );
        assert_eq!(
            condition_met(
                &RuleCondition::AccountPnlPct {
                    at_most: None,
                    at_least: Some(0.9),
                },
                &snap,
                now
            )
            .as_deref(),
            Some("la cuenta va +1,00 % hoy")
        );
        assert_eq!(
            condition_met(
                &RuleCondition::PositionPnlUsd {
                    symbol: "NQ".into(),
                    at_most: Some(-50.0),
                    at_least: None,
                },
                &snap,
                now
            )
            .as_deref(),
            Some("la posición de NQ va -75,00 $")
        );
    }

    #[test]
    fn validation_rejects_empty_inverted_and_non_finite_bounds() {
        let pct = |most, least| RuleCondition::SystemPnlPct {
            system_id: "01".into(),
            at_most: most,
            at_least: least,
        };
        assert!(validate(&pct(None, None)).is_err());
        assert!(validate(&pct(Some(-1.0), Some(1.0))).is_err());
        assert!(validate(&pct(Some(f64::NAN), None)).is_err());
        assert!(validate(&pct(Some(-1.0), Some(-2.0))).is_ok());
        assert!(
            validate(&RuleCondition::SystemIdle {
                system_id: "01".into(),
                minutes: 0,
            })
            .is_err()
        );
    }

    #[test]
    fn rules_roundtrip_through_the_store() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::MIGRATION_SQL).unwrap();
        let rule = Rule {
            id: "rul_1".into(),
            name: "EURUSD en rojo".into(),
            condition: RuleCondition::SystemPnlPct {
                system_id: "01".into(),
                at_most: Some(-2.0),
                at_least: None,
            },
            proposal: Some(zeron_proto::trading::ProposalAction::StopSystems {
                system_ids: vec!["01".into()],
            }),
            reason: "Parar si pierde 2 %".into(),
            status: RuleStatus::Fired,
            created_at: 10,
            expires_at: Some(20),
            fired_at: Some(15),
            note: Some("Sistema 01 va -2,50 % hoy".into()),
        };
        upsert(&conn, &rule).unwrap();
        let back = list(&conn).unwrap();
        assert_eq!(back, vec![rule.clone()]);

        let mut cancelled = rule;
        cancelled.status = RuleStatus::Cancelled;
        upsert(&conn, &cancelled).unwrap();
        assert_eq!(list(&conn).unwrap(), vec![cancelled]);
    }
}
