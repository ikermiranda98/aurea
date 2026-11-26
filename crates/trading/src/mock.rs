//! Fixture engine: a believable snapshot stream with no broker behind it.
//!
//! Used by the RPC wiring whenever the real engine is absent (Phase 1), so the
//! dashboard (lane B) and the visual QA (lane E) can build against real shapes.
//!
//! The fixture is not just static: the action surface (start/stop, close,
//! panic, tasks, links, account select) mutates an [`Overrides`] overlay and
//! republishes the snapshot immediately, keeping the dashboard interactive
//! while the real engine (lane A) lands.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{Local, TimeZone, Utc};
use tokio::sync::watch;
use zeron_proto::trading::*;

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// The static half of the fixture: systems, account, positions, tasks, links.
pub fn fixture() -> Snapshot {
    let now = now_ms();

    let systems = vec![
        System {
            id: "01_eurusd_mt5".into(),
            name: "EURUSD MT5".into(),
            running: true,
            active: true,
            pid: Some(4242),
            started_at: Some(now - 3_600_000),
            symbol: "EURUSD".into(),
            timeframe: "M15".into(),
            pnl_day: Some(127.40),
            capital: Some(12_500.0),
            pnl_pct: Some(1.02),
            client_id: None,
            close_hour: Some("23:59:00".into()),
            last_activity_at: Some(now - 60_000),
            description: Some("Demo: momentum intradía sobre EURUSD en MT5.".into()),
            rules: None,
            note: None,
        },
        System {
            id: "02_nq_tws".into(),
            name: "NQ TWS".into(),
            running: false,
            active: false,
            pid: None,
            started_at: None,
            symbol: "NQ".into(),
            timeframe: "M5".into(),
            pnl_day: Some(-42.10),
            capital: Some(8_000.0),
            pnl_pct: Some(-0.53),
            client_id: Some(4210),
            close_hour: Some("22:55:00".into()),
            last_activity_at: Some(now - 7_200_000),
            description: Some("Demo: apertura de sesión NY sobre NQ en TWS paper.".into()),
            rules: None,
            note: None,
        },
    ];

    let account = Account {
        id: "MT5-51234567".into(),
        kind: "MT5 · demo".into(),
        cash: Some(10_000.0),
        equity: Some(10_085.30),
        pnl_day: Some(85.30),
        pnl_realized: Some(42.10),
        available_funds: Some(9_940.0),
        demo: Some(true),
        buying_power: Some(99_400.0),
        margin_used: Some(60.0),
        excess_liquidity: Some(9_880.0),
        updated_at: Some(now),
        link: Some("mt5-51234567@AureaDemo".into()),
    };

    let positions = vec![
        Position {
            symbol: "EURUSD".into(),
            side: PositionSide::Long,
            size: 0.50,
            entry: Some(1.08412),
            current: Some(1.08573),
            pnl: Some(80.50),
            pnl_pct: Some(0.15),
        },
        Position {
            symbol: "NQ".into(),
            side: PositionSide::Long,
            size: 1.0,
            entry: Some(21_455.25),
            current: Some(21_519.50),
            pnl: Some(128.50),
            pnl_pct: Some(0.30),
        },
        Position {
            symbol: "XAUUSD".into(),
            side: PositionSide::Short,
            size: 0.10,
            entry: Some(4_221.30),
            current: Some(4_228.10),
            pnl: Some(-68.00),
            pnl_pct: Some(-0.16),
        },
    ];

    let tasks = vec![
        Task {
            id: "trd_mock0001".into(),
            kind: TaskKind::StopSystems,
            system_ids: vec![],
            run_at: now + 5_400_000,
            status: TaskStatus::Pending,
            resolved_at: None,
            time_created: now - 600_000,
            time_updated: now - 600_000,
        },
        Task {
            id: "trd_mock0002".into(),
            kind: TaskKind::StartSystems,
            system_ids: vec!["02_nq_tws".into()],
            run_at: now - 86_400_000,
            status: TaskStatus::Ran,
            resolved_at: Some(now - 86_340_000),
            time_created: now - 90_000_000,
            time_updated: now - 86_340_000,
        },
    ];

    let logs = vec![
        log(
            now - 12_000,
            LogLevel::Info,
            "01_eurusd_mt5",
            "Bot arrancado (pid 4242).",
            Some("trading.log.system_started"),
            Some(LogEvent::Open),
        ),
        log(
            now - 60_000,
            LogLevel::Info,
            "01_eurusd_mt5",
            "Señal de entrada LONG EURUSD @ 1.08412.",
            Some("trading.log.signal"),
            Some(LogEvent::Signal),
        ),
        log(
            now - 120_000,
            LogLevel::Warning,
            "motor",
            "TWS paper: reconexión en 5 s (socket cerrado por el broker).",
            Some("trading.log.tws_reconnect"),
            None,
        ),
        log(
            now - 240_000,
            LogLevel::Error,
            "02_nq_tws",
            "Bot detenido por el usuario.",
            Some("trading.log.system_stopped"),
            Some(LogEvent::Close),
        ),
        log(
            now - 600_000,
            LogLevel::Info,
            "motor",
            "Pool cargado: 2 sistemas.",
            Some("trading.log.pool_loaded"),
            None,
        ),
    ];

    let trades = vec![
        Trade {
            system: "01_eurusd_mt5".into(),
            ts: now - 3_600_000,
            pnl: Some(42.10),
            message: "Cierre LONG EURUSD @ 1.08500 (+42.10)".into(),
        },
        Trade {
            system: "02_nq_tws".into(),
            ts: now - 7_200_000,
            pnl: Some(-18.75),
            message: "Cierre LONG NQ @ 21420.00 (-18.75)".into(),
        },
    ];

    let equity = (0..24)
        .map(|i| EquityPoint {
            ts: now - (23 - i) as i64 * 900_000,
            equity: Some(10_000.0 + (i as f64) * 3.55),
            pnl_day: Some((i as f64) * 3.55),
        })
        .collect();

    let links = vec![
        Link {
            id: "mt5-51234567@AureaDemo".into(),
            host: None,
            port: None,
            label: None,
            broker: Some(Broker::Mt5),
            mt5_login: Some(51_234_567),
            mt5_server: Some("AureaDemo".into()),
            connected: true,
        },
        Link {
            id: "127.0.0.1:7497".into(),
            host: Some("127.0.0.1".into()),
            port: Some(7497),
            label: Some("TWS paper".into()),
            broker: Some(Broker::Ibkr),
            mt5_login: None,
            mt5_server: None,
            connected: true,
        },
    ];

    Snapshot {
        version: "0.1.0-mock".into(),
        connected: true,
        client_in_use: None,
        account: Some(account),
        systems,
        positions: positions.clone(),
        open_positions: Some(positions.clone()),
        closable_positions: Some(positions),
        logs,
        tasks,
        rules: None,
        trades: Some(trades),
        equity: Some(equity),
        accounts: None,
        links: Some(links),
        systems_dir: Some("C:\\Aurea\\sistemas".into()),
        programs: Some(Programs {
            tws: ProgramState::Running,
            mt5: Some(ProgramState::Running),
        }),
        proposals: None,
    }
}

