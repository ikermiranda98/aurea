//! In-memory trading state / snapshot builder for Aurea (lane A).
//!
//! Port of the engine's in-memory state that feeds `snapshot()` from
//! Openbot's `packages/core/src/trading.ts` §2.3, built on top of A1's
//! pool/store primitives. Self-contained: no async runtime, network or
//! database is required; the engine layer wraps an instance in
//! `Arc<Mutex<_>>` / `Arc<RwLock<_>>`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use zeron_proto::trading::{
    Account, EquityPoint, Link, Log, LogLevel, Position, Programs, Snapshot, System, Task, Trade,
};

use crate::lifecycle::Runtime;
use crate::pool;

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

/// Anything that can be turned into an optional systems directory.
pub trait IntoSystemsDir {
    fn into_systems_dir(self) -> Option<PathBuf>;
}

impl IntoSystemsDir for Option<PathBuf> {
    fn into_systems_dir(self) -> Option<PathBuf> {
        self
    }
}
impl IntoSystemsDir for PathBuf {
    fn into_systems_dir(self) -> Option<PathBuf> {
        Some(self)
    }
}
impl IntoSystemsDir for &Path {
    fn into_systems_dir(self) -> Option<PathBuf> {
        Some(self.to_path_buf())
    }
}
impl IntoSystemsDir for String {
    fn into_systems_dir(self) -> Option<PathBuf> {
        Some(PathBuf::from(self))
    }
}
impl IntoSystemsDir for &str {
    fn into_systems_dir(self) -> Option<PathBuf> {
        Some(PathBuf::from(self))
    }
}
impl IntoSystemsDir for Option<&Path> {
    fn into_systems_dir(self) -> Option<PathBuf> {
        self.map(|p| p.to_path_buf())
    }
}
impl IntoSystemsDir for Option<String> {
    fn into_systems_dir(self) -> Option<PathBuf> {
        self.map(PathBuf::from)
    }
}
impl IntoSystemsDir for Option<&str> {
    fn into_systems_dir(self) -> Option<PathBuf> {
        self.map(PathBuf::from)
    }
}

/// Anything that can be interpreted as an "extra / bypass debounce" flag.
pub trait PushLogFlag {
    fn is_extra(&self) -> bool;
}

impl PushLogFlag for bool {
    fn is_extra(&self) -> bool {
        *self
    }
}
impl PushLogFlag for i64 {
    fn is_extra(&self) -> bool {
        *self != 0
    }
}
impl PushLogFlag for u64 {
    fn is_extra(&self) -> bool {
        *self != 0
    }
}
impl PushLogFlag for i32 {
    fn is_extra(&self) -> bool {
        *self != 0
    }
}
impl PushLogFlag for Option<i64> {
    fn is_extra(&self) -> bool {
        self.is_some()
    }
}
impl PushLogFlag for Option<u64> {
    fn is_extra(&self) -> bool {
        self.is_some()
    }
}

/// Anything that can be turned into an optional f64 PnL.
pub trait IntoOptionF64 {
    fn into_option(self) -> Option<f64>;
}

impl IntoOptionF64 for Option<f64> {
    fn into_option(self) -> Option<f64> {
        self
    }
}
impl IntoOptionF64 for f64 {
    fn into_option(self) -> Option<f64> {
        Some(self)
    }
}
impl IntoOptionF64 for f32 {
    fn into_option(self) -> Option<f64> {
        Some(self as f64)
    }
}
impl IntoOptionF64 for i64 {
    fn into_option(self) -> Option<f64> {
        Some(self as f64)
    }
}
impl IntoOptionF64 for i32 {
    fn into_option(self) -> Option<f64> {
        Some(self as f64)
    }
}
impl IntoOptionF64 for u64 {
    fn into_option(self) -> Option<f64> {
        Some(self as f64)
    }
}

/// Anything that can be viewed as an optional `Runtime` reference.
pub trait AsRuntimeRef {
    fn as_runtime(&self) -> Option<&Runtime>;
}

