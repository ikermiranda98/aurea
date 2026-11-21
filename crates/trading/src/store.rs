//! SQLite store for the trading engine - a 10-table port of Openbot's
//! `packages/core/src/trading/sql.ts`. Self-contained: schema migrations run on
//! open and all CRUD/helpers live here.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde::Serialize;
use sha1::{Digest, Sha1};

/// Milliseconds since the Unix epoch, matching the TS `Date.now()` stamps.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS trading_task (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    system_ids TEXT NOT NULL,
    run_at INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    resolved_at INTEGER,
    time_created INTEGER,
    time_updated INTEGER
);
CREATE TABLE IF NOT EXISTS trading_trade (
    id TEXT PRIMARY KEY,
    system TEXT NOT NULL,
    ts INTEGER NOT NULL,
    pnl REAL,
    message TEXT NOT NULL,
    time_created INTEGER,
    time_updated INTEGER
);
CREATE TABLE IF NOT EXISTS trading_link (
    id TEXT PRIMARY KEY,
    host TEXT NOT NULL,
    port INTEGER NOT NULL,
    label TEXT,
    broker TEXT NOT NULL DEFAULT 'ibkr',
    mt5_login INTEGER,
    mt5_server TEXT,
    time_created INTEGER,
    time_updated INTEGER
);
CREATE TABLE IF NOT EXISTS trading_meta (
    id TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    time_created INTEGER,
    time_updated INTEGER
);
CREATE TABLE IF NOT EXISTS trading_account (
    id TEXT PRIMARY KEY,
    link_id TEXT NOT NULL,
    tipo TEXT,
    time_created INTEGER,
    time_updated INTEGER
);
CREATE TABLE IF NOT EXISTS trading_bot_order (
    id INTEGER PRIMARY KEY,
    system TEXT NOT NULL,
    status TEXT NOT NULL,
    data TEXT NOT NULL,
    updated INTEGER NOT NULL,
    time_created INTEGER,
    time_updated INTEGER
);
CREATE TABLE IF NOT EXISTS trading_bot_fill (
    id TEXT PRIMARY KEY,
    system TEXT NOT NULL,
    time INTEGER NOT NULL,
    data TEXT NOT NULL,
    time_created INTEGER,
    time_updated INTEGER
);
CREATE TABLE IF NOT EXISTS trading_close (
    id TEXT PRIMARY KEY,
    symbol TEXT NOT NULL,
    lock TEXT,
    verdict TEXT,
    ts INTEGER NOT NULL,
    time_created INTEGER,
    time_updated INTEGER
);
CREATE TABLE IF NOT EXISTS trading_rule (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    condition TEXT NOT NULL,
    proposal TEXT,
    reason TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'watching',
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    fired_at INTEGER,
    note TEXT,
    time_created INTEGER,
    time_updated INTEGER
);
CREATE TABLE IF NOT EXISTS trading_proposal (
    id TEXT PRIMARY KEY,
    action TEXT NOT NULL,
    reason TEXT NOT NULL,
    chat_id TEXT,
    status TEXT NOT NULL DEFAULT 'pending',
    created_at INTEGER NOT NULL,
    resolved_at INTEGER,
    result TEXT,
    time_created INTEGER,
    time_updated INTEGER
);
CREATE INDEX IF NOT EXISTS trading_proposal_created_idx ON trading_proposal(created_at);
CREATE UNIQUE INDEX IF NOT EXISTS trading_close_lock_idx ON trading_close(lock);
"#;

/// A `Store` wraps a single SQLite connection (behind a mutex) and applies the
/// 10-table trading schema on open.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

/// Alias kept for callers that prefer the longer name.
pub type TradingStore = Store;

impl Store {
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn migrate(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(MIGRATION_SQL)?;
        Ok(())
    }