fn log(
    ts: i64,
    level: LogLevel,
    system: &str,
    message: &str,
    key: Option<&str>,
    event: Option<LogEvent>,
) -> Log {
    Log {
        ts,
        level,
        system: system.into(),
        message: message.into(),
        message_key: key.map(Into::into),
        message_params: None,
        event,
    }
}

/// Everything the action surface changed on top of the base fixture.
#[derive(Default)]
struct Overrides {
    /// Explicit running flag per system id (last action wins).
    running: HashMap<String, bool>,
    /// Positions the user closed (or panic closed), by symbol.
    closed: BTreeSet<String>,
    /// Status overrides for the fixture's own tasks.
    task_status: HashMap<String, TaskStatus>,
    /// Tasks added at runtime, newest first.
    extra_tasks: Vec<Task>,
    /// Log rows authored by actions, newest first.
    extra_logs: Vec<Log>,
    /// Links added at runtime.
    extra_links: Vec<Link>,
    /// Links withdrawn at runtime.
    removed_links: BTreeSet<String>,
    /// Explicit socket state (connect/disconnect).
    connected: Option<bool>,
    /// Systems folder the pool pick switched to.
    systems_dir: Option<String>,
    /// Task id sequence for runtime-added tasks.
    next_task: u64,
}

fn overrides() -> &'static Mutex<Overrides> {
    static OVERRIDES: OnceLock<Mutex<Overrides>> = OnceLock::new();
    OVERRIDES.get_or_init(|| Mutex::new(Overrides::default()))
}

