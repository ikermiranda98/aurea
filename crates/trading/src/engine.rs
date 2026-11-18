//! The real trading engine: orchestrates pool, store, state, tasks and lifecycle.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tokio::time::{Duration, Instant};
use zeron_proto::trading::{
    PoolSelectResult, Snapshot, Task, TaskAddInput, TaskKind, TaskStatus, TradingError,
};

use crate::pool;
use crate::state::TradingState;
use crate::store::Store;
use crate::tasks;
use crate::{SnapshotWatch};

/// The trading engine.
pub struct TradingEngine {
    store: Store,
    state: Arc<Mutex<TradingState>>,
    systems_dir: Option<PathBuf>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    watch: watch::Sender<Snapshot>,
    _scheduler: tokio::task::JoinHandle<()>,
}

impl TradingEngine {
    pub fn new(systems_dir: Option<PathBuf>, db_path: Option<PathBuf>) -> Result<Self, String> {
        let store = match db_path {
            Some(p) if !p.as_os_str().is_empty() => Store::open(&p).map_err(|e| e.to_string())?,
            _ => Store::open_in_memory().map_err(|e| e.to_string())?,
        };

        let mut state = TradingState::new();
        if let Some(dir) = &systems_dir {
            state.scan_pool(dir);
            state.set_systems_dir(dir.clone());
        }

        // Reuse or generate client_id.
        if store.meta_get("client_id").map_err(|e| e.to_string())?.is_none() {
            let id = 1000 + (std::process::id() as i64 % 9000);
            store
                .meta_set("client_id", &id.to_string())
                .map_err(|e| e.to_string())?;
        }

        let state = Arc::new(Mutex::new(state));
        let initial = state.lock().unwrap().snapshot(now_ms());
        let (watch, _rx) = watch::channel(initial);

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state_clone = Arc::clone(&state);
        let store_conn = store.conn();
        let watch_clone = watch.clone();
        let cancel_clone = Arc::clone(&cancel);

        let scheduler = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                if cancel_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let now = now_ms();
                let conn = store_conn.lock().unwrap();
                let pending = tasks::list_tasks(&conn, 50).unwrap_or_default();
                drop(conn);

                for task in &pending {
                    if task.status != TaskStatus::Pending || task.run_at > now {
                        continue;
                    }
                    let conn = store_conn.lock().unwrap();
                    let _ = conn.execute(
                        "UPDATE trading_task SET status='ran', resolved_at=?1, time_updated=?1 WHERE id=?2",
                        rusqlite::params![now, task.id],
                    );
                    drop(conn);
                }

                let snap = state_clone.lock().unwrap().snapshot(now);
                let _ = watch_clone.send_replace(snap);
            }
        });

        let engine = Self {
            store,
            state,
            systems_dir,
            cancel,
            watch,
            _scheduler: scheduler,
        };

        let snap = engine.state.lock().unwrap().snapshot(now_ms());
        let _ = engine.watch.send_replace(snap);

        Ok(engine)
    }

    pub fn snapshot(&self) -> Snapshot {
        self.state.lock().unwrap().snapshot(now_ms())
    }

    pub fn watch(&self) -> SnapshotWatch {
        self.watch.subscribe()
    }

    pub fn start_system(&self, system_id: &str) -> Result<(), TradingError> {
        let mut state = self.state.lock().unwrap();
        if !state.systems.iter().any(|d| d.id == system_id) {
            return Err(TradingError::SystemNotFound {
                system_id: system_id.to_string(),
            });
        }
        if state.bots.get(system_id).map(|r| r.started_at.is_some()).unwrap_or(false) {
            return Ok(());
        }
        let runtime = crate::lifecycle::runtime_nuevo();
        state.bots.insert(system_id.to_string(), runtime);
        state.push_log(
            zeron_proto::trading::LogLevel::Info,
            "motor",
            format!("Bot {} arrancado", system_id),
            now_ms(),
            true,
        );
        drop(state);
        self.publish();
        Ok(())
    }

    pub fn stop_system(&self, system_id: &str) -> Result<(), TradingError> {
        let mut state = self.state.lock().unwrap();
        if !state.systems.iter().any(|d| d.id == system_id) {
            return Err(TradingError::SystemNotFound {
                system_id: system_id.to_string(),
            });
        }
        state.bots.remove(system_id);
        state.push_log(
            zeron_proto::trading::LogLevel::Info,
            "motor",
            format!("Bot {} parado", system_id),
            now_ms(),
            true,
        );
        drop(state);
        self.publish();
        Ok(())
    }

    pub fn task_add(&self, input: &TaskAddInput) -> Result<Task, TradingError> {
        let state = self.state.lock().unwrap();
        let valid_ids: Vec<String> = state.systems.iter().map(|d| d.id.clone()).collect();
        drop(state);

        let conn = self.store.conn();
        let guard = conn.lock().unwrap();
        let task = tasks::add_task(&guard, input, now_ms(), &valid_ids)
            .map_err(|msg| TradingError::InvalidTask { message: msg })?;
        let tasks_list = tasks::list_tasks(&guard, 50).unwrap_or_default();
        drop(guard);

        let mut state = self.state.lock().unwrap();
        state.tasks = tasks_list;
        drop(state);
        self.publish();
        Ok(task)
    }

    pub fn task_cancel(&self, task_id: &str) -> Result<Task, TradingError> {
        let conn = self.store.conn();
        let guard = conn.lock().unwrap();
        let task = tasks::cancel_task(&guard, task_id, now_ms())
            .map_err(|msg| TradingError::TaskNotFound {
                task_id: task_id.to_string(),
            })?;
        let tasks_list = tasks::list_tasks(&guard, 50).unwrap_or_default();
        drop(guard);

        let mut state = self.state.lock().unwrap();
        state.tasks = tasks_list;
        drop(state);
        self.publish();
        Ok(task)
    }

    pub fn task_list(&self) -> Vec<Task> {
        {
            let conn = self.store.conn();
            let guard = conn.lock().unwrap();
            tasks::list_tasks(&guard, 50).unwrap_or_default()
        }
    }

    pub fn pool_select(&mut self, dir: &str) -> Result<PoolSelectResult, TradingError> {
        let path = PathBuf::from(dir);
        let names = pool::scan_dirs(&path);
        if names.is_empty() {
            return Err(TradingError::InvalidTask {
                message: format!("No systems in {dir}"),
            });
        }
        let count = names.len() as u32;

        let mut state = self.state.lock().unwrap();
        state.scan_pool(&path);
        state.set_systems_dir(path.clone());
        drop(state);

        let _ = self.store.meta_set("carpeta-sistemas", dir);
        self.systems_dir = Some(path);

        self.publish();
        Ok(PoolSelectResult {
            dir: dir.to_string(),
            systems: count,
        })
    }

    pub fn shutdown(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn publish(&self) {
        let snap = self.state.lock().unwrap().snapshot(now_ms());
        let _ = self.watch.send_replace(snap);
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn construction_in_memory() {
        let engine = TradingEngine::new(None, None).unwrap();
        let snap = engine.snapshot();
        let _ = snap.systems.len();
    }

    #[tokio::test]
    async fn pool_select_and_task_roundtrip() {
        let seq = std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("aurea_engine_test_{}_{}", std::process::id(), seq));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("07_prueba")).unwrap();
        fs::write(dir.join("07_prueba").join("bot.py"), "").unwrap();

        let mut engine = TradingEngine::new(None, None).unwrap();
        let res = engine.pool_select(dir.to_str().unwrap()).unwrap();
        assert_eq!(res.systems, 1);

        let snap = engine.snapshot();
        assert_eq!(snap.systems.len(), 1);

        let input = TaskAddInput {
            kind: TaskKind::StopSystems,
            system_ids: Some(vec![]),
            delay_minutes: Some(5.0),
            at_time: None,
        };
        let task = engine.task_add(&input).unwrap();
        assert_eq!(task.status, TaskStatus::Pending);

        let cancelled = engine.task_cancel(&task.id).unwrap();
        assert_eq!(cancelled.status, TaskStatus::Cancelled);

        let _ = fs::remove_dir_all(&dir);
    }
}