    /// Run a read-only closure against the connection.
    pub fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let guard = self.conn.lock().unwrap();
        f(&guard)
    }

    /// Run a mutable closure against the connection.
    pub fn with_conn_mut<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let mut guard = self.conn.lock().unwrap();
        f(&mut guard)
    }

    /// Cloneable handle to the underlying mutex (for callers that want to lock it).
    pub fn conn(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }


    // ------------------------------------------------------------------ meta
    pub fn meta_get(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT value FROM trading_meta WHERE id = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
        })
    }

    pub fn meta_set(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        let now = now_ms();
        self.with_conn_mut(|conn| {
            conn.execute(
                "INSERT INTO trading_meta (id, value, time_created, time_updated) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(id) DO UPDATE SET value = excluded.value, time_updated = excluded.time_updated",
                params![key, value, now, now],
            )?;
            Ok(())
        })
    }

    pub fn meta_remove(&self, key: &str) -> rusqlite::Result<()> {
        self.with_conn_mut(|conn| {
            conn.execute("DELETE FROM trading_meta WHERE id = ?1", params![key])?;
            Ok(())
        })
    }

    // aliases
    pub fn get_meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.meta_get(key)
    }
    pub fn set_meta(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.meta_set(key, value)
    }

    // --------------------------------------------------------------- trades
    /// Deterministic identity "system:ts:<sha1(message)[0..12]>".
    pub fn trade_identity(system: &str, ts: i64, message: &str) -> String {
        let mut hasher = Sha1::new();
        hasher.update(message.as_bytes());
        let hex = format!("{:x}", hasher.finalize());
        format!("{}:{}:{}", system, ts, &hex[..12])
    }

    /// Insert a trade; returns `true` when a new row was inserted (dedupes on PK).
    pub fn insert_trade(
        &self,
        system: &str,
        ts: i64,
        pnl: Option<f64>,
        message: &str,
    ) -> rusqlite::Result<bool> {
        let id = Self::trade_identity(system, ts, message);
        let changes = self.with_conn_mut(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO trading_trade \
                 (id, system, ts, pnl, message, time_created, time_updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![id, system, ts, pnl, message, ts, ts],
            )?;
            Ok(conn.changes() > 0)
        })?;
        Ok(changes)
    }

    pub fn insert_trade_row(&self, trade: &Trade) -> rusqlite::Result<bool> {
        self.insert_trade(&trade.system, trade.ts, trade.pnl, &trade.message)
    }

    pub fn list_trades(&self, limit: usize) -> rusqlite::Result<Vec<Trade>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT system, ts, pnl, message FROM trading_trade \
                 ORDER BY ts DESC LIMIT ?1",
            )?;
            let rows = stmt
                .query_map(params![limit as i64], |row| {
                    Ok(Trade {
                        system: row.get(0)?,
                        ts: row.get(1)?,
                        pnl: row.get(2)?,
                        message: row.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // ---------------------------------------------------------------- tasks
    pub fn insert_task<I, S>(
        &self,
        id: &str,
        kind: &str,
        system_ids: I,
        run_at: i64,
    ) -> rusqlite::Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let ids: Vec<String> = system_ids.into_iter().map(|s| s.as_ref().to_string()).collect();
        let json = serde_json::to_string(&ids).unwrap_or_else(|_| "[]".to_string());
        let now = now_ms();
        self.with_conn_mut(|conn| {
            conn.execute(
                "INSERT INTO trading_task \
                 (id, kind, system_ids, run_at, status, resolved_at, time_created, time_updated) \
                 VALUES (?1, ?2, ?3, ?4, 'pending', NULL, ?5, ?6)",
                params![id, kind, json, run_at, now, now],
            )?;
            Ok(())
        })
    }

    pub fn insert_task_full(&self, task: &Task) -> rusqlite::Result<()> {
        let json = serde_json::to_string(&task.system_ids).unwrap_or_else(|_| "[]".to_string());
        self.with_conn_mut(|conn| {
            conn.execute(
                "INSERT INTO trading_task \
                 (id, kind, system_ids, run_at, status, resolved_at, time_created, time_updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    task.id,
                    task.kind,
                    json,
                    task.run_at,
                    task.status,
                    task.resolved_at,
                    task.time_created,
                    task.time_updated
                ],
            )?;
            Ok(())
        })
    }

    pub fn list_tasks(&self) -> rusqlite::Result<Vec<Task>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, kind, system_ids, run_at, status, resolved_at, time_created, time_updated \
                 FROM trading_task \
                 ORDER BY time_created DESC, run_at DESC \
                 LIMIT 50",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    let system_ids_raw: String = row.get(2)?;
                    let system_ids: Vec<String> =
                        serde_json::from_str(&system_ids_raw).unwrap_or_default();
                    Ok(Task {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        system_ids,
                        run_at: row.get(3)?,
                        status: row.get(4)?,
                        resolved_at: row.get(5)?,
                        time_created: row.get(6)?,
                        time_updated: row.get(7)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn update_task_status(
        &self,
        id: &str,
        status: &str,
        resolved_at: Option<i64>,
    ) -> rusqlite::Result<()> {
        let now = now_ms();
        self.with_conn_mut(|conn| {
            conn.execute(
                "UPDATE trading_task SET status = ?1, resolved_at = ?2, time_updated = ?3 WHERE id = ?4",
                params![status, resolved_at, now, id],
            )?;
            Ok(())
        })
    }

    pub fn update_task(&self, id: &str, status: &str, resolved_at: Option<i64>) -> rusqlite::Result<()> {
        self.update_task_status(id, status, resolved_at)
    }
    pub fn set_task_status(&self, id: &str, status: &str, resolved_at: Option<i64>) -> rusqlite::Result<()> {
        self.update_task_status(id, status, resolved_at)
    }
    pub fn task_update_status(&self, id: &str, status: &str, resolved_at: Option<i64>) -> rusqlite::Result<()> {
        self.update_task_status(id, status, resolved_at)
    }

    // ----------------------------------------------------------------- links
    pub fn upsert_link(
        &self,
        id: &str,
        host: &str,
        port: i64,
        label: Option<&str>,
        broker: &str,
        mt5_login: Option<i64>,
        mt5_server: Option<&str>,
    ) -> rusqlite::Result<()> {
        let now = now_ms();
        self.with_conn_mut(|conn| {
            conn.execute(
                "INSERT INTO trading_link \
                 (id, host, port, label, broker, mt5_login, mt5_server, time_created, time_updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT(id) DO UPDATE SET \
                   host = excluded.host, port = excluded.port, label = excluded.label, \
                   broker = excluded.broker, mt5_login = excluded.mt5_login, \
                   mt5_server = excluded.mt5_server, time_updated = excluded.time_updated",
                params![id, host, port, label, broker, mt5_login, mt5_server, now, now],
            )?;
            Ok(())
        })
    }

    pub fn upsert_link_opt(
        &self,
        id: &str,
        host: &str,
        port: i64,
        label: Option<&str>,
        broker: Option<&str>,
        mt5_login: Option<i64>,
        mt5_server: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.upsert_link(id, host, port, label, broker.unwrap_or("ibkr"), mt5_login, mt5_server)
    }

    pub fn insert_link(&self, link: &Link) -> rusqlite::Result<()> {
        self.upsert_link(
            &link.id,
            &link.host,
            link.port,
            link.label.as_deref(),
            &link.broker,
            link.mt5_login,
            link.mt5_server.as_deref(),
        )
    }

    pub fn delete_link(&self, id: &str) -> rusqlite::Result<()> {
        self.with_conn_mut(|conn| {
            conn.execute("DELETE FROM trading_link WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    pub fn list_links(&self) -> rusqlite::Result<Vec<Link>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, host, port, label, broker, mt5_login, mt5_server, time_created, time_updated \
                 FROM trading_link ORDER BY time_created ASC",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(Link {
                        id: row.get(0)?,
                        host: row.get(1)?,
                        port: row.get(2)?,
                        label: row.get(3)?,
                        broker: row.get(4)?,
                        mt5_login: row.get(5)?,
                        mt5_server: row.get(6)?,
                        time_created: row.get(7)?,
                        time_updated: row.get(8)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // -------------------------------------------------------------- accounts
    pub fn upsert_account(
        &self,
        id: &str,
        link_id: &str,
        tipo: Option<&str>,
    ) -> rusqlite::Result<()> {
        let now = now_ms();
        self.with_conn_mut(|conn| {
            conn.execute(
                "INSERT INTO trading_account (id, link_id, tipo, time_created, time_updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(id) DO UPDATE SET \
                   link_id = excluded.link_id, tipo = excluded.tipo, time_updated = excluded.time_updated",
                params![id, link_id, tipo, now, now],
            )?;
            Ok(())
        })
    }

    pub fn insert_account(&self, account: &Account) -> rusqlite::Result<()> {
        self.upsert_account(&account.id, &account.link_id, account.tipo.as_deref())
    }

    pub fn delete_account(&self, id: &str) -> rusqlite::Result<()> {
        self.with_conn_mut(|conn| {
            conn.execute("DELETE FROM trading_account WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    pub fn delete_accounts_by_link(&self, link_id: &str) -> rusqlite::Result<()> {
        self.with_conn_mut(|conn| {
            conn.execute("DELETE FROM trading_account WHERE link_id = ?1", params![link_id])?;
            Ok(())
        })
    }

    /// Delete accounts whose link_id is no longer in `known_link_ids`.
    pub fn remove_accounts_for_missing_links<I, S>(
        &self,
        known_link_ids: I,
    ) -> rusqlite::Result<usize>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let known: Vec<String> = known_link_ids.into_iter().map(|s| s.as_ref().to_string()).collect();
        self.with_conn_mut(|conn| {
            // Build a NOT IN list.
            if known.is_empty() {
                let n = conn.execute("DELETE FROM trading_account", [])?;
                return Ok(n);
            }
            let placeholders: Vec<String> = known.iter().enumerate().map(|(i, _)| format!("?{}", i + 1)).collect();
            let sql = format!(
                "DELETE FROM trading_account WHERE link_id NOT IN ({})",
                placeholders.join(", ")
            );
            let params: Vec<&dyn rusqlite::ToSql> = known.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let n = conn.execute(&sql, params.as_slice())?;
            Ok(n)
        })
    }

    pub fn remove_orphan_accounts(&self, known_link_ids: &[String]) -> rusqlite::Result<usize> {
        self.remove_accounts_for_missing_links(known_link_ids.iter().cloned())
    }

    pub fn remove_accounts_without_links(&self) -> rusqlite::Result<usize> {
        self.with_conn_mut(|conn| {
            let n = conn.execute(
                "DELETE FROM trading_account WHERE link_id NOT IN (SELECT id FROM trading_link)",
                [],
            )?;
            Ok(n)
        })
    }

    pub fn sync_accounts_remove_missing(&self, known_link_ids: &[String]) -> rusqlite::Result<usize> {
        self.remove_accounts_for_missing_links(known_link_ids.iter().cloned())
    }

    pub fn list_accounts(&self) -> rusqlite::Result<Vec<Account>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, link_id, tipo, time_created, time_updated FROM trading_account ORDER BY time_created ASC",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(Account {
                        id: row.get(0)?,
                        link_id: row.get(1)?,
                        tipo: row.get(2)?,
                        time_created: row.get(3)?,
                        time_updated: row.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // -------------------------------------------------------------- bot order
    pub fn upsert_bot_order(&self, order: &BotOrder) -> rusqlite::Result<()> {
        let now = now_ms();
        self.with_conn_mut(|conn| {
            conn.execute(
                "INSERT INTO trading_bot_order \
                 (id, system, status, data, updated, time_created, time_updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(id) DO UPDATE SET \
                   system = excluded.system, status = excluded.status, data = excluded.data, \
                   updated = excluded.updated, time_updated = excluded.time_updated",
                params![
                    order.id,
                    order.system,
                    order.status,
                    order.data,
                    order.updated,
                    order.time_created.unwrap_or(now),
                    now
                ],
            )?;
            Ok(())
        })
    }

    pub fn insert_bot_order(&self, order: &BotOrder) -> rusqlite::Result<()> {
        self.upsert_bot_order(order)
    }

    pub fn list_bot_orders(&self) -> rusqlite::Result<Vec<BotOrder>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, system, status, data, updated, time_created, time_updated FROM trading_bot_order",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(BotOrder {
                        id: row.get(0)?,
                        system: row.get(1)?,
                        status: row.get(2)?,
                        data: row.get(3)?,
                        updated: row.get(4)?,
                        time_created: row.get(5)?,
                        time_updated: row.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // --------------------------------------------------------------- bot fill
    pub fn insert_bot_fill(&self, fill: &BotFill) -> rusqlite::Result<()> {
        let now = now_ms();
        self.with_conn_mut(|conn| {
            conn.execute(
                "INSERT OR IGNORE INTO trading_bot_fill \
                 (id, system, time, data, time_created, time_updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    fill.id,
                    fill.system,
                    fill.time,
                    fill.data,
                    fill.time_created.unwrap_or(now),
                    now
                ],
            )?;
            Ok(())
        })
    }

    pub fn upsert_bot_fill(&self, fill: &BotFill) -> rusqlite::Result<()> {
        self.insert_bot_fill(fill)
    }

    pub fn list_bot_fills(&self) -> rusqlite::Result<Vec<BotFill>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, system, time, data, time_created, time_updated FROM trading_bot_fill",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(BotFill {
                        id: row.get(0)?,
                        system: row.get(1)?,
                        time: row.get(2)?,
                        data: row.get(3)?,
                        time_created: row.get(4)?,
                        time_updated: row.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    // -------------------------------------------------------------- close/claims
    pub fn insert_close(&self, close: &Close) -> rusqlite::Result<()> {
        let now = now_ms();
        self.with_conn_mut(|conn| {
            conn.execute(
                "INSERT INTO trading_close \
                 (id, symbol, lock, verdict, ts, time_created, time_updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    close.id,
                    close.symbol,
                    close.lock,
                    close.verdict,
                    close.ts,
                    close.time_created.unwrap_or(now),
                    close.time_updated.unwrap_or(now)
                ],
            )?;
            Ok(())
        })
    }

    pub fn insert_close_claim(&self, close: &Close) -> rusqlite::Result<bool> {
        let now = now_ms();
        self.with_conn_mut(|conn| {
            let n = conn.execute(
                "INSERT OR IGNORE INTO trading_close \
                 (id, symbol, lock, verdict, ts, time_created, time_updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    close.id,
                    close.symbol,
                    close.lock,
                    close.verdict,
                    close.ts,
                    close.time_created.unwrap_or(now),
                    close.time_updated.unwrap_or(now)
                ],
            )?;
            Ok(n > 0)
        })
    }

    pub fn get_close(&self, id: &str) -> rusqlite::Result<Option<Close>> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT id, symbol, lock, verdict, ts, time_created, time_updated \
                 FROM trading_close WHERE id = ?1",
                params![id],
                |row| {
                    Ok(Close {
                        id: row.get(0)?,
                        symbol: row.get(1)?,
                        lock: row.get(2)?,
                        verdict: row.get(3)?,
                        ts: row.get(4)?,
                        time_created: row.get(5)?,
                        time_updated: row.get(6)?,
                    })
                },
            )
            .optional()
        })
    }

    pub fn find_close_by_lock(&self, lock: &str) -> rusqlite::Result<Option<Close>> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT id, symbol, lock, verdict, ts, time_created, time_updated \
                 FROM trading_close WHERE lock = ?1",
                params![lock],
                |row| {
                    Ok(Close {
                        id: row.get(0)?,
                        symbol: row.get(1)?,
                        lock: row.get(2)?,
                        verdict: row.get(3)?,
                        ts: row.get(4)?,
                        time_created: row.get(5)?,
                        time_updated: row.get(6)?,
                    })
                },
            )
            .optional()
        })
    }

    pub fn update_close_verdict(
        &self,
        id: &str,
        verdict: Option<&str>,
        lock: Option<&str>,
        ts: i64,
    ) -> rusqlite::Result<()> {
        let now = now_ms();
        self.with_conn_mut(|conn| {
            conn.execute(
                "UPDATE trading_close SET verdict = ?1, lock = ?2, ts = ?3, time_updated = ?4 WHERE id = ?5",
                params![verdict, lock, ts, now, id],
            )?;
            Ok(())
        })
    }

    /// Delete verdict rows older than `max_age_ms` (open claims, lock held, are kept).
    pub fn purge_aged_verdicts(&self, now_ms: i64, max_age_ms: i64) -> rusqlite::Result<usize> {
        let cutoff = now_ms - max_age_ms;
        self.with_conn_mut(|conn| {
            let n = conn.execute(
                "DELETE FROM trading_close WHERE verdict IS NOT NULL AND ts <= ?1",
                params![cutoff],
            )?;
            Ok(n)
        })
    }

    pub fn purge_verdicts(&self, now_ms: i64, max_age_ms: i64) -> rusqlite::Result<usize> {
        self.purge_aged_verdicts(now_ms, max_age_ms)
    }

    pub fn purge_aged_verdicts_default(&self, now_ms: i64) -> rusqlite::Result<usize> {
        self.purge_aged_verdicts(now_ms, 24 * 60 * 60 * 1000)
    }
}