/// Applies the overlay on top of the base fixture.
fn compose(o: &Overrides) -> Snapshot {
    let now = now_ms();
    let mut snap = fixture();

    for system in &mut snap.systems {
        match o.running.get(&system.id) {
            Some(true) => {
                system.running = true;
                system.pid = Some(system.pid.unwrap_or(4242));
                system.started_at = Some(system.started_at.unwrap_or(now));
                system.last_activity_at = Some(now);
            }
            Some(false) => {
                system.running = false;
                system.pid = None;
                system.started_at = None;
            }
            None => {}
        }
    }

    if let Some(connected) = o.connected {
        snap.connected = connected;
        if !connected {
            snap.account = None;
            snap.positions.clear();
            snap.open_positions = Some(Vec::new());
            snap.closable_positions = Some(Vec::new());
        }
    }

    snap.positions.retain(|p| !o.closed.contains(&p.symbol));
    if let Some(open) = snap.open_positions.as_mut() {
        open.retain(|p| !o.closed.contains(&p.symbol));
    }
    if let Some(closable) = snap.closable_positions.as_mut() {
        closable.retain(|p| !o.closed.contains(&p.symbol));
    }

    for task in &mut snap.tasks {
        if let Some(status) = o.task_status.get(&task.id) {
            task.status = *status;
            task.resolved_at = Some(now);
            task.time_updated = now;
        }
    }
    for task in o.extra_tasks.iter().rev() {
        snap.tasks.insert(0, task.clone());
    }

    for row in o.extra_logs.iter().rev() {
        snap.logs.insert(0, row.clone());
    }

    if let Some(dir) = o.systems_dir.clone() {
        snap.systems_dir = Some(dir);
    }
    if let Some(links) = snap.links.as_mut() {
        links.retain(|l| !o.removed_links.contains(&l.id));
        links.extend(o.extra_links.iter().cloned());
    }

    snap
}

static TX: OnceLock<watch::Sender<Snapshot>> = OnceLock::new();

fn tx() -> &'static watch::Sender<Snapshot> {
    TX.get_or_init(|| {
        let (tx, _rx) = watch::channel(current());
        // The 2 s ticker only exists to move the equity history along; every
        // action already publishes through the sender. Callers without a
        // runtime (the action tests, a CLI probe) still get a working
        // snapshot instead of a panic from `tokio::spawn`.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let ticker = tx.clone();
            handle.spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(2));
                loop {
                    tick.tick().await;
                    let snapshot = {
                        let guard = match overrides().lock() {
                            Ok(g) => g,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        let mut snap = compose(&guard);
                        if let Some(eq) = snap.equity.as_mut() {
                            let equity = snap.account.as_ref().and_then(|a| a.equity);
                            let pnl_day = snap.account.as_ref().and_then(|a| a.pnl_day);
                            eq.push(EquityPoint {
                                ts: now_ms(),
                                equity,
                                pnl_day,
                            });
                            if eq.len() > 1000 {
                                eq.remove(0);
                            }
                        }
                        snap
                    };
                    if ticker.send(snapshot).is_err() {
                        break;
                    }
                }
            });
        }
        tx
    })
}

/// A fresh receiver over the fixture stream; the ticker refreshes it every 2 s.
pub fn watch() -> watch::Receiver<Snapshot> {
    tx().subscribe()
}

/// The snapshot as it stands right now (actions included), without subscribing.
pub fn current() -> Snapshot {
    let guard = match overrides().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    compose(&guard)
}

fn publish() {
    let snapshot = current();
    let _ = tx().send(snapshot);
}

fn push_log(level: LogLevel, system: &str, message: String, key: &str, event: Option<LogEvent>) {
    if let Ok(mut o) = overrides().lock() {
        o.extra_logs.insert(
            0,
            log(
                now_ms(),
                level,
                system,
                &message,
                Some(key),
                event,
            ),
        );
        if o.extra_logs.len() > 200 {
            o.extra_logs.truncate(200);
        }
    }
}

