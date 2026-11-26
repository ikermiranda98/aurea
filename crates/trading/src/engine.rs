//! The real trading engine: orchestrates pool, store, state, tasks and lifecycle.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use tokio::time::Duration;
use zeron_proto::trading::{
    PoolSelectResult, Snapshot, Task, TaskAddInput, TaskKind, TaskStatus, TradingError,
};

use crate::pool;
use crate::state::TradingState;
use crate::store::Store;
use crate::tasks;
use crate::{SnapshotWatch};

/// Told when a scheduled task ends in `Failed` or `Missed`, with the note the
/// person would read: the desk says out loud what did not happen. A task that
/// simply ran does not call it (the desk and the log already show it).
pub type TaskListener = Arc<dyn Fn(&Task, TaskStatus, &str) + Send + Sync>;

/// The trading engine.
pub struct TradingEngine {
    store: Store,
    state: Arc<Mutex<TradingState>>,
    systems_dir: Option<PathBuf>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    watch: watch::Sender<Snapshot>,
    task_listener: Arc<Mutex<Option<TaskListener>>>,
    _scheduler: tokio::task::JoinHandle<()>,
}

impl TradingEngine {
    pub fn new(systems_dir: Option<PathBuf>, db_path: Option<PathBuf>) -> Result<Self, String> {
        let store = match db_path {
            Some(p) if !p.as_os_str().is_empty() => Store::open(&p).map_err(|e| e.to_string())?,
            _ => Store::open_in_memory().map_err(|e| e.to_string())?,
        };

        // The folder chosen in Settings → Sistemas (`pool_select`) when no
        // TRADING_SYSTEMS_DIR overrides it, as long as it still exists.
        let systems_dir = systems_dir.or_else(|| {
            store
                .meta_get("carpeta-sistemas")
                .ok()
                .flatten()
                .map(PathBuf::from)
                .filter(|dir| dir.is_dir())
        });
        let mut state = TradingState::new();
        if let Some(dir) = &systems_dir {
            state.scan_pool(dir);
            state.set_systems_dir(dir.clone());
        }
        state.wanted = store
            .meta_get(WANTED_KEY)
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        let resume_from = now_ms() + RESUME_DELAY_MS;

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
        let scheduler_dir = systems_dir.clone();
        let task_listener: Arc<Mutex<Option<TaskListener>>> = Arc::new(Mutex::new(None));
        let listener_clone = Arc::clone(&task_listener);

        let scheduler = tokio::spawn(async move {
            let mut resuming = true;
            let mut last_resume = 0_i64;
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                if cancel_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let now = now_ms();
                let conn = store_conn.lock().unwrap();
                // Every pending task, however many newer ones the store holds.
                let pending = tasks::list_pending_tasks(&conn).unwrap_or_default();
                drop(conn);

                for task in &pending {
                    if task.status != TaskStatus::Pending || task.run_at > now {
                        continue;
                    }
                    // A task runs its bot action here; the person already said
                    // yes when they scheduled it.
                    let (status, note) =
                        run_due_task(&state_clone, &scheduler_dir, &cancel_clone, task, now);
                    let conn = store_conn.lock().unwrap();
                    let _ = conn.execute(
                        "UPDATE trading_task SET status=?1, resolved_at=?2, time_updated=?2 WHERE id=?3",
                        rusqlite::params![tasks::status_to_str(status), now, task.id],
                    );
                    drop(conn);
                    if let Some(note) = note {
                        if matches!(status, TaskStatus::Failed | TaskStatus::Missed) {
                            let listener = listener_clone
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .clone();
                            if let Some(listener) = listener {
                                listener(task, status, &note);
                            }
                        }
                        state_clone.lock().unwrap().push_log(
                            zeron_proto::trading::LogLevel::Info,
                            "motor",
                            note,
                            now,
                            true,
                        );
                    }
                }

                // What the person wants running survives a restart.
                let wanted = {
                    let mut state = state_clone.lock().unwrap();
                    std::mem::take(&mut state.wanted_dirty).then(|| state.wanted.clone())
                };
                if let Some(wanted) = wanted {
                    let conn = store_conn.lock().unwrap();
                    let _ = conn.execute(
                        "INSERT INTO trading_meta (id, value, time_created, time_updated) \
                         VALUES (?1, ?2, ?3, ?3) \
                         ON CONFLICT(id) DO UPDATE SET value = excluded.value, time_updated = excluded.time_updated",
                        rusqlite::params![WANTED_KEY, serde_json::to_string(&wanted).unwrap_or_default(), now],
                    );
                }
                // After a restart: bring back the bots the person had running.
                if resuming && now >= resume_from && now - last_resume >= 5_000 {
                    last_resume = now;
                    resuming = resume_step(&state_clone, &scheduler_dir, now >= resume_from + RESUME_WINDOW_MS);
                }

                let conn = store_conn.lock().unwrap();
                let tasks_now = tasks::list_tasks(&conn, 50).unwrap_or_default();
                drop(conn);
                let mut state = state_clone.lock().unwrap();
                state.tasks = tasks_now;
                let snap = state.snapshot(now);
                drop(state);
                publish_if_changed(&watch_clone, snap);
            }
        });

        // The desk's sweep: a bot that exits on its own must stop reading as
        // running (the panel used to keep it forever), and a live `panel.pid`
        // left by a previous boot is adopted.
        let sweep_state = Arc::clone(&state);
        let sweep_watch = watch.clone();
        let sweep_cancel = Arc::clone(&cancel);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(45));
            loop {
                // The first tick is immediate, so a boot adopts what a previous
                // run left alive before the panel shows anything.
                interval.tick().await;
                if sweep_cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let snap = {
                    let mut state = sweep_state.lock().unwrap();
                    sweep_state_in(&mut state);
                    state.snapshot(now_ms())
                };
                publish_if_changed(&sweep_watch, snap);
            }
        });

        // One reader for the whole desk: every running bot - started here or
        // adopted by the sweep - has its logs ingested every 5s.
        let ingest_state = Arc::clone(&state);
        let ingest_cancel = Arc::clone(&cancel);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                interval.tick().await;
                if ingest_cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let running: Vec<(String, Option<PathBuf>)> = {
                    let state = ingest_state.lock().unwrap();
                    let dir = state.systems_dir.clone();
                    state
                        .bots
                        .iter()
                        .filter(|(_, runtime)| runtime.started_at.is_some())
                        .map(|(id, _)| (id.clone(), dir.clone()))
                        .collect()
                };
                for (id, dir) in running {
                    ingest_one(&ingest_state, &id, dir.as_deref());
                }
            }
        });

        let engine = Self {
            store,
            state,
            systems_dir,
            cancel,
            watch,
            task_listener,
            _scheduler: scheduler,
        };

        let snap = engine.state.lock().unwrap().snapshot(now_ms());
        publish_if_changed(&engine.watch, snap);

        Ok(engine)
    }

    pub fn snapshot(&self) -> Snapshot {
        self.state.lock().unwrap().snapshot(now_ms())
    }

    pub fn store(&self) -> Store {
        self.store.clone()
    }

    pub fn state(&self) -> Arc<Mutex<TradingState>> {
        Arc::clone(&self.state)
    }

    pub fn watch(&self) -> SnapshotWatch {
        self.watch.subscribe()
    }

    /// The desk's news hook for scheduled tasks that did not do their job
    /// (`Failed` or `Missed`); set once by the engine's assembly.
    pub fn set_task_listener(&self, listener: TaskListener) {
        *self
            .task_listener
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(listener);
    }

    pub fn start_system(&self, system_id: &str) -> Result<(), TradingError> {
        let outcome = start_system_in(&self.state, &self.systems_dir, system_id);
        if outcome.is_ok() {
            self.save_wanted();
            self.publish();
        }
        outcome
    }

    /// Writes what the person wants running now, not on the next tick: Aurea
    /// may close right after a start or a stop.
    fn save_wanted(&self) {
        let wanted = {
            let mut state = self.state.lock().unwrap();
            std::mem::take(&mut state.wanted_dirty).then(|| state.wanted.clone())
        };
        if let Some(wanted) = wanted {
            let _ = self.store.meta_set(WANTED_KEY, &serde_json::to_string(&wanted).unwrap_or_default());
        }
    }
}