impl AsRuntimeRef for &Runtime {
    fn as_runtime(&self) -> Option<&Runtime> {
        Some(*self)
    }
}
impl AsRuntimeRef for Runtime {
    fn as_runtime(&self) -> Option<&Runtime> {
        Some(self)
    }
}
impl AsRuntimeRef for Option<&Runtime> {
    fn as_runtime(&self) -> Option<&Runtime> {
        *self
    }
}
impl AsRuntimeRef for Option<Runtime> {
    fn as_runtime(&self) -> Option<&Runtime> {
        self.as_ref()
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// In-memory engine state that feeds `snapshot()`.
#[derive(Debug, Clone, Default)]
pub struct TradingState {
    pub systems_dir: Option<PathBuf>,
    pub systems: Vec<pool::Dossier>,
    pub bots: HashMap<String, Runtime>,
    pub pnl_bots: HashMap<String, f64>,
    pub positions: Vec<Position>,
    pub logs: Vec<Log>,
    pub tasks: Vec<Task>,
    pub trades: Vec<Trade>,
    pub equity: Vec<EquityPoint>,
    pub accounts: Vec<Account>,
    pub links: Vec<Link>,
    pub connected: bool,
    pub client_in_use: Option<i64>,
    pub account: Option<Account>,
    pub programs: Option<Programs>,
    pub echo_recent: HashMap<String, i64>,
}

/// Alias kept for callers that prefer the longer name.
pub type EngineState = TradingState;
/// Alias kept for callers that prefer the shorter name.
pub type State = TradingState;

impl TradingState {
    pub fn new() -> Self {
        Self::default()
    }

    // -- systems dir ---------------------------------------------------------
    pub fn set_systems_dir<D: IntoSystemsDir>(&mut self, dir: D) {
        self.systems_dir = dir.into_systems_dir();
    }

    // -- pool scan -----------------------------------------------------------
    pub fn scan_pool(&mut self, dir: impl AsRef<Path>) {
        let dir = dir.as_ref();
        self.systems_dir = Some(dir.to_path_buf());
        self.systems = pool::scan_dirs(dir)
            .into_iter()
            .map(|name| pool::dossier(dir, &name))
            .collect();
    }

    pub fn scan_pool_current(&mut self) {
        if let Some(dir) = self.systems_dir.clone() {
            self.scan_pool(dir);
        }
    }

    pub fn reload_pool(&mut self, dir: impl AsRef<Path>) {
        self.scan_pool(dir);
    }

    // -- logs ----------------------------------------------------------------
    pub fn push_log<F: PushLogFlag>(
        &mut self,
        level: LogLevel,
        system: impl Into<String>,
        message: impl Into<String>,
        now_ms: i64,
        extra: F,
    ) {
        let system = system.into();
        let message = message.into();
        let extra = extra.is_extra();

        let key = format!("{system}\u{001f}{message}");
        if !extra {
            if let Some(last) = self.echo_recent.get(&key) {
                if now_ms - *last < 60_000 {
                    return;
                }
            }
        }

        self.echo_recent.insert(key, now_ms);

        self.logs.insert(
            0,
            Log {
                ts: now_ms,
                level,
                system,
                message,
                message_key: None,
                message_params: None,
                event: None,
            },
        );
        if self.logs.len() > 800 {
            self.logs.truncate(800);
        }
    }

    // -- trades --------------------------------------------------------------
    pub fn register_trade<P: IntoOptionF64>(
        &mut self,
        system: impl Into<String>,
        ts: i64,
        pnl: P,
        message: impl Into<String>,
    ) -> bool {
        let system = system.into();
        let message = message.into();
        let pnl = pnl.into_option();

        if self.trades.iter().any(|t| t.system == system && t.ts == ts) {
            return false;
        }

        self.trades.insert(
            0,
            Trade {
                system,
                ts,
                pnl,
                message,
            },
        );
        if self.trades.len() > 200 {
            self.trades.truncate(200);
        }
        true
    }

    // -- schema mapping ------------------------------------------------------
    pub fn to_schema_system<R: AsRuntimeRef>(dossier: &pool::Dossier, runtime: R) -> System {
        let runtime = runtime.as_runtime();
        System {
            id: dossier.id.clone(),
            name: dossier.dir_name.clone(),
            running: runtime.map(|r| r.started_at.is_some()).unwrap_or(false),
            active: runtime.map(|r| r.planeado).unwrap_or(false),
            pid: runtime.and_then(|r| r.pid),
            started_at: runtime.and_then(|r| r.started_at),
            symbol: dossier.symbol.clone().unwrap_or_default(),
            timeframe: dossier.timeframe.clone().unwrap_or_default(),
            pnl_day: None,
            client_id: dossier.client_id,
            close_hour: dossier.close_hour.clone(),
            last_activity_at: runtime.and_then(|r| r.ultima_actividad),
            description: dossier.description.clone(),
            rules: dossier.rules.clone(),
        }
    }

    // -- snapshot ------------------------------------------------------------
    pub fn snapshot(&self, _now_ms: i64) -> Snapshot {
        let systems: Vec<System> = self
            .systems
            .iter()
            .map(|d| {
                let mut s = Self::to_schema_system(d, self.bots.get(&d.id));
                if let Some(pnl) = self.pnl_bots.get(&d.id) {
                    s.pnl_day = Some(*pnl);
                }
                s
            })
            .collect();

        let tasks: Vec<Task> = self.tasks.iter().take(50).cloned().collect();

        Snapshot {
            version: "nativo-1".to_string(),
            connected: self.connected,
            client_in_use: self.client_in_use,
            account: self.account.clone(),
            systems,
            positions: self.positions.clone(),
            open_positions: Some(self.positions.clone()),
            closable_positions: Some(self.positions.clone()),
            logs: self.logs.clone(),
            tasks,
            trades: if self.trades.is_empty() {
                None
            } else {
                Some(self.trades.clone())
            },
            equity: if self.equity.is_empty() {
                None
            } else {
                Some(self.equity.clone())
            },
            accounts: if self.accounts.is_empty() {
                None
            } else {
                Some(self.accounts.clone())
            },
            links: if self.links.is_empty() {
                None
            } else {
                Some(self.links.clone())
            },
            systems_dir: self
                .systems_dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            programs: self.programs.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::Runtime;
    use crate::pool;
    use std::path::PathBuf;
    use zeron_proto::trading::{LogLevel, Programs, ProgramState};

    fn temp_pool() -> (PathBuf, tempfileish) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let seq = N.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "zeron_state_pool_{}_{}",
            std::process::id(),
            seq
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        // create a couple of systems
        for name in ["02_nasdaq_ruptura_media_hora", "07_oro_nube_ichimoku"] {
            let f = path.join(name);
            std::fs::create_dir_all(&f).unwrap();
            std::fs::write(
                f.join("config.py"),
                "CLIENT_ID = 7\nHORA_CIERRE = (16, 30)\n",
            )
            .unwrap();
        }
        (path, tempfileish)
    }

    struct tempfileish;

    #[test]
    fn push_log_ring_cap() {
        let mut st = TradingState::default();
        for i in 0..900 {
            st.push_log(LogLevel::Info, "motor", format!("m{i}"), i as i64, false);
        }
        // debounce: same message within 60s skipped, but messages differ so all push then cap
        assert!(st.logs.len() <= 800);
        assert_eq!(st.logs.len(), 800);
    }

    #[test]
    fn push_log_debounce() {
        let mut st = TradingState::default();
        st.push_log(LogLevel::Info, "motor", "hola", 1000, false);
        st.push_log(LogLevel::Info, "motor", "hola", 1001, false);
        assert_eq!(st.logs.len(), 1);
        st.push_log(LogLevel::Info, "motor", "hola", 1000 + 61_000, false);
        assert_eq!(st.logs.len(), 2);
        // extra bypasses debounce
        st.push_log(LogLevel::Info, "motor", "hola", 1000 + 61_001, true);
        st.push_log(LogLevel::Info, "motor", "hola", 1000 + 61_002, true);
        assert_eq!(st.logs.len(), 4);
    }

    #[test]
    fn register_trade_dedup_cap_order() {
        let mut st = TradingState::default();
        assert!(st.register_trade("s", 1, Some(2.5), "a"));
        assert!(!st.register_trade("s", 1, Some(9.9), "b")); // dedup by system+ts
        assert!(st.register_trade("s", 2, 3.0, "c"));
        // newest first
        assert_eq!(st.trades[0].ts, 2);
        assert_eq!(st.trades[1].ts, 1);
        // cap 200
        for i in 0..250 {
            st.register_trade("s", 1000 + i, Some(i as f64), format!("m{i}"));
        }
        assert_eq!(st.trades.len(), 200);
        assert_eq!(st.trades[0].ts, 1249);
    }

    #[test]
    fn to_schema_system_mapping() {
        let mut d = pool::dossier(
            &std::env::temp_dir(),
            "xx",
        );
        // build dossier manually by using a real pool entry instead
        let pool = {
            let p = std::env::temp_dir().join("zeron_state_map_pool");
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            let f = p.join("07_oro_nube_ichimoku");
            std::fs::create_dir_all(&f).unwrap();
            std::fs::write(
                f.join("config.py"),
                "CLIENT_ID = 42\nHORA_CIERRE = (16, 30, 5)\n",
            )
            .unwrap();
            std::fs::write(
                f.join("reglas.json"),
                r#"{"idea":"idea oro","reglas":{"entrada":"rompe"}}"#,
            )
            .unwrap();
            p
        };
        let dossier = pool::dossier(&pool, "07_oro_nube_ichimoku");

        let mut rt = Runtime::default();
        rt.started_at = Some(1234);
        rt.pid = Some(99);
        rt.planeado = true;
        rt.ultima_actividad = Some(5678);

        let sys = TradingState::to_schema_system(&dossier, &rt);
        assert_eq!(sys.id, "07");
        assert!(sys.running);
        assert!(sys.active);
        assert_eq!(sys.pid, Some(99));
        assert_eq!(sys.started_at, Some(1234));
        assert_eq!(sys.symbol, "GC");
        assert_eq!(sys.timeframe, "1h");
        assert_eq!(sys.client_id, Some(42));
        assert_eq!(sys.close_hour.as_deref(), Some("16:30:05"));
        assert_eq!(sys.last_activity_at, Some(5678));
        assert_eq!(sys.description.as_deref(), Some("idea oro"));
        assert!(sys.rules.as_ref().unwrap().contains_key("entrada"));

        // none runtime
        let sys2 = TradingState::to_schema_system(&dossier, None::<Runtime>);
        assert!(!sys2.running);
        assert!(!sys2.active);

        let _ = d;
        let _ = std::fs::remove_dir_all(&pool);
    }

    #[test]
    fn scan_pool_and_snapshot_basic() {
        let (path, _) = temp_pool();
        let mut st = TradingState::default();
        st.set_systems_dir(Some(path.clone()));
        st.scan_pool(path.clone());
        assert_eq!(st.systems.len(), 2);
        assert_eq!(st.systems_dir, Some(path.clone()));

        st.connected = true;
        st.client_in_use = Some(7);
        st.programs = Some(Programs {
            tws: ProgramState::Running,
            mt5: None,
        });
        st.push_log(LogLevel::Info, "motor", "hola", 10, false);

        let snap = st.snapshot(999);
        assert_eq!(snap.version, "nativo-1");
        assert!(snap.connected);
        assert_eq!(snap.client_in_use, Some(7));
        assert_eq!(snap.systems.len(), 2);
        assert_eq!(snap.logs.len(), 1);
        assert_eq!(snap.systems_dir, Some(path.to_string_lossy().to_string()));
        assert!(snap.programs.is_some());

        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn snapshot_trades_optional() {
        let mut st = TradingState::default();
        assert!(st.snapshot(0).trades.is_none());
        st.register_trade("s", 1, None, "m");
        assert_eq!(st.snapshot(0).trades.unwrap().len(), 1);
    }
}