/// Starts a system's bot (simulated): flips its flags, marks a pid, logs it.
pub fn start_system(system_id: &str) -> Option<System> {
    if !fixture().systems.iter().any(|s| s.id == system_id) {
        return None;
    }
    if let Ok(mut o) = overrides().lock() {
        o.running.insert(system_id.to_string(), true);
    }
    push_log(
        LogLevel::Info,
        system_id,
        "Bot arrancado (simulado).".into(),
        "trading.log.system_started",
        Some(LogEvent::Open),
    );
    publish();
    current().systems.into_iter().find(|s| s.id == system_id)
}

/// Stops a system's bot (simulated): the pid-guarded kill path, faked.
pub fn stop_system(system_id: &str) -> Option<System> {
    if !fixture().systems.iter().any(|s| s.id == system_id) {
        return None;
    }
    if let Ok(mut o) = overrides().lock() {
        o.running.insert(system_id.to_string(), false);
    }
    push_log(
        LogLevel::Info,
        system_id,
        "Bot detenido (simulado).".into(),
        "trading.log.system_stopped",
        Some(LogEvent::Close),
    );
    publish();
    current().systems.into_iter().find(|s| s.id == system_id)
}

/// The full close cycle for one position, faked with the same verdict shape.
pub fn close_position(symbol: &str) -> PositionCloseResult {
    if !current().positions.iter().any(|p| p.symbol == symbol) {
        return PositionCloseResult {
            ok: false,
            message: format!("No hay posición abierta en {symbol}."),
            message_key: Some("trading.close.not_open".into()),
            message_params: None,
        };
    }
    if let Ok(mut o) = overrides().lock() {
        o.closed.insert(symbol.to_string());
    }
    push_log(
        LogLevel::Info,
        "motor",
        format!("Posición {symbol} cerrada (simulada)."),
        "trading.close.ok",
        Some(LogEvent::Close),
    );
    publish();
    PositionCloseResult {
        ok: true,
        message: format!("Posición {symbol} cerrada (simulada)."),
        message_key: Some("trading.close.ok".into()),
        message_params: None,
    }
}

/// Closes every position the dashboard shows (simulated close-all sweep).
pub fn close_all_positions() -> PositionCloseResult {
    let symbols: Vec<String> = current().positions.iter().map(|p| p.symbol.clone()).collect();
    if symbols.is_empty() {
        return PositionCloseResult {
            ok: true,
            message: "No hay posiciones abiertas.".into(),
            message_key: Some("trading.close.nothing".into()),
            message_params: None,
        };
    }
    if let Ok(mut o) = overrides().lock() {
        for symbol in &symbols {
            o.closed.insert(symbol.clone());
        }
    }
    push_log(
        LogLevel::Warning,
        "motor",
        format!("{} posiciones cerradas (simulado).", symbols.len()),
        "trading.close.all_ok",
        Some(LogEvent::Close),
    );
    publish();
    PositionCloseResult {
        ok: true,
        message: format!("{} posiciones cerradas (simulado).", symbols.len()),
        message_key: Some("trading.close.all_ok".into()),
        message_params: None,
    }
}

/// The panic button: stop every bot, then the emergency close-all, faked.
pub fn panic() -> PanicResult {
    let snap = current();
    let stopped: Vec<String> = snap
        .systems
        .iter()
        .filter(|s| s.running)
        .map(|s| s.id.clone())
        .collect();
    let symbols: Vec<String> = snap.positions.iter().map(|p| p.symbol.clone()).collect();
    if let Ok(mut o) = overrides().lock() {
        for id in &stopped {
            o.running.insert(id.clone(), false);
        }
        for symbol in &symbols {
            o.closed.insert(symbol.clone());
        }
    }
    push_log(
        LogLevel::Error,
        "motor",
        format!(
            "PANIC (simulado): {} sistemas parados, {} posiciones cerradas.",
            stopped.len(),
            symbols.len()
        ),
        "trading.panic.done",
        Some(LogEvent::Close),
    );
    publish();
    PanicResult {
        stopped,
        close: PositionCloseResult {
            ok: true,
            message: format!("{} posiciones cerradas (simulado).", symbols.len()),
            message_key: Some("trading.close.all_ok".into()),
            message_params: None,
        },
        message: "Pánico ejecutado (simulado).".into(),
        message_key: Some("trading.panic.done".into()),
        message_params: None,
    }
}