/// Starts one bot from the pool. Shared by the desk's button and the task
/// scheduler, which runs a start the person already scheduled.
fn start_system_in(
    state_arc: &Arc<Mutex<TradingState>>,
    systems_dir: &Option<PathBuf>,
    system_id: &str,
) -> Result<(), TradingError> {
    let mut state = state_arc.lock().unwrap();
        if !state.systems.iter().any(|d| d.id == system_id) {
            return Err(TradingError::SystemNotFound {
                system_id: system_id.to_string(),
            });
        }
        if state.bots.get(system_id).map(|r| r.started_at.is_some()).unwrap_or(false) {
            return Ok(());
        }

        let systems_dir = state.systems_dir.clone().or(systems_dir.clone());
        let dossier = state.systems.iter().find(|d| d.id == system_id).cloned();
        let dir_name = dossier
            .as_ref()
            .map(|d| d.dir_name.clone())
            .unwrap_or_else(|| system_id.to_string());
        let folder = systems_dir
            .as_ref()
            .map(|d| d.join(&dir_name))
            .unwrap_or_else(|| std::path::PathBuf::from(&dir_name));

        // Resolve pythonw.
        let pythonw = resolve_pythonw();

        let bot_py = folder.join("bot.py");

        // A fresh consola.log per start: the previous run's errors move to
        // consola.anterior.log instead of reading as today's.
        let consola_path = folder.join("consola.log");
        if std::fs::metadata(&consola_path).is_ok_and(|m| m.len() > 0) {
            let _ = std::fs::rename(&consola_path, folder.join("consola.anterior.log"));
        }
        let consola_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&consola_path)
            .map_err(|e| TradingError::InvalidRequest {
                message: format!("no se pudo abrir consola.log: {e}"),
            })?;

        // Seed offsets.
        let today_prefix = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let registro_path = folder.join("registro.log");
        let reg_offset = crate::lifecycle::today_offset(&registro_path, &today_prefix);
        let con_offset = crate::lifecycle::file_tail_size(&consola_path);

        // A bot trades on MetaTrader 5 while an MT5 account is registered:
        // OpenBot's ib_async stand-in, its token and the bots' API go in its
        // environment (crates/trading/src/mt5/bots.rs); otherwise TWS.
        let mt5 = BOT_ENVIRONMENT
            .get()
            .and_then(|entorno| entorno(system_id, &folder));
        let mt5 = match mt5 {
            Some(Err(motivo)) => {
                state.push_log(
                    zeron_proto::trading::LogLevel::Error,
                    "motor",
                    format!("Bot {system_id} no arranca en MetaTrader 5: {motivo}"),
                    now_ms(),
                    true,
                );
                return Err(TradingError::InvalidRequest { message: motivo });
            }
            Some(Ok(entorno)) => Some(entorno),
            None => None,
        };

        // Spawn.
        let mut cmd = std::process::Command::new(&pythonw);
        cmd.arg(&bot_py)
            .current_dir(&folder)
            .env("PYTHONPATH", folder.to_string_lossy().to_string())
            .env("PYTHONIOENCODING", "utf-8")
            .env("OPENBOT_SYSTEM_ID", system_id)
            .env("OPENBOT_ENGINE_URL", "http://127.0.0.1:0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(consola_file));
        if let Some(entorno) = &mt5 {
            cmd.envs(entorno.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let child = cmd.spawn().map_err(|e| TradingError::InvalidRequest {
            message: format!("spawn failed: {e}"),
        })?;
        let pid = child.id();
        // Drop the handle; we track pid.
        drop(child);
        if pid == 0 {
            return Err(TradingError::InvalidRequest {
                message: "spawn failed: no pid".to_string(),
            });
        }

        // Write panel.pid.
        let _ = std::fs::write(folder.join("panel.pid"), pid.to_string());

        // Set runtime.
        let mut runtime = crate::lifecycle::runtime_nuevo();
        runtime.source = Some(crate::lifecycle::RuntimeSource::Panel);
        runtime.pid = Some(pid);
        runtime.started_at = Some(now_ms());
        runtime.registro_offset = reg_offset;
        runtime.consola_offset = con_offset;
        state.bots.insert(system_id.to_string(), runtime);
        state.notes.remove(system_id);
        state.adopted.remove(system_id);
        if state.wanted.insert(system_id.to_string()) {
            state.wanted_dirty = true;
        }

        let destino = mt5
            .as_ref()
            .and_then(|e| e.iter().find(|(k, _)| k == "OPENBOT_MT5_LINK"))
            .map(|(_, enlace)| format!(" en MetaTrader 5 ({enlace})"))
            .unwrap_or_default();
        state.push_log(
            zeron_proto::trading::LogLevel::Info,
            "motor",
            format!("Bot {}{destino} arrancado", system_id),
            now_ms(),
            true,
        );
        drop(state);

        Ok(())
    }

impl TradingEngine {
    pub fn stop_system(&self, system_id: &str) -> Result<(), TradingError> {
        let outcome = stop_system_in(&self.state, &self.systems_dir, system_id);
        self.save_wanted();
        self.publish();
        outcome
    }
}

/// Stops one bot of the pool (pid-guarded). Shared with the task scheduler.
fn stop_system_in(
    state_arc: &Arc<Mutex<TradingState>>,
    systems_dir: &Option<PathBuf>,
    system_id: &str,
) -> Result<(), TradingError> {
    let mut state = state_arc.lock().unwrap();
        if !state.systems.iter().any(|d| d.id == system_id) {
            return Err(TradingError::SystemNotFound {
                system_id: system_id.to_string(),
            });
        }
        if state.wanted.remove(system_id) {
            state.wanted_dirty = true;
        }
        // Stopped by hand: the post-restart resume must not bring it back.
        state.adopted.remove(system_id);
        let running = state.bots.get(system_id).map(|r| r.started_at.is_some()).unwrap_or(false);
        if !running {
            // Already stopped: keep the note saying why it stopped.
            return Ok(());
        }
        state.notes.remove(system_id);

        let systems_dir = state.systems_dir.clone().or(systems_dir.clone());
        let dossier = state.systems.iter().find(|d| d.id == system_id).cloned();
        let dir_name = dossier
            .as_ref()
            .map(|d| d.dir_name.clone())
            .unwrap_or_else(|| system_id.to_string());
        let folder = systems_dir
            .as_ref()
            .map(|d| d.join(&dir_name))
            .unwrap_or_else(|| std::path::PathBuf::from(&dir_name));

        let pid = state.bots.get(system_id).and_then(|r| r.pid);

        if let Some(pid) = pid {
            match pid_vivo_sync(pid) {
                None => {
                    drop(state);
                    return Err(TradingError::InvalidRequest {
                        message: "no se pudo confirmar".to_string(),
                    });
                }
                Some(false) => {
                    // already dead; clean up
                }
                Some(true) => {
                    let es_bot = pid_es_bot_sync(pid, Some(&folder));
                    match es_bot {
                        None => {
                            // Unverified: nothing was stopped, and saying so
                            // matters (a proposal or a task would otherwise
                            // report the bot as stopped while it runs).
                            state.push_log(
                                zeron_proto::trading::LogLevel::Warning,
                                "motor",
                                format!("Bot {} no se pudo verificar; sigue en marcha", system_id),
                                now_ms(),
                                true,
                            );
                            drop(state);
                            return Err(TradingError::InvalidRequest {
                                message: format!(
                                    "no se pudo verificar que el proceso {pid} sea el bot; no se ha parado"
                                ),
                            });
                        }
                        Some(false) => {
                            // recycled; clean up
                        }
                        Some(true) => {
                            let _ = std::process::Command::new("taskkill")
                                .args(["/PID", &pid.to_string(), "/T", "/F"])
                                .output();
                            match pid_vivo_sync(pid) {
                                Some(false) => {}
                                _ => {
                                    state.push_log(
                                        zeron_proto::trading::LogLevel::Warning,
                                        "motor",
                                        format!("Bot {} sigue vivo", system_id),
                                        now_ms(),
                                        true,
                                    );
                                    drop(state);
                                    return Err(TradingError::InvalidRequest {
                                        message: "no se pudo confirmar".to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        } else {
            // Running without a known process: there is nothing the desk can
            // stop. A panel row without a pid is stale (the sweep retires
            // it); one started elsewhere must be stopped there.
            let es_panel = state.bots.get(system_id).and_then(|r| r.source)
                == Some(crate::lifecycle::RuntimeSource::Panel);
            if !es_panel {
                state.push_log(
                    zeron_proto::trading::LogLevel::Warning,
                    "motor",
                    format!("Bot {} externo sin PID; sigue en marcha", system_id),
                    now_ms(),
                    true,
                );
                drop(state);
                return Err(TradingError::InvalidRequest {
                    message: "el bot se arrancó fuera de Aurea y no se conoce su proceso; páralo desde donde se arrancó".into(),
                });
            }
        }

        // cleanup
        let _ = std::fs::remove_file(folder.join("panel.pid"));
        state.bots.remove(system_id);
        state.push_log(
            zeron_proto::trading::LogLevel::Info,
            "motor",
            format!("Bot {} parado", system_id),
            now_ms(),
            true,
        );
        drop(state);
        Ok(())
    }

impl TradingEngine {
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
            .map_err(|_msg| TradingError::TaskNotFound {
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

    /// Where the systems come from (a repository folder, Settings → Sistemas)
    /// and what its last download brought.
    pub fn repo_state(&self) -> zeron_proto::trading::RepoState {
        let read = |key: &str| self.store.meta_get(key).ok().flatten();
        zeron_proto::trading::RepoState {
            source: read("repo-sistemas").and_then(|text| serde_json::from_str(&text).ok()),
            last: read("repo-sistemas-ultima").and_then(|text| serde_json::from_str(&text).ok()),
        }
    }

    /// Use a repository download as the systems folder and remember its source.
    pub fn adopt_repo(
        &mut self,
        source: &zeron_proto::trading::RepoSource,
        report: &zeron_proto::trading::RepoSyncReport,
    ) -> Result<(), TradingError> {
        self.pool_select(&report.dir)?;
        if let Ok(text) = serde_json::to_string(source) {
            let _ = self.store.meta_set("repo-sistemas", &text);
        }
        if let Ok(text) = serde_json::to_string(report) {
            let _ = self.store.meta_set("repo-sistemas-ultima", &text);
        }
        let message = if report.changed.is_empty() {
            format!("Sistemas del repositorio al día ({})", report.commit)
        } else {
            format!(
                "Sistemas actualizados desde el repositorio ({}): {} ficheros cambiados",
                report.commit,
                report.changed.len()
            )
        };
        self.note(zeron_proto::trading::LogLevel::Info, message);
        // A bot that keeps running keeps the code it started with.
        let tocados: Vec<String> = {
            let state = self.state.lock().unwrap();
            state
                .systems
                .iter()
                .filter(|d| state.bots.get(&d.id).is_some_and(|r| r.started_at.is_some()))
                .filter(|d| {
                    report
                        .changed
                        .iter()
                        .any(|path| path.split('/').any(|part| part == d.dir_name))
                })
                .map(|d| d.id.clone())
                .collect()
        };
        if !tocados.is_empty() {
            self.note(
                zeron_proto::trading::LogLevel::Warning,
                format!(
                    "Sistemas en marcha con código nuevo: {}. Siguen con el anterior hasta que los pares y arranques",
                    tocados.join(", ")
                ),
            );
        }
        Ok(())
    }

    /// A local folder replaces the repository: forget its source.
    pub fn forget_repo(&mut self) {
        let _ = self.store.meta_set("repo-sistemas", "");
        let _ = self.store.meta_set("repo-sistemas-ultima", "");
    }

    /// Whether any bot runs now (its code must not change under it).
    pub fn any_system_running(&self) -> bool {
        self.snapshot().systems.iter().any(|system| system.running)
    }

    /// Whether a systems folder is configured (it may hold no systems yet).
    /// Without one the desk is the demo fixture, so its actions must not go to
    /// the (empty) native engine.
    pub fn has_pool(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.systems_dir.is_some() || !state.systems.is_empty()
    }

    /// One engine-log row from the desk itself.
    pub fn note(&self, level: zeron_proto::trading::LogLevel, message: String) {
        self.state.lock().unwrap().push_log(level, "motor", message, now_ms(), true);
        self.publish();
    }

    /// The systems folder in use and how many systems it holds (`None` when
    /// none is chosen: the desk shows the demo pool).
    pub fn pool_current(&self) -> Option<PoolSelectResult> {
        let dir = self
            .state
            .lock()
            .unwrap()
            .systems_dir
            .clone()
            .or_else(|| self.systems_dir.clone())?;
        Some(PoolSelectResult {
            systems: pool::scan_dirs(&dir).len() as u32,
            dir: dir.to_string_lossy().into_owned(),
        })
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

    /// Adopts a bot started outside the engine (via panel.pid or TWS probe).
    pub fn adopt_system(&self, system_id: &str) {
        let mut state = self.state.lock().unwrap();
        let dossier = state.systems.iter().find(|d| d.id == system_id).cloned();
        let Some(dossier) = dossier else { return; };
        let dir_name = dossier.dir_name.clone();
        let systems_dir = state.systems_dir.clone().unwrap_or_default();
        let folder = systems_dir.join(&dir_name);
        let panel_pid_path = folder.join("panel.pid");

        // Try panel.pid first.
        if let Ok(contents) = std::fs::read_to_string(&panel_pid_path) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                if pid > 0 {
                    // We can't call pid_vivo/pid_es_bot sync easily without tokio runtime
                    // in all contexts; just adopt if pid > 0 (pragmatic for V1).
                    let runtime = crate::lifecycle::runtime_nuevo();
                    let mut rt = runtime;
                    rt.source = Some(crate::lifecycle::RuntimeSource::Panel);
                    rt.pid = Some(pid);
                    rt.started_at = Some(now_ms());
                    state.bots.insert(system_id.to_string(), rt);
                    state.push_log(
                        zeron_proto::trading::LogLevel::Info,
                        "motor",
                        format!("Bot {} adoptado (panel.pid={})", system_id, pid),
                        now_ms(),
                        true,
                    );
                    drop(state);
                    self.publish();
                    return;
                }
            }
        }

        // Fallback: if system has client_id, adopt as external (pragmatic).
        if dossier.client_id.is_some() {
            let runtime = crate::lifecycle::runtime_nuevo();
            let mut rt = runtime;
            rt.source = Some(crate::lifecycle::RuntimeSource::External);
            rt.pid = None;
            rt.started_at = Some(now_ms());
            state.bots.insert(system_id.to_string(), rt);
            state.push_log(
                zeron_proto::trading::LogLevel::Info,
                "motor",
                format!("Bot {} adoptado (externo, client_id)", system_id),
                now_ms(),
                true,
            );
            drop(state);
            self.publish();
        }
    }

    /// 45s sweep: retire bots whose process is gone, adopt the ones a live
    /// `panel.pid` points at. A `tasklist` answer of "unknown" retires nobody:
    /// a bot left running outside the panel must never be forgotten.
    pub fn sweep(&self) {
        let snap = {
            let mut state = self.state.lock().unwrap();
            sweep_state_in(&mut state);
            state.snapshot(now_ms())
        };
        publish_if_changed(&self.watch, snap);
    }

    pub fn shutdown(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn publish(&self) {
        let snap = self.state.lock().unwrap().snapshot(now_ms());
        publish_if_changed(&self.watch, snap);
    }
}

/// Where a bot trades: given its system id and folder, `None` = TWS (no MT5
/// account registered), `Some(Ok(env))` = MetaTrader 5 with that extra
/// environment, `Some(Err(why))` = MT5 is registered but unreachable.
pub type BotEnvironment = Box<
    dyn Fn(&str, &Path) -> Option<std::result::Result<Vec<(String, String)>, String>> + Send + Sync,
>;

static BOT_ENVIRONMENT: std::sync::OnceLock<BotEnvironment> = std::sync::OnceLock::new();

/// Set once by the engine's assembly (the desk's MT5 side).
pub fn set_bot_environment(entorno: BotEnvironment) {
    let _ = BOT_ENVIRONMENT.set(entorno);
}

/// Publishes `snap` only when it differs from what the watchers last saw.
/// The scheduler ticks every second: re-sending an identical desk made every
/// window redraw (and, without a GPU, each redraw is real CPU).
pub(crate) fn publish_if_changed(watch: &watch::Sender<Snapshot>, snap: Snapshot) {
    watch.send_if_modified(|current| {
        if *current == snap {
            false
        } else {
            *current = snap;
            true
        }
    });
}

/// One sweep pass over the desk's runtimes (the caller publishes the result).
fn sweep_state_in(state: &mut TradingState) {
    let now = now_ms();
    let systems_dir = state.systems_dir.clone().unwrap_or_default();

    let mut to_remove: Vec<(String, Option<u32>)> = Vec::new();
    for (id, runtime) in state.bots.iter() {
        let es_panel = runtime.source == Some(crate::lifecycle::RuntimeSource::Panel);
        let vivo = runtime.pid.and_then(pid_vivo_sync);
        if sweep_debe_retirar(runtime.started_at.is_some(), runtime.pid, es_panel, vivo) {
            to_remove.push((id.clone(), runtime.pid));
        }
    }
    for (id, pid) in to_remove {
        state.bots.remove(&id);
        state.adopted.remove(&id);
        let mut nota = None;
        if let Some(dir_name) = state
            .systems
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.dir_name.clone())
        {
            let folder = systems_dir.join(dir_name);
            let _ = std::fs::remove_file(folder.join("panel.pid"));
            // The runtime's offsets already sit at the end (the log reader
            // moved them): read today's lines, and this run's consola (each
            // start rotates it).
            let hoy = chrono::Local::now().format("%Y-%m-%d").to_string();
            let registro_offset = crate::lifecycle::today_offset(&folder.join("registro.log"), &hoy);
            nota = stop_reason(&folder, registro_offset, 0);
        }
        let detalle = pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
        let (level, message) = match &nota {
            Some(nota) if nota.starts_with("falló") => (
                zeron_proto::trading::LogLevel::Error,
                format!("Bot {id} {nota}{detalle}"),
            ),
            Some(nota) => (zeron_proto::trading::LogLevel::Info, format!("Bot {id} {nota}{detalle}")),
            None => (zeron_proto::trading::LogLevel::Info, format!("Bot {id} terminó{detalle}")),
        };
        state.push_log(level, "motor", message, now, true);
        // Ended on its own terms (done for today): reopening Aurea must not
        // start it again. One that failed stays wanted, for another try.
        if !nota.as_deref().is_some_and(|n| n.starts_with("falló")) && state.wanted.remove(&id) {
            state.wanted_dirty = true;
        }
        if let Some(nota) = nota {
            state.notes.insert(id.clone(), nota);
        }
    }

    // Adopt a bot whose `panel.pid` points at a live process: it was started
    // outside the panel (or the app restarted under it). A stale file goes.
    let system_ids: Vec<String> = state.systems.iter().map(|d| d.id.clone()).collect();
    for id in system_ids {
        if state.bots.contains_key(&id) {
            continue;
        }
        let Some(dir_name) = state
            .systems
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.dir_name.clone())
        else {
            continue;
        };
        let panel_pid_path = systems_dir.join(&dir_name).join("panel.pid");
        let Ok(raw) = std::fs::read_to_string(&panel_pid_path) else {
            continue;
        };
        let Some(pid) = raw.trim().parse::<u32>().ok() else {
            let _ = std::fs::remove_file(&panel_pid_path);
            continue;
        };
        if pid_vivo_sync(pid) != Some(true) {
            let _ = std::fs::remove_file(&panel_pid_path);
            continue;
        }
        // Windows reuses process ids: a live pid is only this bot when its
        // command line says so (the check Stop uses). Another program means
        // the file is stale; no answer means asking again next sweep.
        match pid_es_bot_sync(pid, Some(&systems_dir.join(&dir_name))) {
            Some(true) => {}
            Some(false) => {
                let _ = std::fs::remove_file(&panel_pid_path);
                continue;
            }
            None => continue,
        }
        let mut runtime = crate::lifecycle::runtime_nuevo();
        runtime.source = Some(crate::lifecycle::RuntimeSource::Panel);
        runtime.pid = Some(pid);
        runtime.started_at = Some(now);
        // Adopt at the end of the logs, like a fresh start: re-reading the
        // whole `registro.log` would replay trades the desk already booked.
        let folder = systems_dir.join(&dir_name);
        let today_prefix = chrono::Utc::now().format("%Y-%m-%d").to_string();
        runtime.registro_offset = crate::lifecycle::today_offset(
            &folder.join("registro.log"),
            &today_prefix,
        );
        runtime.consola_offset = crate::lifecycle::file_tail_size(&folder.join("consola.log"));
        state.bots.insert(id.clone(), runtime);
        state.adopted.insert(id.clone());
        state.push_log(
            zeron_proto::trading::LogLevel::Info,
            "motor",
            format!("Bot {id} adoptado por sweep (pid {pid})"),
            now,
            true,
        );
    }
}

/// Where the systems the person wants running are kept in the store.
const WANTED_KEY: &str = "bots-en-marcha";
/// A restart waits this long (the sweep adopts what is still alive, MT5
/// reconnects) before bringing bots back…
const RESUME_DELAY_MS: i64 = 4_000;
/// …and keeps trying this long while MT5 is not ready yet.
const RESUME_WINDOW_MS: i64 = 5 * 60_000;

/// One pass of the post-restart resume. Bots the person had running come
/// back; a bot adopted from the previous run that trades on MT5 is restarted,
/// since its token died with that run. Returns whether to try again.
fn resume_step(state_arc: &Arc<Mutex<TradingState>>, systems_dir: &Option<PathBuf>, last: bool) -> bool {
    let (wanted, adopted, folders) = {
        let state = state_arc.lock().unwrap();
        let dir = state.systems_dir.clone().or(systems_dir.clone());
        let folders: std::collections::HashMap<String, PathBuf> = state
            .systems
            .iter()
            .filter_map(|d| dir.as_ref().map(|dir| (d.id.clone(), dir.join(&d.dir_name))))
            .collect();
        let wanted: Vec<String> = state
            .wanted
            .iter()
            .filter(|id| folders.contains_key(*id))
            .filter(|id| !state.bots.get(*id).is_some_and(|r| r.started_at.is_some()))
            .cloned()
            .collect();
        let adopted: Vec<String> = state.adopted.iter().cloned().collect();
        (wanted, adopted, folders)
    };
    let mut pending = false;
    let mut fallos = Vec::new();
    for id in adopted.iter().chain(wanted.iter()) {
        let Some(folder) = folders.get(id) else { continue };
        let mt5 = BOT_ENVIRONMENT.get().and_then(|entorno| entorno(id, folder));
        if let Some(Err(motivo)) = &mt5 {
            pending = true;
            fallos.push(format!("{id}: {motivo}"));
            continue;
        }
        let reinicio = adopted.contains(id);
        if reinicio {
            if mt5.is_none() {
                // A TWS bot keeps its connection across a restart.
                state_arc.lock().unwrap().adopted.remove(id);
                continue;
            }
            if let Err(error) = stop_system_in(state_arc, systems_dir, id) {
                fallos.push(format!("{id}: {error}"));
                continue;
            }
        }
        match start_system_in(state_arc, systems_dir, id) {
            Ok(()) => state_arc.lock().unwrap().push_log(
                zeron_proto::trading::LogLevel::Info,
                "motor",
                if reinicio {
                    format!("Bot {id} reiniciado para reconectarlo a MetaTrader 5 tras reabrir Aurea")
                } else {
                    format!("Bot {id} rearrancado: estaba en marcha al cerrar Aurea")
                },
                now_ms(),
                true,
            ),
            Err(error) => fallos.push(format!("{id}: {error}")),
        }
    }
    if pending && !last {
        return true;
    }
    if !fallos.is_empty() {
        state_arc.lock().unwrap().push_log(
            zeron_proto::trading::LogLevel::Error,
            "motor",
            format!("No se pudieron rearrancar algunos bots — {}", fallos.join(" · ")),
            now_ms(),
            true,
        );
    }
    false
}

/// Why a bot that exited on its own stopped: the error it died with (from
/// `consola.log`) or its own last word (from `registro.log`), as a short note.
fn stop_reason(folder: &Path, registro_offset: u64, consola_offset: u64) -> Option<String> {
    fn tail(path: &Path, from: u64) -> String {
        let Ok(bytes) = std::fs::read(path) else {
            return String::new();
        };
        let from = usize::try_from(from).unwrap_or(0).min(bytes.len());
        String::from_utf8_lossy(&bytes[from..]).into_owned()
    }
    fn short(text: &str) -> String {
        let text = text.trim();
        match text.char_indices().nth(140) {
            Some((cut, _)) => format!("{}…", &text[..cut]),
            None => text.to_string(),
        }
    }
    let consola = tail(&folder.join("consola.log"), consola_offset);
    if consola.contains("Traceback") {
        let ultima = consola
            .lines()
            .rev()
            .map(str::trim_end)
            .find(|l| !l.is_empty() && !l.starts_with(' ') && !l.contains("Warning"))?;
        return Some(format!("falló: {}", short(ultima)));
    }
    const GENERICAS: [&str; 5] = [
        "desconectado",
        "la estrategia ha terminado por hoy",
        "disconnecting",
        "disconnected",
        "conectado a ",
    ];
    let registro = tail(&folder.join("registro.log"), registro_offset);
    let ultima = registro
        .lines()
        .rev()
        .filter_map(parse_bot_line)
        .map(|(_, msg)| msg)
        .find(|msg| {
            let low = msg.to_lowercase();
            !GENERICAS.iter().any(|g| low.starts_with(g))
        })?;
    // The bots prefix their lines with the strategy's name.
    let ultima = match ultima.strip_prefix('[').and_then(|r| r.split_once("] ")) {
        Some((_, resto)) => resto.to_string(),
        None => ultima,
    };
    Some(format!("terminó: {}", short(&ultima)))
}

/// The sweep's verdict on one runtime: a live process is never retired, and an
/// unknown answer from `tasklist` never retires anyone either.
fn sweep_debe_retirar(started: bool, pid: Option<u32>, es_panel: bool, vivo: Option<bool>) -> bool {
    if !started {
        return false;
    }
    match pid {
        Some(_) => vivo == Some(false),
        None => es_panel,
    }
}

fn resolve_pythonw() -> String {
    // Try `where pythonw`.
    if let Ok(output) = std::process::Command::new("where").arg("pythonw").output() {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(first) = stdout.lines().next() {
                let trimmed = first.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }
    "pythonw".to_string()
}

fn pid_vivo_sync(pid: u32) -> Option<bool> {
    let mut cmd = std::process::Command::new("tasklist");
    cmd.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let output = cmd.output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return None;
    }
    Some(stdout.contains(&format!("\"{pid}\"")))
}

fn pid_es_bot_sync(pid: u32, dir: Option<&std::path::Path>) -> Option<bool> {
    let filter = format!("ProcessId={pid}");
    let mut cmd = std::process::Command::new("powershell");
    cmd.args([
        "-NoProfile",
        "-Command",
        &format!(
            "Get-CimInstance Win32_Process -Filter \"{}\" | Select-Object -ExpandProperty CommandLine",
            filter
        ),
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let output = cmd.output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let panel_contents = match dir {
        Some(d) => std::fs::read_to_string(d.join("panel.pid")).ok(),
        None => None,
    };
    crate::lifecycle::pid_es_bot_decision(pid, &stdout, panel_contents.as_deref(), dir)
}

fn ingest_one(state: &Arc<Mutex<TradingState>>, system_id: &str, systems_dir: Option<&Path>) {
    use std::path::PathBuf;
    // Resolve folder and offsets.
    let (folder, mut reg_offset, mut con_offset) = {
        let st = state.lock().unwrap();
        let base = systems_dir
            .map(PathBuf::from)
            .or(st.systems_dir.clone());
        let dossier = st.systems.iter().find(|d| d.id == system_id);
        let dir_name = dossier
            .map(|d| d.dir_name.clone())
            .unwrap_or_else(|| system_id.to_string());
        let folder = base
            .map(|b| b.join(&dir_name))
            .unwrap_or_else(|| PathBuf::from(&dir_name));
        let rt = st.bots.get(system_id);
        let (ro, co) = rt
            .map(|r| (r.registro_offset, r.consola_offset))
            .unwrap_or((0, 0));
        (folder, ro, co)
    };

    let reg_path = folder.join("registro.log");
    if let Ok(meta) = std::fs::metadata(&reg_path) {
        let size = meta.len();
        if size > reg_offset {
            if let Ok(bytes) = std::fs::read(&reg_path) {
                let start = reg_offset as usize;
                let to_read = ((size - reg_offset) as usize).min(1_000_000);
                let end = (start + to_read).min(bytes.len());
                let chunk = &bytes[start..end];
                let text = String::from_utf8_lossy(chunk);
                let mut last_ts = now_ms();
                for line in text.lines() {
                    if let Some((ts, msg)) = parse_bot_line(line) {
                        last_ts = ts;
                        if is_sorro(&msg) {
                            if let Some(v) = parse_unrealized(&msg) {
                                let mut st = state.lock().unwrap();
                                st.pnl_bots.insert(system_id.to_string(), v);
                            }
                            continue;
                        }
                        {
                            let mut st = state.lock().unwrap();
                            st.push_log(
                                zeron_proto::trading::LogLevel::Info,
                                system_id.to_string(),
                                msg.clone(),
                                ts,
                                false,
                            );
                            if is_close(&msg) {
                                let pnl = parse_trade_pnl(&msg);
                                st.register_trade(system_id.to_string(), ts, pnl, msg.clone());
                            }
                        }
                    } else if line.starts_with(char::is_whitespace) {
                        // continuation; skip
                    } else if line.contains("Traceback") || line.contains("Exception") {
                        let mut st = state.lock().unwrap();
                        st.push_log(
                            zeron_proto::trading::LogLevel::Error,
                            system_id.to_string(),
                            line.to_string(),
                            last_ts,
                            false,
                        );
                    }
                }
                reg_offset += (end - start) as u64;
            }
        }
    }

    // consola.log: update offset only.
    let con_path = folder.join("consola.log");
    if let Ok(meta) = std::fs::metadata(&con_path) {
        let size = meta.len();
        if size > con_offset {
            con_offset = size;
        }
    }

    let mut st = state.lock().unwrap();
    if let Some(rt) = st.bots.get_mut(system_id) {
        rt.registro_offset = reg_offset;
        rt.consola_offset = con_offset;
    }
}

fn parse_bot_line(line: &str) -> Option<(i64, String)> {
    if line.len() < 24 {
        return None;
    }
    let bytes = line.as_bytes();
    if bytes[23] != b' ' {
        return None;
    }
    let ts_part = &line[..23];
    let msg = line[24..].to_string();
    let fixed = ts_part.replacen(',', ".", 1);
    let dt = chrono::NaiveDateTime::parse_from_str(&fixed, "%Y-%m-%d %H:%M:%S%.3f").ok()?;
    // Python's logging writes `asctime` in the machine's local time.
    use chrono::TimeZone;
    let millis = chrono::Local
        .from_local_datetime(&dt)
        .earliest()
        .map_or_else(|| dt.and_utc().timestamp_millis(), |t| t.timestamp_millis());
    Some((millis, msg))
}

fn is_sorro(msg: &str) -> bool {
    let patterns = [
        "updatePortfolio:",
        "Disconnecting",
        "position: ",
        "Connecting",
        "Connected",
        "Logged on",
        "API connection",
        "Synchronization",
        "connectRequest",
        "commissionReport:",
        "execDetails ",
        "openOrder:",
        "orderStatus:",
    ];
    if patterns.iter().any(|p| msg.contains(p)) {
        return true;
    }
    if let Some(idx) = msg.find("Warning ") {
        let rest = &msg[idx + "Warning ".len()..];
        if rest.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
            return true;
        }
    }
    is_ib_link_noise(msg)
}

/// IB API chatter about the link and the request budget, as each bot's own
/// `ib_async` session reports it. The desk's log is the desk's: the bots keep
/// these in their own `registro.log`. They are:
///
/// - `1100`/`1102` — TWS lost/restored its link to IBKR (a farm heartbeat).
/// - `322` — `ib_async` resubscribes the account summary on every `1102`
///   without cancelling the previous one, until TWS refuses more.
/// - `162` — TWS's trading session changed IP, so historical data is refused
///   for that bot (WARP/another login); the bot then logs its own decision
///   error, which is the actionable line.
/// - `10339` — `ib_async` sends an end date for a continuous future.
fn is_ib_link_noise(msg: &str) -> bool {
    let Some(rest) = msg.strip_prefix("Error ") else {
        return false;
    };
    let code = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    matches!(
        code.parse::<u32>().ok(),
        Some(162 | 322 | 1100 | 1102 | 10339)
    )
}

fn is_close(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    msg.contains("FUERA") || lower.contains("posición cerrada") || lower.contains("posicion cerrada")
}

fn parse_trade_pnl(msg: &str) -> Option<f64> {
    // find '(' ... ' USD brutos)'
    if let Some(start) = msg.find('(') {
        let rest = &msg[start + 1..];
        if let Some(end) = rest.find(" USD brutos)") {
            let num = &rest[..end];
            return num.trim().parse::<f64>().ok();
        }
    }
    None
}

fn parse_unrealized(msg: &str) -> Option<f64> {
    if let Some(idx) = msg.find("unrealizedPNL=") {
        let rest = &msg[idx + "unrealizedPNL=".len()..];
        // take until whitespace
        let end = rest
            .find(|c: char| c.is_whitespace())
            .unwrap_or(rest.len());
        return rest[..end].parse::<f64>().ok();
    }
    None
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// How late a pending task may still run. Past this the engine was off at its
/// time: starting a bot hours after the person planned is not what they asked.
const TASK_GRACE_MS: i64 = 5 * 60_000;

fn task_is_missed(now: i64, run_at: i64) -> bool {
    now.saturating_sub(run_at) > TASK_GRACE_MS
}

/// The systems a task acts on: its own ids, or every system when it names none.
fn task_targets(state: &Arc<Mutex<TradingState>>, task: &Task) -> Vec<String> {
    if !task.system_ids.is_empty() {
        return task.system_ids.clone();
    }
    state
        .lock()
        .unwrap()
        .systems
        .iter()
        .map(|system| system.id.clone())
        .collect()
}

/// `HH:MM` on the PC clock.
fn local_clock(ms: i64) -> String {
    chrono::TimeZone::timestamp_millis_opt(&chrono::Local, ms)
        .single()
        .map(|at| at.format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".into())
}

/// Runs one due task and says how it ended: `Ran`, `Failed` (nothing could be
/// done) or `Missed` (its time passed while the desk was off). The note is the
/// engine-log line the person reads.
fn run_due_task(
    state: &Arc<Mutex<TradingState>>,
    systems_dir: &Option<PathBuf>,
    _cancel: &Arc<std::sync::atomic::AtomicBool>,
    task: &Task,
    now: i64,
) -> (TaskStatus, Option<String>) {
    let (verb, participle) = match task.kind {
        TaskKind::StartSystems => ("arrancar", "arrancado"),
        TaskKind::StopSystems => ("parar", "parado"),
    };
    let ids = task_targets(state, task);
    let label = if ids.is_empty() {
        "todos los sistemas".to_string()
    } else if ids.len() == 1 {
        format!("el sistema {}", ids[0])
    } else {
        format!("{} sistemas", ids.len())
    };
    if task_is_missed(now, task.run_at) {
        return (
            TaskStatus::Missed,
            Some(format!(
                "Tarea perdida: {verb} {label} a las {} (el motor no estaba en marcha).",
                local_clock(task.run_at)
            )),
        );
    }
    if ids.is_empty() {
        return (
            TaskStatus::Failed,
            Some(format!("Tarea fallida: no hay sistemas que {verb}.")),
        );
    }
    let mut done = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for id in &ids {
        let outcome = match task.kind {
            TaskKind::StartSystems => start_system_in(state, systems_dir, id),
            TaskKind::StopSystems => stop_system_in(state, systems_dir, id),
        };
        match outcome {
            Ok(()) => done += 1,
            Err(error) => failed.push(format!("{id}: {error}")),
        }
    }
    if failed.is_empty() {
        let done_label = if done == 1 {
            format!("1 sistema {participle}")
        } else {
            format!("{done} sistemas {participle}s")
        };
        (
            TaskStatus::Ran,
            Some(format!("Tarea programada: {done_label}.")),
        )
    } else if done == 0 {
        (
            TaskStatus::Failed,
            Some(format!(
                "Tarea fallida: no se pudo {verb} {label}: {}",
                failed.join("; ")
            )),
        )
    } else {
        (
            TaskStatus::Failed,
            Some(format!(
                "Tarea a medias: {done} de {} sistemas {participle}s; fallaron: {}",
                ids.len(),
                failed.join("; ")
            )),
        )
    }
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
    async fn start_system_unknown_returns_error() {
        let engine = TradingEngine::new(None, None).unwrap();
        assert!(engine.start_system("99").is_err());
    }

    #[test]
    fn ib_link_chatter_never_reaches_the_desk_log() {
        // What the bots' own ib_async sessions report on a TWS↔IBKR blip.
        assert!(is_sorro(
            "Error 322, reqId 13: Error processing request.-'ReqAccountSummary(62)' : cause - Maximum number of account summary requests exceeded; desubscribe to previous request first"
        ));
        assert!(is_sorro(
            "Error 1100, reqId -1: Connectivity between IBKR and Trader Workstation has been lost."
        ));
        assert!(is_sorro(
            "Error 1102, reqId -1: Connectivity between IBKR and Trader Workstation has been restored - data maintained."
        ));
        assert!(is_sorro(
            "Error 162, reqId 14: Historical Market Data Service error message:Trading TWS session is connected from a different IP address, contract: Forex('USDJPY')"
        ));
        assert!(is_sorro(
            "Error 10339, reqId 83: Setting end date/time for continuous future security type is not allowed."
        ));
        // The desk's own decisions and real failures still land.
        assert!(!is_sorro("vela 2026-11-24 00:00 UTC | cierre 3660.2 | fuera -> nada"));
        assert!(!is_sorro("Error 201, reqId 9: Order rejected - reason: margin"));
        assert!(!is_sorro("error en la decision: 'ts'"));
    }

    #[test]
    fn the_sweep_retires_only_bots_whose_process_is_gone() {
        // A live process is never retired; an unknown answer never either.
        assert!(!sweep_debe_retirar(true, Some(7), true, Some(true)));
        assert!(!sweep_debe_retirar(true, Some(7), true, None));
        assert!(sweep_debe_retirar(true, Some(7), true, Some(false)));
        // A panel row without a pid cannot be proven alive.
        assert!(sweep_debe_retirar(true, None, true, None));
        // A row that never started, and an external one without a pid, are
        // left alone.
        assert!(!sweep_debe_retirar(false, None, true, None));
        assert!(!sweep_debe_retirar(true, None, false, None));
    }

    #[tokio::test]
    async fn the_chosen_systems_folder_outlasts_a_restart() {
        let seq = std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("aurea_pool_restart_{}_{}", std::process::id(), seq));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("sistemas");
        fs::create_dir_all(dir.join("01_uno")).unwrap();
        fs::create_dir_all(dir.join("02_dos")).unwrap();
        fs::create_dir_all(dir.join("notas")).unwrap();
        let db = root.join("trading.db");

        let mut engine = TradingEngine::new(None, Some(db.clone())).unwrap();
        assert_eq!(engine.pool_current(), None);
        engine.pool_select(dir.to_str().unwrap()).unwrap();
        drop(engine);

        let engine = TradingEngine::new(None, Some(db.clone())).unwrap();
        let current = engine.pool_current().expect("remembered folder");
        assert_eq!(current.dir, dir.to_string_lossy());
        assert_eq!(current.systems, 2);
        assert_eq!(engine.snapshot().systems.len(), 2);
        drop(engine);

        // A folder that no longer exists is forgotten, not fatal.
        fs::remove_dir_all(&dir).unwrap();
        let engine = TradingEngine::new(None, Some(db)).unwrap();
        assert_eq!(engine.pool_current(), None);
        drop(engine);
        let _ = fs::remove_dir_all(&root);
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

    #[test]
    fn a_task_that_passed_while_the_desk_was_off_is_missed() {
        let now = 1_000_000_000_000i64;
        assert!(!task_is_missed(now, now - 60_000), "a minute late still runs");
        assert!(task_is_missed(now, now - TASK_GRACE_MS - 1));
    }

    #[test]
    fn a_task_without_ids_targets_every_system() {
        let seq = std::sync::atomic::AtomicU64::new(0).fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("aurea_engine_targets_{}_{}", std::process::id(), seq));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("01_uno")).unwrap();
        fs::create_dir_all(dir.join("02_dos")).unwrap();

        let mut state = TradingState::new();
        state.scan_pool(&dir);
        let state = Arc::new(Mutex::new(state));

        let named = Task {
            id: "trd_named".into(),
            kind: TaskKind::StopSystems,
            system_ids: vec!["02".into()],
            run_at: 0,
            status: TaskStatus::Pending,
            resolved_at: None,
            time_created: 0,
            time_updated: 0,
        };
        assert_eq!(task_targets(&state, &named), ["02"]);

        let every = Task { system_ids: vec![], ..named };
        let mut ids = task_targets(&state, &every);
        ids.sort();
        assert_eq!(ids, ["01", "02"], "no ids means the whole pool");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_due_task_that_cannot_run_is_failed_and_names_the_system() {
        let state = Arc::new(Mutex::new(TradingState::new()));
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let now = now_ms();
        let task = Task {
            id: "trd_missing".into(),
            kind: TaskKind::StartSystems,
            system_ids: vec!["99".into()],
            run_at: now,
            status: TaskStatus::Pending,
            resolved_at: None,
            time_created: now,
            time_updated: now,
        };
        let (status, note) = run_due_task(&state, &None, &cancel, &task, now);
        assert_eq!(status, TaskStatus::Failed);
        let note = note.unwrap();
        assert!(note.contains("99"), "{note}");
        assert!(note.contains("no se pudo arrancar"), "{note}");
    }

    #[tokio::test]
    async fn the_scheduler_never_calls_a_task_done_when_it_could_not_run_it() {
        let mut engine = TradingEngine::new(None, None).unwrap();
        // An empty pool: the task is due, there is nothing to start, so the
        // desk must say failed (before, it marked every due task ran).
        let fired: Arc<Mutex<Vec<(String, TaskStatus, String)>>> = Arc::default();
        let sink = fired.clone();
        engine.set_task_listener(Arc::new(move |task, status, note| {
            sink.lock()
                .unwrap()
                .push((task.id.clone(), status, note.to_owned()));
        }));
        let task = engine
            .task_add(&TaskAddInput {
                kind: TaskKind::StartSystems,
                system_ids: Some(vec![]),
                delay_minutes: Some(0.02),
                at_time: None,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let snap = engine.snapshot();
        let stored = snap.tasks.iter().find(|t| t.id == task.id).unwrap();
        assert_eq!(stored.status, TaskStatus::Failed, "{stored:?}");
        assert!(
            snap.logs
                .iter()
                .any(|log| log.message.contains("no hay sistemas")),
            "{:?}",
            snap.logs.last()
        );
        // The person is told, once, with the same sentence as the log.
        let fired = fired.lock().unwrap();
        assert_eq!(fired.len(), 1, "{fired:?}");
        assert_eq!(fired[0].0, task.id);
        assert_eq!(fired[0].1, TaskStatus::Failed);
        assert!(fired[0].2.contains("no hay sistemas"), "{:?}", fired[0]);
    }

    #[tokio::test]
    async fn a_task_whose_time_passed_while_aurea_was_off_rings_once() {
        let mut engine = TradingEngine::new(None, None).unwrap();
        let fired: Arc<Mutex<Vec<(TaskStatus, String)>>> = Arc::default();
        let sink = fired.clone();
        engine.set_task_listener(Arc::new(move |_task, status, note| {
            sink.lock().unwrap().push((status, note.to_owned()));
        }));
        let task = engine
            .task_add(&TaskAddInput {
                kind: TaskKind::StopSystems,
                system_ids: Some(vec![]),
                delay_minutes: Some(5.0),
                at_time: None,
            })
            .unwrap();
        // As if its time had passed long before this engine was watching.
        {
            let store = engine.store();
            let conn = store.conn();
            conn.lock().unwrap().execute(
                "UPDATE trading_task SET run_at=?1 WHERE id=?2",
                rusqlite::params![now_ms() - TASK_GRACE_MS - 60_000, task.id],
            )
            .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(2500)).await;
        let fired = fired.lock().unwrap();
        assert_eq!(fired.len(), 1, "{fired:?}");
        assert_eq!(fired[0].0, TaskStatus::Missed);
        assert!(fired[0].1.contains("Tarea perdida"), "{:?}", fired[0]);
        // It is not run late: the stored task stays missed.
        let snap = engine.snapshot();
        let stored = snap.tasks.iter().find(|t| t.id == task.id).unwrap();
        assert_eq!(stored.status, TaskStatus::Missed, "{stored:?}");
    }

    #[test]
    fn stop_reason_reads_the_bots_last_word_without_the_generic_lines() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("registro.log"),
            [
                "2026-11-25 21:35:46,100 contexto: 87 sesiones",
                "2026-11-25 21:35:46,200 [Apertura Alcista] el bot ha arrancado despues de la hora de la senal: hoy no opera",
                "2026-11-25 21:35:47,300 la estrategia ha terminado por hoy",
                "2026-11-25 21:35:49,400 desconectado",
                "",
            ]
            .join("\n"),
        )
        .unwrap();
        fs::write(dir.path().join("consola.log"), "aviso de pandas\n").unwrap();

        let nota = stop_reason(dir.path(), 0, 0);

        assert_eq!(
            nota.as_deref(),
            Some("terminó: el bot ha arrancado despues de la hora de la senal: hoy no opera")
        );
    }

    #[test]
    fn stop_reason_reports_the_error_a_bot_died_with() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("registro.log"), "2026-11-25 21:35:46,100 contexto\n").unwrap();
        fs::write(
            dir.path().join("consola.log"),
            [
                "Traceback (most recent call last):",
                "  File \"bot.py\", line 1",
                "    ov.getresult()",
                "ConnectionRefusedError: [WinError 1225] refused",
                "C:/bot.py:256: Pandas4Warning: deprecated",
                "  df = df[x]",
                "",
            ]
            .join("\n"),
        )
        .unwrap();

        let nota = stop_reason(dir.path(), 0, 0);

        assert_eq!(nota.as_deref(), Some("falló: ConnectionRefusedError: [WinError 1225] refused"));
    }

    #[test]
    fn stop_reason_ignores_what_came_before_this_run() {
        let dir = tempfile::tempdir().unwrap();
        let viejo = "Traceback (most recent call last):\nValueError: viejo\n";
        fs::write(dir.path().join("consola.log"), viejo).unwrap();
        fs::write(dir.path().join("registro.log"), "").unwrap();

        assert_eq!(stop_reason(dir.path(), 0, viejo.len() as u64), None);
    }
}
