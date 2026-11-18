//! Durable close-claim logic and pure helpers ported from Openbot
//! `packages/core/src/trading.ts` (spec `SPECS/A4.md`).
//!
//! Self-contained: operates on a `&rusqlite::Connection` so it can compose
//! with the store layer later. The `trading_close` table (id PK, symbol,
//! lock, verdict, ts, timestamps) plus its unique index on `lock` must
//! exist in the database (NULL locks never collide in SQLite).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use rusqlite::{params, Connection};
// serde traits used via serde_json; no direct derives needed here.
use zeron_proto::trading::PositionCloseResult;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// A claim is considered "live" (still running) for this long.
pub const VIGENCIA_RECLAMO_MS: i64 = 5 * 60_000;

/// A recorded verdict is replayable for this long.
pub const VIGENCIA_VEREDICTO_MS: i64 = 24 * 60 * 60_000;

/// Order statuses that count as terminal.
pub static TERMINACIONES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut s = HashSet::new();
    s.insert("Filled");
    s.insert("Cancelled");
    s.insert("ApiCancelled");
    s.insert("Inactive");
    s
});

/// Default tick sizes per tracked family.
pub static TICKS_DEFECTO: LazyLock<HashMap<String, f64>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    m.insert("MNQ".to_string(), 0.25);
    m.insert("M2K".to_string(), 0.1);
    m.insert("MGC".to_string(), 0.1);
    m.insert("USDJPY".to_string(), 0.001);
    m
});

const FAMILIAS_RASTREO: [&str; 4] = ["MNQ", "M2K", "MGC", "USDJPY"];

// ---------------------------------------------------------------------------
// Reclamo (claim result)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Reclamo {
    Nueva,
    Repetida(PositionCloseResult),
    Vestigio(PositionCloseResult),
    Ajena(PositionCloseResult),
    Fallida(PositionCloseResult),
}

// ---------------------------------------------------------------------------
// Verdict constructors
// ---------------------------------------------------------------------------

pub fn vestigio(symbol: &str) -> PositionCloseResult {
    PositionCloseResult {
        ok: false,
        message: format!(
            "The previous close of {} left no verdict: its session died mid-cycle…",
            symbol
        ),
        message_key: Some("trading.close.vestigio".into()),
        message_params: None,
    }
}

pub fn interrumpido(symbol: &str) -> PositionCloseResult {
    PositionCloseResult {
        ok: false,
        message: format!("The close of {} was interrupted.", symbol),
        message_key: Some("trading.close.interrupted".into()),
        message_params: None,
    }
}

pub fn ocupado(_symbol: &str) -> PositionCloseResult {
    PositionCloseResult {
        ok: false,
        message: "A close is already running…".to_string(),
        message_key: Some("trading.close.busy".into()),
        message_params: None,
    }
}

pub fn db_caida(_symbol: &str) -> PositionCloseResult {
    PositionCloseResult {
        ok: false,
        message: "the database is unavailable, so the close was not sent".to_string(),
        message_key: Some("trading.close.unverified".into()),
        message_params: None,
    }
}

// ---------------------------------------------------------------------------
// Claim implementation
// ---------------------------------------------------------------------------

pub trait IntoLockKey {
    fn into_lock_key(self) -> Option<String>;
}

impl IntoLockKey for Option<String> {
    fn into_lock_key(self) -> Option<String> {
        self
    }
}

impl IntoLockKey for Option<&str> {
    fn into_lock_key(self) -> Option<String> {
        self.map(|s| s.to_string())
    }
}

impl IntoLockKey for String {
    fn into_lock_key(self) -> Option<String> {
        Some(self)
    }
}

impl IntoLockKey for &str {
    fn into_lock_key(self) -> Option<String> {
        Some(self.to_string())
    }
}

pub fn reclamar_cierre<L, N>(
    conn: &Connection,
    id: &str,
    symbol: &str,
    lock_key: L,
    now_ms: N,
) -> Reclamo
where
    L: IntoLockKey,
    N: Into<i64>,
{
    let now_ms: i64 = now_ms.into();
    let lock_key = lock_key.into_lock_key();
    let lk = lock_key.as_deref();
    match reclamar_inner(conn, id, symbol, lk, now_ms) {
        Ok(r) => r,
        Err(_) => Reclamo::Fallida(db_caida(symbol)),
    }
}

// Aliases for TS-flavoured names.
pub fn reclamar_cierre_db<L, N>(
    conn: &Connection,
    id: &str,
    symbol: &str,
    lock_key: L,
    now_ms: N,
) -> Reclamo
where
    L: IntoLockKey,
    N: Into<i64>,
{
    reclamar_cierre(conn, id, symbol, lock_key, now_ms)
}