/// Validates a task input and computes its run time on the PC's local clock.
fn run_at_from(input: &TaskAddInput) -> Result<i64, TradingError> {
    if input.delay_minutes.is_some() && input.at_time.is_some() {
        return Err(TradingError::InvalidTask {
            message: "Indica minutos o una hora, no las dos cosas.".into(),
        });
    }
    if let Some(minutes) = input.delay_minutes {
        if !minutes.is_finite() || minutes <= 0.0 {
            return Err(TradingError::InvalidTask {
                message: "Los minutos de espera deben ser un número mayor que cero.".into(),
            });
        }
        return Ok(now_ms() + (minutes * 60_000.0) as i64);
    }
    let Some(at) = input.at_time.as_deref() else {
        return Err(TradingError::InvalidTask {
            message: "Indica en cuántos minutos o a qué hora.".into(),
        });
    };
    let invalid = || TradingError::InvalidTask {
        message: "La hora debe tener el formato HH:MM.".into(),
    };
    let (hours, minutes) = at.split_once(':').ok_or_else(invalid)?;
    let hour: u32 = hours.parse().map_err(|_| invalid())?;
    let minute: u32 = minutes.parse().map_err(|_| invalid())?;
    if hour > 23 || minute > 59 {
        return Err(invalid());
    }
    let today = Local::now().date_naive();
    let naive = today.and_hms_opt(hour, minute, 0).ok_or_else(invalid)?;
    let mut run_at = Local
        .from_local_datetime(&naive)
        .single()
        .ok_or_else(invalid)?
        .timestamp_millis();
    if run_at <= now_ms() {
        let tomorrow = (today + chrono::Days::new(1))
            .and_hms_opt(hour, minute, 0)
            .ok_or_else(invalid)?;
        run_at = Local
            .from_local_datetime(&tomorrow)
            .single()
            .ok_or_else(invalid)?
            .timestamp_millis();
    }
    Ok(run_at)
}

/// Schedules a systems task (simulated) and returns it.
pub fn task_add(input: &TaskAddInput) -> Result<Task, TradingError> {
    let run_at = run_at_from(input)?;
    let pool = fixture().systems;
    let ids: Vec<String> = input.system_ids.clone().unwrap_or_default();
    for id in &ids {
        if !pool.iter().any(|s| &s.id == id) {
            return Err(TradingError::SystemNotFound {
                system_id: id.clone(),
            });
        }
    }
    let now = now_ms();
    let task = {
        let mut o = match overrides().lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        o.next_task += 1;
        let task = Task {
            id: format!("trd_mock{:04}", o.next_task),
            kind: input.kind,
            system_ids: ids,
            run_at,
            status: TaskStatus::Pending,
            resolved_at: None,
            time_created: now,
            time_updated: now,
        };
        o.extra_tasks.insert(0, task.clone());
        task
    };
    push_log(
        LogLevel::Info,
        "motor",
        format!(
            "Tarea programada: {} a las {} (simulada).",
            match task.kind {
                TaskKind::StartSystems => "arrancar sistemas",
                TaskKind::StopSystems => "parar sistemas",
            },
            Local
                .timestamp_millis_opt(task.run_at)
                .single()
                .map(|dt| dt.format("%H:%M").to_string())
                .unwrap_or_else(|| "?".into())
        ),
        "trading.log.task_added",
        None,
    );
    publish();
    Ok(task)
}

/// Withdraws a pending task (simulated); returns it with its new status.
pub fn task_cancel(task_id: &str) -> Result<Task, TradingError> {
    let now = now_ms();
    let mut found = false;
    {
        let mut o = match overrides().lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(task) = o.extra_tasks.iter_mut().find(|t| t.id == task_id) {
            task.status = TaskStatus::Cancelled;
            task.resolved_at = Some(now);
            task.time_updated = now;
            found = true;
        } else if fixture().tasks.iter().any(|t| t.id == task_id) {
            o.task_status.insert(task_id.to_string(), TaskStatus::Cancelled);
            found = true;
        }
    }
    if !found {
        return Err(TradingError::TaskNotFound {
            task_id: task_id.to_string(),
        });
    }
    push_log(
        LogLevel::Info,
        "motor",
        format!("Tarea {task_id} cancelada (simulada)."),
        "trading.log.task_cancelled",
        None,
    );
    publish();
    current()
        .tasks
        .into_iter()
        .find(|t| t.id == task_id)
        .ok_or_else(|| TradingError::TaskNotFound {
            task_id: task_id.to_string(),
        })
}

