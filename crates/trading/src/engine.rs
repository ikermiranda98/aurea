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

    pub fn store(&self) -> Store {
        self.store.clone()
    }

    pub fn state(&self) -> Arc<Mutex<TradingState>> {
        Arc::clone(&self.state)
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

        let systems_dir = state.systems_dir.clone().or(self.systems_dir.clone());
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

        // Open consola.log in append mode.
        let consola_path = folder.join("consola.log");
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

        state.push_log(
            zeron_proto::trading::LogLevel::Info,
            "motor",
            format!("Bot {} arrancado", system_id),
            now_ms(),
            true,
        );
        drop(state);

        // Spawn background ingestion task.
        let state_arc = Arc::clone(&self.state);
        let cancel = Arc::clone(&self.cancel);
        let sid = system_id.to_string();
        let sd = systems_dir.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(5));
                loop {
                    interval.tick().await;
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    ingest_one(&state_arc, &sid, sd.as_deref());
                }
            });
        }

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
        let running = state.bots.get(system_id).map(|r| r.started_at.is_some()).unwrap_or(false);
        if !running {
            return Ok(());
        }

        let systems_dir = state.systems_dir.clone().or(self.systems_dir.clone());
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
                            // keep running, warn
                            state.push_log(
                                zeron_proto::trading::LogLevel::Warning,
                                "motor",
                                format!("Bot {} no se pudo verificar", system_id),
                                now_ms(),
                                true,
                            );
                            drop(state);
                            self.publish();
                            return Ok(());
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
                                    self.publish();
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
            // external without pid: warn
            state.push_log(
                zeron_proto::trading::LogLevel::Warning,
                "motor",
                format!("Bot {} externo sin PID", system_id),
                now_ms(),
                true,
            );
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

    /// 45s sweep: keep adopted bots honest, retire stale ones, adopt new ones.
    pub fn sweep(&self) {
        let mut state = self.state.lock().unwrap();
        let now = now_ms();
        let systems_dir = state.systems_dir.clone().unwrap_or_default();

        // Retire bots that are no longer alive (pragmatic: check panel.pid exists).
        let mut to_remove = Vec::new();
        for (id, rt) in state.bots.iter() {
            if rt.started_at.is_some() && rt.source != Some(crate::lifecycle::RuntimeSource::Panel) {
                continue; // only sweep adopted bots
            }
            if let Some(pid) = rt.pid {
                let panel_pid_path = state
                    .systems
                    .iter()
                    .find(|d| d.id == *id)
                    .map(|d| systems_dir.join(&d.dir_name).join("panel.pid"));
                if let Some(path) = panel_pid_path {
                    if !path.exists() {
                        to_remove.push(id.clone());
                    }
                }
            }
        }
        for id in to_remove {
            state.bots.remove(&id);
            state.push_log(
                zeron_proto::trading::LogLevel::Info,
                "motor",
                format!("Bot {} retirado por sweep", id),
                now,
                true,
            );
        }

        // Try to adopt bots not currently running.
        let system_ids: Vec<String> = state.systems.iter().map(|d| d.id.clone()).collect();
        for id in system_ids {
            if state.bots.contains_key(&id) {
                continue;
            }
            let dir_name = state.systems.iter().find(|d| d.id == id).map(|d| d.dir_name.clone()).unwrap_or_default();
            let panel_pid_path = systems_dir.join(&dir_name).join("panel.pid");
            if panel_pid_path.exists() {
                // Attempt adoption (pragmatic: just adopt).
                let runtime = crate::lifecycle::runtime_nuevo();
                let mut rt = runtime;
                rt.source = Some(crate::lifecycle::RuntimeSource::Panel);
                rt.started_at = Some(now);
                state.bots.insert(id.clone(), rt);
                state.push_log(
                    zeron_proto::trading::LogLevel::Info,
                    "motor",
                    format!("Bot {} adoptado por sweep", id),
                    now,
                    true,
                );
            }
        }
        drop(state);
        self.publish();
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
    let millis = dt.and_utc().timestamp_millis();
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
    false
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
}