// ------------------------------------------------------------------ types
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub kind: String,
    pub system_ids: Vec<String>,
    pub run_at: i64,
    pub status: String,
    pub resolved_at: Option<i64>,
    pub time_created: Option<i64>,
    pub time_updated: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub system: String,
    pub ts: i64,
    pub pnl: Option<f64>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub id: String,
    pub host: String,
    pub port: i64,
    pub label: Option<String>,
    pub broker: String,
    pub mt5_login: Option<i64>,
    pub mt5_server: Option<String>,
    pub time_created: Option<i64>,
    pub time_updated: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub link_id: String,
    pub tipo: Option<String>,
    pub time_created: Option<i64>,
    pub time_updated: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BotOrder {
    pub id: i64,
    pub system: String,
    pub status: String,
    pub data: String,
    pub updated: i64,
    pub time_created: Option<i64>,
    pub time_updated: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BotFill {
    pub id: String,
    pub system: String,
    pub time: i64,
    pub data: String,
    pub time_created: Option<i64>,
    pub time_updated: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Close {
    pub id: String,
    pub symbol: String,
    pub lock: Option<String>,
    pub verdict: Option<String>,
    pub ts: i64,
    pub time_created: Option<i64>,
    pub time_updated: Option<i64>,
}

// aliases
pub type TradingTask = Task;
pub type TaskRow = Task;
pub type TradingTrade = Trade;
pub type TradeRow = Trade;
pub type TradingLink = Link;
pub type LinkRow = Link;
pub type TradingAccount = Account;
pub type AccountRow = Account;
pub type TradingBotOrder = BotOrder;
pub type BotOrderRow = BotOrder;
pub type TradingBotFill = BotFill;
pub type BotFillRow = BotFill;
pub type TradingClose = Close;
pub type CloseRow = Close;
pub type CloseClaim = Close;

// free function alias for trade identity (module-level)
pub fn trade_identity(system: &str, ts: i64, message: &str) -> String {
    Store::trade_identity(system, ts, message)
}

// ------------------------------------------------------------------ tests
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_creates_ten_tables_and_unique_index() {
        let store = Store::open_in_memory().unwrap();
        store
            .with_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'trading_%' ORDER BY name",
                )?;
                let names: Vec<String> = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                assert_eq!(names.len(), 10, "expected 10 tables, got {:?}", names);
                assert!(names.iter().any(|name| name == "trading_rule"), "{names:?}");
                assert!(names.iter().any(|name| name == "trading_proposal"), "{names:?}");
                let idx: Option<String> = conn
                    .query_row(
                        "SELECT name FROM sqlite_master WHERE type='index' AND name='trading_close_lock_idx'",
                        [],
                        |r| r.get(0),
                    )
                    .optional()?;
                assert_eq!(idx.as_deref(), Some("trading_close_lock_idx"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn trade_identity_matches_sha1_prefix() {
        use sha1::{Digest, Sha1};
        let msg = "FUERA +12.5 USD brutos";
        let mut h = Sha1::new();
        h.update(msg.as_bytes());
        let hex = format!("{:x}", h.finalize());
        let expected = format!("sys:42:{}", &hex[..12]);
        assert_eq!(Store::trade_identity("sys", 42, msg), expected);
        assert_eq!(trade_identity("sys", 42, msg), expected);
    }

    #[test]
    fn insert_trade_dedupes_on_conflict() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.insert_trade("s", 1, Some(2.5), "msg").unwrap());
        assert!(!store.insert_trade("s", 1, Some(2.5), "msg").unwrap());
        let trades = store.list_trades(10).unwrap();
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].system, "s");
        assert_eq!(trades[0].ts, 1);
        assert_eq!(trades[0].pnl, Some(2.5));
    }

    #[test]
    fn meta_get_set_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.meta_get("k").unwrap(), None);
        store.meta_set("k", "v").unwrap();
        assert_eq!(store.meta_get("k").unwrap(), Some("v".to_string()));
        store.meta_set("k", "v2").unwrap();
        assert_eq!(store.meta_get("k").unwrap(), Some("v2".to_string()));
    }

    #[test]
    fn trading_close_null_lock_allows_duplicates_but_nonnull_rejects() {
        let store = Store::open_in_memory().unwrap();
        // two rows with NULL lock are allowed (SQLite counts NULLs as distinct)
        store
            .with_conn_mut(|conn| {
                conn.execute(
                    "INSERT INTO trading_close (id, symbol, lock, verdict, ts) VALUES ('a', 'X', NULL, 'v1', 1)",
                    [],
                )?;
                conn.execute(
                    "INSERT INTO trading_close (id, symbol, lock, verdict, ts) VALUES ('b', 'Y', NULL, 'v2', 2)",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        // duplicate non-null lock rejected
        store
            .with_conn_mut(|conn| {
                conn.execute(
                    "INSERT INTO trading_close (id, symbol, lock, verdict, ts) VALUES ('c', 'Z', 'lock1', NULL, 3)",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let err = store.with_conn_mut(|conn| {
            conn.execute(
                "INSERT INTO trading_close (id, symbol, lock, verdict, ts) VALUES ('d', 'W', 'lock1', NULL, 4)",
                [],
            )
        });
        assert!(err.is_err(), "duplicate non-null lock should be rejected");
    }

    #[test]
    fn purge_deletes_only_aged_verdict_rows() {
        let store = Store::open_in_memory().unwrap();
        let now = 1_000_000i64;
        let max_age = 24 * 60 * 60 * 1000i64;
        // aged verdict row -> deleted
        store
            .with_conn_mut(|conn| {
                conn.execute(
                    "INSERT INTO trading_close (id, symbol, lock, verdict, ts) VALUES ('v1', 'X', NULL, 'verdict', ?1)",
                    params![now - max_age - 1],
                )?;
                // fresh verdict row -> kept
                conn.execute(
                    "INSERT INTO trading_close (id, symbol, lock, verdict, ts) VALUES ('v2', 'Y', NULL, 'verdict', ?1)",
                    params![now],
                )?;
                // open claim (verdict NULL) aged -> kept
                conn.execute(
                    "INSERT INTO trading_close (id, symbol, lock, verdict, ts) VALUES ('o1', 'Z', 'lockz', NULL, ?1)",
                    params![now - max_age - 1],
                )?;
                Ok(())
            })
            .unwrap();
        let deleted = store.purge_aged_verdicts(now, max_age).unwrap();
        assert_eq!(deleted, 1);
        // verify remaining
        store
            .with_conn(|conn| {
                let count: i64 = conn.query_row("SELECT COUNT(*) FROM trading_close", [], |r| r.get(0))?;
                assert_eq!(count, 2);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn open_creates_file_and_roundtrips() {
        let dir = std::env::temp_dir().join("aurea_store_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("store.sqlite3");
        let _ = std::fs::remove_file(&path);
        {
            let store = Store::open(&path).unwrap();
            store.meta_set("a", "b").unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.meta_get("a").unwrap(), Some("b".to_string()));
        let _ = std::fs::remove_file(&path);
    }
}