/// The user's own "Conectar" click (simulated: the socket was already up).
pub fn connect() -> Option<Account> {
    if let Ok(mut o) = overrides().lock() {
        o.connected = Some(true);
    }
    publish();
    current().account
}

/// Drops the socket (simulated) until the next connect.
pub fn disconnect() -> Option<Account> {
    if let Ok(mut o) = overrides().lock() {
        o.connected = Some(false);
    }
    publish();
    None
}

/// Switches the dashboard's shown account (the mock knows its single demo one).
pub fn account_select(account_id: &str) -> Result<Option<Account>, TradingError> {
    let snap = current();
    let Some(account) = snap.account else {
        return Err(TradingError::InvalidRequest {
            message: "No account has been discovered yet.".into(),
        });
    };
    if account.id != account_id {
        return Err(TradingError::InvalidRequest {
            message: format!("Unknown account: {account_id}"),
        });
    }
    publish();
    Ok(current().account)
}

/// Registers a watch (simulated) and returns its row.
pub fn link_add(input: &LinkAddInput) -> Result<Link, TradingError> {
    let broker = input.broker.unwrap_or(Broker::Ibkr);
    let (id, host, port, mt5_login, mt5_server) = match broker {
        Broker::Mt5 => {
            let login = input.mt5_login.ok_or_else(|| TradingError::InvalidRequest {
                message: "An MT5 link needs its login.".into(),
            })?;
            let server = input
                .mt5_server
                .clone()
                .ok_or_else(|| TradingError::InvalidRequest {
                    message: "An MT5 link needs its server.".into(),
                })?;
            (
                format!("mt5-{login}@{server}"),
                None,
                None,
                Some(login),
                Some(server),
            )
        }
        Broker::Ibkr | Broker::Nt => {
            let host = input.host.clone().unwrap_or_else(|| "127.0.0.1".into());
            let port = input.port.unwrap_or(7497);
            (format!("{host}:{port}"), Some(host), Some(port), None, None)
        }
    };
    let link = Link {
        id,
        host,
        port,
        label: input.label.clone(),
        broker: Some(broker),
        mt5_login,
        mt5_server,
        connected: true,
    };
    if let Ok(mut o) = overrides().lock() {
        o.removed_links.remove(&link.id);
        if !fixture().links.iter().flatten().any(|l| l.id == link.id)
            && !o.extra_links.iter().any(|l| l.id == link.id)
        {
            o.extra_links.push(link.clone());
        }
    }
    push_log(
        LogLevel::Info,
        "motor",
        format!("Enlace registrado: {} (simulado).", link.id),
        "trading.log.link_added",
        None,
    );
    publish();
    Ok(link)
}

/// Withdraws a link (simulated) and returns the remaining rows.
pub fn link_remove(link_id: &str) -> Result<Vec<Link>, TradingError> {
    if !current().links.iter().flatten().any(|l| l.id == link_id) {
        return Err(TradingError::InvalidRequest {
            message: format!("Unknown link: {link_id}"),
        });
    }
    if let Ok(mut o) = overrides().lock() {
        o.extra_links.retain(|l| l.id != link_id);
        o.removed_links.insert(link_id.to_string());
    }
    push_log(
        LogLevel::Info,
        "motor",
        format!("Enlace retirado: {link_id} (simulado)."),
        "trading.log.link_removed",
        None,
    );
    publish();
    Ok(current().links.unwrap_or_default())
}

/// Searches this computer's MetaTrader 5 terminals (one demo fixture here).
pub fn mt5_detect(probe: bool) -> Vec<Mt5Detected> {
    vec![Mt5Detected {
        login: 51_234_567,
        server: "AureaDemo".into(),
        name: Some("Cuenta Demo".into()),
        active: true,
        live: probe,
    }]
}