fn reclamar_inner(
    conn: &Connection,
    id: &str,
    symbol: &str,
    lock_key: Option<&str>,
    now_ms: i64,
) -> Result<Reclamo, rusqlite::Error> {
    for _pass in 0..2 {
        // 1. read own row by id
        let own = conn.query_row(
            "SELECT lock, verdict, ts FROM trading_close WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        );
        match own {
            Ok((_lock, Some(verdict), _ts)) => {
                match serde_json::from_str::<PositionCloseResult>(&verdict) {
                    Ok(parsed) => return Ok(Reclamo::Repetida(parsed)),
                    Err(_) => return Ok(Reclamo::Fallida(db_caida(symbol))),
                }
            }
            Ok((_lock, None, ts)) => {
                if now_ms - ts < VIGENCIA_RECLAMO_MS {
                    return Ok(Reclamo::Vestigio(ocupado(symbol)));
                }
                // old open claim: convert to vestigio, free lock, ts=now
                let v = vestigio(symbol);
                let vj = serde_json::to_string(&v).unwrap_or_default();
                conn.execute(
                    "UPDATE trading_close SET verdict = ?1, lock = NULL, ts = ?2, time_updated = ?2 WHERE id = ?3",
                    params![vj, now_ms, id],
                )?;
                return Ok(Reclamo::Vestigio(v));
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(e) => return Err(e),
        }

        // 2. insert
        let inserted = insert_row(conn, id, symbol, lock_key, now_ms)?;
        if inserted {
            return Ok(Reclamo::Nueva);
        }

        // 3. not inserted: read holder by lock
        if let Some(lk) = lock_key {
            let holder = conn.query_row(
                "SELECT id, symbol, ts FROM trading_close WHERE lock = ?1",
                [lk],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            );
            match holder {
                Ok((holder_id, holder_symbol, holder_ts)) => {
                    if now_ms - holder_ts < VIGENCIA_RECLAMO_MS {
                        return Ok(Reclamo::Ajena(ocupado(symbol)));
                    }
                    // absorb old holder
                    let v = vestigio(&holder_symbol);
                    let vj = serde_json::to_string(&v).unwrap_or_default();
                    conn.execute(
                        "UPDATE trading_close SET verdict = ?1, lock = NULL, ts = ?2, time_updated = ?2 WHERE id = ?3",
                        params![vj, now_ms, holder_id],
                    )?;
                    // retry loop
                    continue;
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    continue;
                }
                Err(e) => return Err(e),
            }
        } else {
            continue;
        }
    }
    Ok(Reclamo::Ajena(ocupado(symbol)))
}

fn insert_row(
    conn: &Connection,
    id: &str,
    symbol: &str,
    lock_key: Option<&str>,
    now_ms: i64,
) -> Result<bool, rusqlite::Error> {
    let res = conn.query_row(
        "INSERT INTO trading_close (id, symbol, lock, verdict, ts, time_created, time_updated) \
         VALUES (?1, ?2, ?3, NULL, ?4, ?4, ?4) \
         ON CONFLICT DO NOTHING RETURNING id",
        params![id, symbol, lock_key, now_ms],
        |r| r.get::<_, String>(0),
    );
    match res {
        Ok(_) => Ok(true),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Finalizer
// ---------------------------------------------------------------------------

pub trait IntoVerdictJson {
    fn into_verdict_json(self) -> String;
}

impl IntoVerdictJson for PositionCloseResult {
    fn into_verdict_json(self) -> String {
        serde_json::to_string(&self).unwrap_or_default()
    }
}

impl IntoVerdictJson for &PositionCloseResult {
    fn into_verdict_json(self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

impl IntoVerdictJson for String {
    fn into_verdict_json(self) -> String {
        self
    }
}

impl IntoVerdictJson for &str {
    fn into_verdict_json(self) -> String {
        self.to_string()
    }
}

impl IntoVerdictJson for serde_json::Value {
    fn into_verdict_json(self) -> String {
        self.to_string()
    }
}

pub fn finalizar_cierre<V, N>(conn: &Connection, id: &str, now_ms: N, verdict: V) -> Result<(), rusqlite::Error>
where
    V: IntoVerdictJson,
    N: Into<i64>,
{
    let now_ms: i64 = now_ms.into();
    let vj = verdict.into_verdict_json();
    let _ = conn.execute(
        "UPDATE trading_close SET verdict = ?1, lock = NULL, ts = ?2, time_updated = ?2 WHERE id = ?3",
        params![vj, now_ms, id],
    );
    Ok(())
}

pub fn finalizar_cierre_db<V, N>(conn: &Connection, id: &str, now_ms: N, verdict: V) -> Result<(), rusqlite::Error>
where
    V: IntoVerdictJson,
    N: Into<i64>,
{
    finalizar_cierre(conn, id, now_ms, verdict)
}

// ---------------------------------------------------------------------------
// Tick helpers
// ---------------------------------------------------------------------------

pub fn familia_de<S>(symbol: S) -> String
where
    S: AsRef<str>,
{
    let symbol = symbol.as_ref();
    let up = symbol.to_uppercase();
    for f in FAMILIAS_RASTREO.iter() {
        if up.starts_with(f) {
            return (*f).to_string();
        }
    }
    up
}

pub fn tick_de_contrato<S>(known: Option<f64>, symbol: S) -> f64
where
    S: AsRef<str>,
{
    let symbol = symbol.as_ref();
    if let Some(t) = known {
        if t.is_finite() && t > 0.0 {
            return t;
        }
    }
    let fam = familia_de(symbol);
    if let Some(t) = TICKS_DEFECTO.get(&fam) {
        if *t > 0.0 {
            return *t;
        }
    }
    *TICKS_DEFECTO.get("MNQ").unwrap_or(&0.25)
}

pub fn en_rejilla<P, T>(precio: P, tick: T) -> f64
where
    P: Into<f64>,
    T: Into<f64>,
{
    let precio: f64 = precio.into();
    let tick: f64 = tick.into();
    if !tick.is_finite() || tick == 0.0 {
        return precio;
    }
    let r = (precio / tick).round() * tick;
    let s = format!("{:.9}", r);
    s.parse::<f64>().unwrap_or(r)
}

pub fn texto_precio<P, T>(precio: P, tick: T) -> String
where
    P: Into<f64>,
    T: Into<f64>,
{
    let precio: f64 = precio.into();
    let tick: f64 = tick.into();
    let tick_str = tick.to_string();
    let decimals = tick_str
        .split('.')
        .nth(1)
        .map(|d| d.len())
        .unwrap_or(0)
        .min(6);
    format!("{:.*}", decimals, precio)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE trading_close (
                id text PRIMARY KEY,
                symbol text NOT NULL,
                lock text,
                verdict text,
                ts integer NOT NULL,
                time_created integer,
                time_updated integer
            );
            CREATE UNIQUE INDEX trading_close_lock_idx ON trading_close(lock);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn nueva_inserts() {
        let conn = setup();
        let r = reclamar_cierre(&conn, "cierre:1", "MNQ", Some("MNQ".to_string()), 1000);
        assert_eq!(r, Reclamo::Nueva);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM trading_close WHERE id='cierre:1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn repetida_returns_verdict() {
        let conn = setup();
        let v = vestigio("MNQ");
        let vj = serde_json::to_string(&v).unwrap();
        conn.execute(
            "INSERT INTO trading_close (id,symbol,lock,verdict,ts,time_created,time_updated) VALUES ('cierre:2','MNQ','MNQ',?1,1000,1000,1000)",
            params![vj],
        )
        .unwrap();
        let r = reclamar_cierre(&conn, "cierre:2", "MNQ", Some("MNQ".to_string()), 1001);
        match r {
            Reclamo::Repetida(p) => assert_eq!(p, v),
            _ => panic!("expected Repetida"),
        }
    }

    #[test]
    fn fresh_own_open_is_vestigio_ocupado() {
        let conn = setup();
        conn.execute(
            "INSERT INTO trading_close (id,symbol,lock,verdict,ts,time_created,time_updated) VALUES ('cierre:3','MNQ','MNQ',NULL,1000,1000,1000)",
            [],
        )
        .unwrap();
        let r = reclamar_cierre(&conn, "cierre:3", "MNQ", Some("MNQ".to_string()), 1000 + 60_000);
        match r {
            Reclamo::Vestigio(p) => assert_eq!(p, ocupado("MNQ")),
            _ => panic!("expected Vestigio"),
        }
        // ts unchanged
        let ts: i64 = conn
            .query_row("SELECT ts FROM trading_close WHERE id='cierre:3'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ts, 1000);
    }

    #[test]
    fn old_own_open_converts_to_vestigio() {
        let conn = setup();
        conn.execute(
            "INSERT INTO trading_close (id,symbol,lock,verdict,ts,time_created,time_updated) VALUES ('cierre:4','MNQ','MNQ',NULL,1000,1000,1000)",
            [],
        )
        .unwrap();
        let now = 1000 + 6 * 60_000;
        let r = reclamar_cierre(&conn, "cierre:4", "MNQ", Some("MNQ".to_string()), now);
        match r {
            Reclamo::Vestigio(p) => assert_eq!(p, vestigio("MNQ")),
            _ => panic!("expected Vestigio"),
        }
        let (lock, verdict, ts): (Option<String>, Option<String>, i64) = conn
            .query_row(
                "SELECT lock, verdict, ts FROM trading_close WHERE id='cierre:4'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(lock.is_none());
        assert!(verdict.is_some());
        assert_eq!(ts, now);
    }

    #[test]
    fn foreign_fresh_lock_is_ajena() {
        let conn = setup();
        conn.execute(
            "INSERT INTO trading_close (id,symbol,lock,verdict,ts,time_created,time_updated) VALUES ('other','MNQ','MNQ',NULL,1000,1000,1000)",
            [],
        )
        .unwrap();
        let r = reclamar_cierre(&conn, "cierre:5", "MNQ", Some("MNQ".to_string()), 1000 + 60_000);
        match r {
            Reclamo::Ajena(p) => assert_eq!(p, ocupado("MNQ")),
            _ => panic!("expected Ajena"),
        }
    }

    #[test]
    fn foreign_old_lock_absorbed_then_nueva() {
        let conn = setup();
        conn.execute(
            "INSERT INTO trading_close (id,symbol,lock,verdict,ts,time_created,time_updated) VALUES ('other','MNQ','MNQ',NULL,1000,1000,1000)",
            [],
        )
        .unwrap();
        let now = 1000 + 6 * 60_000;
        let r = reclamar_cierre(&conn, "cierre:6", "MNQ", Some("MNQ".to_string()), now);
        assert_eq!(r, Reclamo::Nueva);
        // holder absorbed
        let (lock, verdict): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT lock, verdict FROM trading_close WHERE id='other'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(lock.is_none());
        assert!(verdict.is_some());
    }

    #[test]
    fn mass_row_null_lock() {
        let conn = setup();
        // mass row: symbol "", lock null
        let r1 = reclamar_cierre(&conn, "cierre-todo:1", "", None::<String>, 1000);
        assert_eq!(r1, Reclamo::Nueva);
        // fresh mass claim same id -> busy
        let r2 = reclamar_cierre(&conn, "cierre-todo:1", "", None::<String>, 1000 + 60_000);
        match r2 {
            Reclamo::Vestigio(p) => assert_eq!(p, ocupado("")),
            _ => panic!("expected Vestigio, got {:?}", r2),
        }
        // another distinct mass id can insert (null lock doesn't collide)
        let r3 = reclamar_cierre(&conn, "cierre-todo:2", "", None::<String>, 1000);
        assert_eq!(r3, Reclamo::Nueva);
    }

    #[test]
    fn finalizar_writes_verdict() {
        let conn = setup();
        conn.execute(
            "INSERT INTO trading_close (id,symbol,lock,verdict,ts,time_created,time_updated) VALUES ('cierre:7','MNQ','MNQ',NULL,1000,1000,1000)",
            [],
        )
        .unwrap();
        let v = interrumpido("MNQ");
        finalizar_cierre(&conn, "cierre:7", 2000, &v).unwrap();
        let (lock, verdict, ts): (Option<String>, Option<String>, i64) = conn
            .query_row(
                "SELECT lock, verdict, ts FROM trading_close WHERE id='cierre:7'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert!(lock.is_none());
        assert_eq!(ts, 2000);
        let parsed: PositionCloseResult = serde_json::from_str(&verdict.unwrap()).unwrap();
        assert_eq!(parsed, v);
    }

    #[test]
    fn tick_helpers_defaults() {
        assert_eq!(*TICKS_DEFECTO.get("MNQ").unwrap(), 0.25);
        assert_eq!(familia_de("MNQM5"), "MNQ");
        assert_eq!(tick_de_contrato(None, "MNQ"), 0.25);
        assert_eq!(tick_de_contrato(Some(0.5), "M2K"), 0.5);
        assert_eq!(en_rejilla(100.1234, 0.25), 100.0); // round to 0.25? 100.1234/0.25=400.4936 round 400 ->100.0
        assert_eq!(en_rejilla(100.1, 0.1), 100.1);
        assert_eq!(texto_precio(100.123456, 0.25), "100.12");
        assert_eq!(texto_precio(100.1, 0.001), "100.100");
    }

    #[test]
    fn terminaciones_contains() {
        assert!(TERMINACIONES.contains("Filled"));
        assert!(TERMINACIONES.contains("Cancelled"));
        assert!(!TERMINACIONES.contains("Pending"));
    }
}
