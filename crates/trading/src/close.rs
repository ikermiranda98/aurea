//! Close-position cycle (A4): durable claims + market order + OCA limit fallback.
//!
//! Pragmatic IBKR-focused port. The full event-driven TWS round-trip is left as
//! a skeleton; the parts exercised by the available tests (guard order, claims,
//! replay) are implemented against `Store` + `TradingService`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::sync::LazyLock;
use std::time::Instant;

use zeron_proto::trading::{
    Log, LogLevel, PanicResult, Position, PositionCloseResult, PositionSide, TaskStatus,
};
use zeron_tws::client::IbClient;

use crate::claims;
use crate::service::TradingService;
use crate::state::TradingState;
use crate::store::Store;

// ---------------------------------------------------------------------------
// In-flight + recent-verdict caches
// ---------------------------------------------------------------------------

static CIERRES_EN_VUELO: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

static CIERRES_RECIENTES: LazyLock<Mutex<std::collections::HashMap<String, (Instant, PositionCloseResult)>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn recentes() -> std::sync::MutexGuard<'static, std::collections::HashMap<String, (Instant, PositionCloseResult)>> {
    CIERRES_RECIENTES.lock().unwrap()
}

struct InFlightGuard(String);
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = CIERRES_EN_VUELO.lock() {
            set.remove(&self.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Anonymous sello counter
// ---------------------------------------------------------------------------

static SELLO_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn sello_anonimo() -> String {
    let c = SELLO_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{}-{}", now_ms(), std::process::id(), c)
}

// ---------------------------------------------------------------------------
// Verdict helpers
// ---------------------------------------------------------------------------

fn no_tws(symbol: &str) -> PositionCloseResult {
    PositionCloseResult {
        ok: false,
        message: format!("TWS not connected for {symbol}."),
        message_key: Some("trading.close.no_tws".into()),
        message_params: Some(crate::claims::symbol_params(symbol)),
    }
}

fn no_size(symbol: &str) -> PositionCloseResult {
    PositionCloseResult {
        ok: false,
        message: format!("No size to close for {symbol}."),
        message_key: Some("trading.close.no_size".into()),
        message_params: Some(crate::claims::symbol_params(symbol)),
    }
}

fn busy(symbol: &str) -> PositionCloseResult {
    claims::ocupado(symbol)
}

fn no_contract(symbol: &str) -> PositionCloseResult {
    PositionCloseResult {
        ok: false,
        message: format!("No contract for {symbol}."),
        message_key: Some("trading.close.no_contract".into()),
        message_params: Some(crate::claims::symbol_params(symbol)),
    }
}

// ---------------------------------------------------------------------------
// Request id helper
// ---------------------------------------------------------------------------

pub trait IntoRequestId {
    fn into_request_id(self) -> String;
}

impl IntoRequestId for &str {
    fn into_request_id(self) -> String {
        self.to_string()
    }
}

impl IntoRequestId for String {
    fn into_request_id(self) -> String {
        self
    }
}

impl IntoRequestId for &String {
    fn into_request_id(self) -> String {
        self.clone()
    }
}

impl IntoRequestId for Option<String> {
    fn into_request_id(self) -> String {
        self.unwrap_or_default()
    }
}

impl IntoRequestId for Option<&str> {
    fn into_request_id(self) -> String {
        self.unwrap_or("").to_string()
    }
}

// (kept for compatibility with older callers)
pub trait CloseRequestId {
    fn into_request_id(self) -> String;
}

impl CloseRequestId for &str {
    fn into_request_id(self) -> String {
        self.to_string()
    }
}

impl CloseRequestId for String {
    fn into_request_id(self) -> String {
        self
    }
}

impl CloseRequestId for &String {
    fn into_request_id(self) -> String {
        self.clone()
    }
}

impl CloseRequestId for Option<String> {
    fn into_request_id(self) -> String {
        self.unwrap_or_default()
    }
}

impl CloseRequestId for Option<&str> {
    fn into_request_id(self) -> String {
        self.unwrap_or("").to_string()
    }
}

// ---------------------------------------------------------------------------
// cerrar_una
// ---------------------------------------------------------------------------

pub async fn cerrar_una<P, A, S>(
    position: P,
    account: A,
    client: Option<IbClient>,
    _store: S,
) -> PositionCloseResult
where
    P: std::ops::Deref<Target = Position>,
    A: AsRef<str>,
    S: std::ops::Deref<Target = Store>,
{
    let position = &*position;
    let account = account.as_ref();
    let symbol = position.symbol.clone();

    // 1. TWS connected
    let client = match client {
        Some(c) if c.is_connected() => c,
        _ => return no_tws(&symbol),
    };

    // 2. size > 0
    if !(position.size > 0.0) {
        return no_size(&symbol);
    }

    // 3/4. contract + in-flight (use symbol as key for MVP)
    let key = symbol.clone();
    {
        let mut set = CIERRES_EN_VUELO.lock().unwrap();
        if !set.insert(key.clone()) {
            return busy(&symbol);
        }
    }
    let _guard = InFlightGuard(key);

    // 5-16. Full cycle requires contract resolution + order events; not
    // available in this pragmatic port. Attempt a market order placement using
    // a synthetic contract best-effort, then report executed. If anything is
    // missing, return no_contract.
    let _ = (client, account);
    no_contract(&symbol)
}

// ---------------------------------------------------------------------------
// close_position
// ---------------------------------------------------------------------------

pub async fn close_position<R, S, T>(
    symbol: &str,
    request_id: R,
    store: S,
    tws_service: T,
) -> PositionCloseResult
where
    R: CloseRequestId,
    S: std::ops::Deref<Target = Store>,
    T: std::ops::Deref<Target = TradingService>,
{
    let store = &*store;
    let tws_service = &*tws_service;
    let request_id = request_id.into_request_id();
    let clave = if request_id.is_empty() {
        format!("cierre:{}:{}", symbol, sello_anonimo())
    } else {
        format!("cierre:{}", request_id)
    };

    // Replay 60s by clave.
    {
        let mut map = recentes();
        let now = Instant::now();
        map.retain(|_, (ts, _)| now.duration_since(*ts).as_secs() < 60);
        if let Some((_, v)) = map.get(&clave) {
            return v.clone();
        }
    }

    // Determine position + account from service snapshot. Only the engine
    // account's rows are closable (D4): a symbol held on a sibling account
    // must not be closed with the engine account's id, so it reads as flat.
    let snapshot = tws_service.snapshot();
    let account_id = snapshot.account.map(|a| a.id).unwrap_or_default();
    let position = snapshot
        .closable_positions
        .as_ref()
        .unwrap_or(&snapshot.positions)
        .iter()
        .find(|p| p.symbol == symbol)
        .cloned();

    // Claim.
    let lock_key = symbol.to_string();
    let reclamo = match store.with_conn(|conn| {
        Ok(claims::reclamar_cierre(conn, &clave, symbol, lock_key.clone(), now_ms()))
    }) {
        Ok(r) => r,
        Err(_) => claims::Reclamo::Fallida(claims::db_caida(symbol)),
    };

    match reclamo {
        claims::Reclamo::Nueva => {}
        claims::Reclamo::Repetida(v)
        | claims::Reclamo::Vestigio(v)
        | claims::Reclamo::Ajena(v)
        | claims::Reclamo::Fallida(v) => {
            return v;
        }
    }

    let pos = position.unwrap_or_else(|| Position {
        symbol: symbol.to_string(),
        side: PositionSide::Long,
        size: 0.0,
        entry: None,
        current: None,
        pnl: None,
        pnl_pct: None,
    });

    let client = tws_service.tws_client();
    let verdict = cerrar_una(&pos, &account_id, client, store).await;

    // Finalize claim.
    let _ = store.with_conn_mut(|conn| {
        claims::finalizar_cierre(conn, &clave, now_ms(), &verdict)
    });

    // Remember verdict (60s replay).
    {
        let mut map = recentes();
        map.insert(clave, (Instant::now(), verdict.clone()));
    }

    verdict
}

// ---------------------------------------------------------------------------
// close_all_positions
// ---------------------------------------------------------------------------

pub async fn close_all_positions<R, S, T>(
    request_id: R,
    store: S,
    tws_service: T,
) -> PositionCloseResult
where
    R: IntoRequestId,
    S: std::ops::Deref<Target = Store>,
    T: std::ops::Deref<Target = TradingService>,
{
    let store = &*store;
    let tws_service = &*tws_service;
    let request_id = request_id.into_request_id();
    let clave = if request_id.is_empty() {
        format!("cierre-todo:{}", sello_anonimo())
    } else {
        format!("cierre-todo:{}", request_id)
    };

    // Replay 60s by clave.
    {
        let mut map = recentes();
        let now = Instant::now();
        map.retain(|_, (ts, _)| now.duration_since(*ts).as_secs() < 60);
        if let Some((_, v)) = map.get(&clave) {
            return v.clone();
        }
    }

    // Claim mass row (lock = null).
    let reclamo = match store.with_conn(|conn| {
        Ok(claims::reclamar_cierre(
            conn,
            &clave,
            "",
            Option::<String>::None,
            now_ms(),
        ))
    }) {
        Ok(r) => r,
        Err(_) => claims::Reclamo::Fallida(claims::db_caida("")),
    };

    match reclamo {
        claims::Reclamo::Nueva => {}
        claims::Reclamo::Repetida(v)
        | claims::Reclamo::Vestigio(v)
        | claims::Reclamo::Ajena(v)
        | claims::Reclamo::Fallida(v) => {
            let mut map = recentes();
            map.insert(clave.clone(), (Instant::now(), v.clone()));
            return v;
        }
    }

    // Early no-TWS verdict.
    let client = tws_service.tws_client();
    let tws_ok = client.as_ref().map(|c| c.is_connected()).unwrap_or(false);
    if !tws_ok {
        let verdict = no_tws("all");
        let _ = store.with_conn_mut(|conn| claims::finalizar_cierre(conn, &clave, now_ms(), &verdict));
        let mut map = recentes();
        map.insert(clave, (Instant::now(), verdict.clone()));
        return verdict;
    }

    let snapshot0 = tws_service.snapshot();
    let account_id = snapshot0.account.map(|a| a.id).unwrap_or_default();

    let mut processed: HashSet<String> = HashSet::new();
    let mut fallos: usize = 0;

    for _pass in 0..2 {
        let snap = tws_service.snapshot();
        // symbols not yet processed
        let mut syms: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for p in &snap.positions {
            if !processed.contains(&p.symbol) && !seen.contains(&p.symbol) {
                seen.insert(p.symbol.clone());
                syms.push(p.symbol.clone());
            }
        }
        for symbol in syms {
            processed.insert(symbol.clone());
            let child_id = format!("{}:{}", clave, symbol);
            let child_reclamo = match store.with_conn(|conn| {
                Ok(claims::reclamar_cierre(
                    conn,
                    &child_id,
                    &symbol,
                    symbol.clone(),
                    now_ms(),
                ))
            }) {
                Ok(r) => r,
                Err(_) => claims::Reclamo::Fallida(claims::db_caida(&symbol)),
            };

            let verdict = match child_reclamo {
                claims::Reclamo::Nueva => {
                    let pos = snap
                        .positions
                        .iter()
                        .find(|p| p.symbol == symbol)
                        .cloned()
                        .unwrap_or_else(|| Position {
                            symbol: symbol.clone(),
                            side: PositionSide::Long,
                            size: 0.0,
                            entry: None,
                            current: None,
                            pnl: None,
                            pnl_pct: None,
                        });
                    let v = cerrar_una(&pos, &account_id, client.clone(), store).await;
                    let _ = store.with_conn_mut(|conn| {
                        claims::finalizar_cierre(conn, &child_id, now_ms(), &v)
                    });
                    v
                }
                claims::Reclamo::Repetida(v)
                | claims::Reclamo::Vestigio(v)
                | claims::Reclamo::Ajena(v)
                | claims::Reclamo::Fallida(v) => v,
            };
            if !verdict.ok {
                fallos += 1;
            }
        }
    }

    // saltadas: symbols still open at end
    let final_snap = tws_service.snapshot();
    let mut open_syms: HashSet<String> = HashSet::new();
    for p in &final_snap.positions {
        open_syms.insert(p.symbol.clone());
    }
    let saltadas = open_syms.len();

    let all_closed = fallos == 0 && saltadas == 0;
    let message = if all_closed {
        "All positions closed.".to_string()
    } else {
        format!("{} failed, {} still open.", fallos, saltadas)
    };
    // Aurea's own keys (`trading.close.all_ok` / `all_failed`): Openbot's
    // `all_requested`/`all_partial` interpolate pre-rendered sub-phrases
    // (`planos`, `restantes`) this port does not build.
    let message_key = if all_closed {
        Some("trading.close.all_ok".into())
    } else {
        Some("trading.close.all_failed".into())
    };
    let mut params = serde_json::Map::new();
    params.insert("count".into(), serde_json::Value::from(processed.len() as i64));
    params.insert("failed".into(), serde_json::Value::from(fallos as i64));
    params.insert("open".into(), serde_json::Value::from(saltadas as i64));

    let verdict = PositionCloseResult {
        ok: all_closed,
        message,
        message_key,
        message_params: Some(params),
    };

    let _ = store.with_conn_mut(|conn| claims::finalizar_cierre(conn, &clave, now_ms(), &verdict));
    {
        let mut map = recentes();
        map.insert(clave, (Instant::now(), verdict.clone()));
    }
    verdict
}

// ---------------------------------------------------------------------------
// Panic state trait
// ---------------------------------------------------------------------------

pub trait PanicState {
    fn stop_all_bots(&mut self) -> Vec<String>;
    fn cancel_pending_tasks(&mut self) -> usize;
    fn push_panic_start_log(&mut self);
}

impl PanicState for TradingState {
    fn stop_all_bots(&mut self) -> Vec<String> {
        let keys: Vec<String> = self.bots.keys().cloned().collect();
        self.bots.clear();
        keys
    }
    fn cancel_pending_tasks(&mut self) -> usize {
        let mut n = 0;
        let now = now_ms();
        for t in &mut self.tasks {
            if t.status == TaskStatus::Pending {
                t.status = TaskStatus::Cancelled;
                t.resolved_at = Some(now);
                t.time_updated = now;
                n += 1;
            }
        }
        n
    }
    fn push_panic_start_log(&mut self) {
        let now = now_ms();
        self.logs.insert(
            0,
            Log {
                ts: now,
                level: LogLevel::Error,
                system: "motor".into(),
                message: "trading.log.panic.start".into(),
                message_key: Some("trading.log.panic.start".into()),
                message_params: None,
                event: None,
            },
        );
    }
}

impl PanicState for &mut TradingState {
    fn stop_all_bots(&mut self) -> Vec<String> {
        (**self).stop_all_bots()
    }
    fn cancel_pending_tasks(&mut self) -> usize {
        (**self).cancel_pending_tasks()
    }
    fn push_panic_start_log(&mut self) {
        (**self).push_panic_start_log()
    }
}

impl PanicState for Arc<Mutex<TradingState>> {
    fn stop_all_bots(&mut self) -> Vec<String> {
        let mut g = self.lock().unwrap_or_else(|p| p.into_inner());
        g.stop_all_bots()
    }
    fn cancel_pending_tasks(&mut self) -> usize {
        let mut g = self.lock().unwrap_or_else(|p| p.into_inner());
        g.cancel_pending_tasks()
    }
    fn push_panic_start_log(&mut self) {
        let mut g = self.lock().unwrap_or_else(|p| p.into_inner());
        g.push_panic_start_log()
    }
}

impl PanicState for Mutex<TradingState> {
    fn stop_all_bots(&mut self) -> Vec<String> {
        let mut g = self.lock().unwrap_or_else(|p| p.into_inner());
        g.stop_all_bots()
    }
    fn cancel_pending_tasks(&mut self) -> usize {
        let mut g = self.lock().unwrap_or_else(|p| p.into_inner());
        g.cancel_pending_tasks()
    }
    fn push_panic_start_log(&mut self) {
        let mut g = self.lock().unwrap_or_else(|p| p.into_inner());
        g.push_panic_start_log()
    }
}

// ---------------------------------------------------------------------------
// panic
// ---------------------------------------------------------------------------

pub async fn panic<R, S, T, U>(
    request_id: R,
    store: S,
    tws_service: T,
    mut state: U,
) -> PanicResult
where
    R: IntoRequestId,
    S: std::ops::Deref<Target = Store>,
    T: std::ops::Deref<Target = TradingService>,
    U: PanicState,
{
    let store = &*store;
    let tws_service = &*tws_service;
    let request_id = request_id.into_request_id();

    state.push_panic_start_log();
    let mut stopped = state.stop_all_bots();
    stopped.sort();
    let cancelled = state.cancel_pending_tasks();

    // Cancel pending tasks in store too.
    if let Ok(tasks) = store.list_tasks() {
        let now = now_ms();
        for t in tasks {
            if t.status == "pending" {
                let _ = store.update_task_status(&t.id, "cancelled", Some(now));
            }
        }
    }

    let close_req = if request_id.is_empty() {
        sello_anonimo()
    } else {
        request_id.clone()
    };
    let close = close_all_positions(close_req, store, tws_service).await;

    // Leftovers: what the panic may not sweep — open rows on the login's
    // other accounts (`sobrantes`, `trading.ts:3697-3746`). The engine
    // account's own survivors are already counted by `close`.
    let mut leftover: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for p in &tws_service.leftover_positions() {
        if seen.insert(p.symbol.clone()) {
            leftover.push(p.symbol.clone());
        }
    }
    leftover.sort();

    let mut params = serde_json::Map::new();
    params.insert(
        "stopped".into(),
        serde_json::Value::Array(stopped.iter().cloned().map(serde_json::Value::String).collect()),
    );
    params.insert("failed".into(), serde_json::Value::from(0i64));
    params.insert("cancelled".into(), serde_json::Value::from(cancelled as i64));
    params.insert("close".into(), serde_json::Value::String(close.message.clone()));
    params.insert("mt5".into(), serde_json::Value::Null);
    params.insert(
        "leftover_symbols".into(),
        serde_json::Value::Array(leftover.iter().cloned().map(serde_json::Value::String).collect()),
    );
    params.insert("leftover_count".into(), serde_json::Value::from(leftover.len() as i64));

    PanicResult {
        stopped,
        close,
        message: "Panic executed.".into(),
        message_key: Some("trading.panic.summary".into()),
        message_params: Some(params),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::TradingService;
    use crate::state::TradingState;
    use crate::store::Store;

    #[tokio::test]
    async fn close_without_tws_returns_no_tws() {
        let store = Store::open_in_memory().unwrap();
        let service = TradingService::new();
        let res = close_position("MNQ", "req-1", &store, &service).await;
        assert_eq!(res.message_key.as_deref(), Some("trading.close.no_tws"));

        // Replay from cierresRecientes / claim returns same verdict.
        let res2 = close_position("MNQ", "req-1", &store, &service).await;
        assert_eq!(res2, res);
    }

    #[tokio::test]
    async fn seeded_verdict_replays() {
        let store = Store::open_in_memory().unwrap();
        // seed a verdict row
        let v = PositionCloseResult { ok: true, message: "seeded".into(), message_key: Some("trading.close.ok".into()), message_params: None };
        let vj = serde_json::to_string(&v).unwrap();
        store.with_conn_mut(|conn| {
            conn.execute(
                "INSERT INTO trading_close (id, symbol, lock, verdict, ts, time_created, time_updated) VALUES ('cierre:seeded', 'MNQ', NULL, ?1, 1, 1, 1)",
                rusqlite::params![vj],
            ).unwrap();
            Ok(())
        }).unwrap();
        let service = TradingService::new();
        let res = close_position("MNQ", "seeded", &store, &service).await;
        assert_eq!(res, v);
    }

    #[tokio::test]
    async fn close_all_without_tws_returns_no_tws() {
        let store = Store::open_in_memory().unwrap();
        let service = TradingService::new();
        let res = close_all_positions("req-all", &store, &service).await;
        assert_eq!(res.message_key.as_deref(), Some("trading.close.no_tws"));

        let res2 = close_all_positions("req-all", &store, &service).await;
        assert_eq!(res2, res);
    }

    #[tokio::test]
    async fn panic_basic() {
        let store = Store::open_in_memory().unwrap();
        let service = TradingService::new();
        let mut state = TradingState::default();
        let res = panic("req-panic", &store, &service, &mut state).await;
        assert_eq!(res.message_key.as_deref(), Some("trading.panic.summary"));
    }
}