/// Knocks on the IBKR API ports (loopback defaults; 7497 answers on this VM).
pub fn ibkr_detect(host: Option<&str>, port: Option<u16>) -> Vec<IbkrPort> {
    let host = host.unwrap_or("127.0.0.1").to_string();
    match port {
        Some(port) => vec![IbkrPort {
            live: port == 7496,
            open: port == 7497,
            host,
            port,
        }],
        None => vec![
            IbkrPort {
                host: host.clone(),
                port: 7496,
                live: true,
                open: false,
            },
            IbkrPort {
                host,
                port: 7497,
                live: false,
                open: true,
            },
        ],
    }
}

/// Points the engine at another systems folder (simulated; refuses empties).
pub fn pool_select(dir: &str) -> Result<PoolSelectResult, TradingError> {
    let dir = dir.trim();
    if dir.is_empty() {
        return Err(TradingError::InvalidRequest {
            message: "Pick a systems folder.".into(),
        });
    }
    if let Ok(mut o) = overrides().lock() {
        o.systems_dir = Some(dir.to_string());
    }
    push_log(
        LogLevel::Info,
        "motor",
        format!("Pool cambiado a {dir} (simulado)."),
        "trading.log.pool_selected",
        None,
    );
    publish();
    Ok(PoolSelectResult {
        dir: dir.to_string(),
        systems: fixture().systems.len() as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// The overlay is process-global, so the action tests take turns.
    fn serial() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock = LOCK.get_or_init(|| Mutex::new(()));
        match lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn reset() {
        let mut overrides = match overrides().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *overrides = Overrides::default();
    }

    #[test]
    fn fixture_is_complete_and_demo() {
        let snapshot = fixture();
        assert_eq!(snapshot.systems.len(), 2);
        assert_eq!(snapshot.positions.len(), 3);
        assert!(snapshot.connected);
        let account = snapshot.account.expect("fixture ships an account");
        assert_eq!(account.demo, Some(true));
        assert_eq!(snapshot.tasks.len(), 2);
        assert_eq!(snapshot.equity.as_ref().map(Vec::len), Some(24));
        assert_eq!(snapshot.links.as_ref().map(Vec::len), Some(2));
        assert_eq!(snapshot.programs.map(|programs| programs.tws), Some(ProgramState::Running));
    }

    #[test]
    fn start_and_stop_flip_the_system_flags() {
        let _guard = serial();
        reset();
        let started = start_system("02_nq_tws").expect("known system");
        assert!(started.running);
        assert!(started.pid.is_some());
        let stopped = stop_system("02_nq_tws").expect("known system");
        assert!(!stopped.running);
        assert!(stopped.pid.is_none());
        assert!(start_system("nope").is_none());
    }

    #[test]
    fn close_retires_one_position_and_refuses_unknowns() {
        let _guard = serial();
        reset();
        assert!(!close_position("UNKNOWN").ok);
        let result = close_position("EURUSD");
        assert!(result.ok, "{}", result.message);
        assert!(!current().positions.iter().any(|p| p.symbol == "EURUSD"));
        assert!(!close_position("EURUSD").ok, "second close finds nothing");
    }

    #[test]
    fn close_all_and_panic_empty_the_book() {
        let _guard = serial();
        reset();
        let all = close_all_positions();
        assert!(all.ok, "{}", all.message);
        assert!(current().positions.is_empty());
        let empty = close_all_positions();
        assert!(empty.ok);
        reset();
        let panic = panic();
        assert!(panic.stopped.contains(&"01_eurusd_mt5".to_string()));
        let snapshot = current();
        assert!(snapshot.positions.is_empty());
        assert!(snapshot.systems.iter().all(|system| !system.running));
    }

    #[test]
    fn task_validation_matches_the_typescript_engine() {
        let _guard = serial();
        reset();
        let both = TaskAddInput {
            kind: TaskKind::StopSystems,
            system_ids: None,
            delay_minutes: Some(5.0),
            at_time: Some("10:00".into()),
        };
        assert!(matches!(
            task_add(&both),
            Err(TradingError::InvalidTask { .. })
        ));
        let neither = TaskAddInput {
            kind: TaskKind::StopSystems,
            system_ids: None,
            delay_minutes: None,
            at_time: None,
        };
        assert!(matches!(
            task_add(&neither),
            Err(TradingError::InvalidTask { .. })
        ));
        let negative = TaskAddInput {
            kind: TaskKind::StopSystems,
            system_ids: None,
            delay_minutes: Some(-1.0),
            at_time: None,
        };
        assert!(matches!(
            task_add(&negative),
            Err(TradingError::InvalidTask { .. })
        ));
        let bad_clock = TaskAddInput {
            kind: TaskKind::StopSystems,
            system_ids: None,
            delay_minutes: None,
            at_time: Some("25:00".into()),
        };
        assert!(matches!(
            task_add(&bad_clock),
            Err(TradingError::InvalidTask { .. })
        ));
        let unknown = TaskAddInput {
            kind: TaskKind::StartSystems,
            system_ids: Some(vec!["nope".into()]),
            delay_minutes: Some(1.0),
            at_time: None,
        };
        assert!(matches!(
            task_add(&unknown),
            Err(TradingError::SystemNotFound { .. })
        ));
    }

    #[test]
    fn task_add_schedules_and_cancel_resolves() {
        let _guard = serial();
        reset();
        let now = now_ms();
        let input = TaskAddInput {
            kind: TaskKind::StartSystems,
            system_ids: Some(vec!["02_nq_tws".into()]),
            delay_minutes: Some(30.0),
            at_time: None,
        };
        let task = task_add(&input).expect("valid task");
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.run_at > now + 29 * 60_000);
        assert_eq!(current().tasks.first().map(|t| t.id.as_str()), Some(task.id.as_str()));
        let cancelled = task_cancel(&task.id).expect("own task");
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
        assert!(cancelled.resolved_at.is_some());
        assert!(matches!(
            task_cancel("trd_missing"),
            Err(TradingError::TaskNotFound { .. })
        ));
    }

    #[test]
    fn links_build_ids_and_mt5_needs_its_identity() {
        let _guard = serial();
        reset();
        let missing = LinkAddInput {
            host: None,
            port: None,
            live: None,
            label: None,
            broker: Some(Broker::Mt5),
            mt5_login: None,
            mt5_server: None,
        };
        assert!(matches!(
            link_add(&missing),
            Err(TradingError::InvalidRequest { .. })
        ));
        let mt5 = LinkAddInput {
            host: None,
            port: None,
            live: None,
            label: Some("Cuenta demo".into()),
            broker: Some(Broker::Mt5),
            mt5_login: Some(51_234_567),
            mt5_server: Some("AureaDemo".into()),
        };
        let link = link_add(&mt5).expect("valid mt5 link");
        assert_eq!(link.id, "mt5-51234567@AureaDemo");
        assert_eq!(link.mt5_login, Some(51_234_567));
        let removed = link_remove(&link.id).expect("known link");
        assert!(!removed.iter().any(|l| l.id == link.id));
        assert!(matches!(
            link_remove("nope"),
            Err(TradingError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn connect_disconnect_and_pool_selection_are_honest() {
        let _guard = serial();
        reset();
        assert!(disconnect().is_none());
        let snapshot = current();
        assert!(!snapshot.connected);
        assert!(snapshot.account.is_none());
        assert!(snapshot.positions.is_empty());
        assert!(connect().is_some());
        assert!(current().connected);
        assert!(matches!(
            pool_select("  "),
            Err(TradingError::InvalidRequest { .. })
        ));
        let picked = pool_select("C:\\Aurea\\otro").expect("non-empty dir");
        assert_eq!(picked.systems, 2);
        assert_eq!(current().systems_dir.as_deref(), Some("C:\\Aurea\\otro"));
        assert!(matches!(
            account_select("DU-other"),
            Err(TradingError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn detectors_report_paper_not_live() {
        let ports = ibkr_detect(None, None);
        let paper = ports.iter().find(|p| p.port == 7497).expect("paper port");
        assert!(!paper.live);
        assert!(paper.open);
        let live = ports.iter().find(|p| p.port == 7496).expect("live port");
        assert!(live.live);
        assert!(!live.open);
        let detected = mt5_detect(true);
        assert_eq!(detected.len(), 1);
        assert!(detected[0].active);
    }
}
