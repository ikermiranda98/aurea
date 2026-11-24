//! Scheduled tasks (agenda) ported from Openbot `packages/core/src/trading.ts`
//! (spec `SPECS/A5.md` §2.1). Self-contained: operates on a
//! `&rusqlite::Connection` (the `trading_task` table) plus an explicit clock
//! (`now_ms`), so it composes with the store layer without coupling to it.

use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{Local, TimeZone};
use rusqlite::{params, Connection};
// Re-export the domain types so callers can use `crate::tasks::*`.
pub use zeron_proto::trading::{Task, TaskAddInput, TaskKind, TaskStatus};

/// Monotonic counter used to mint `trd_<n>` ids. It restarts with the process
/// while the store outlives the engine, so [`next_task_id`] skips numbers the
/// database already holds — otherwise a fresh run would collide on `trd_1`.
static TASK_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_task_id(conn: &Connection) -> String {
    loop {
        let id = format!("trd_{}", TASK_COUNTER.fetch_add(1, Ordering::SeqCst));
        let taken = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM trading_task WHERE id = ?1)",
                params![id],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if !taken {
            return id;
        }
    }
}

fn kind_to_str(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::StartSystems => "start-systems",
        TaskKind::StopSystems => "stop-systems",
    }
}

fn kind_from_str(s: &str) -> TaskKind {
    match s {
        "start-systems" | "StartSystems" | "start_systems" => TaskKind::StartSystems,
        "stop-systems" | "StopSystems" | "stop_systems" => TaskKind::StopSystems,
        _ => {
            if s.to_lowercase().contains("stop") {
                TaskKind::StopSystems
            } else {
                TaskKind::StartSystems
            }
        }
    }
}

pub(crate) fn status_to_str(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::Ran => "ran",
        TaskStatus::Missed => "missed",
        TaskStatus::Cancelled => "cancelled",
        TaskStatus::Failed => "failed",
    }
}


fn status_from_str(s: &str) -> TaskStatus {
    match s.to_lowercase().as_str() {
        "pending" => TaskStatus::Pending,
        "ran" => TaskStatus::Ran,
        "missed" => TaskStatus::Missed,
        "cancelled" => TaskStatus::Cancelled,
        "failed" => TaskStatus::Failed,
        _ => TaskStatus::Pending,
    }
}

/// A small helper trait so `compute_run_at_delay` accepts both integer and
/// floating-point minute counts.
pub trait IntoMinutes {
    fn into_minutes(self) -> f64;
}

macro_rules! impl_into_minutes {
    ($($t:ty),*) => {
        $(impl IntoMinutes for $t {
            fn into_minutes(self) -> f64 { self as f64 }
        })*
    };
}

impl_into_minutes!(f32, f64, i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

/// `now + delay_minutes * 60_000`.
pub fn compute_run_at_delay<M>(now_ms: i64, delay_minutes: M) -> i64
where
    M: IntoMinutes,
{
    now_ms + (delay_minutes.into_minutes() * 60_000.0) as i64
}

/// Next local wall-clock instant for `"HH:MM"` (today if still in the future,
/// tomorrow otherwise), anchored on `now_ms`'s local date.
pub fn compute_run_at_time(now_ms: i64, at_time: &str) -> Result<i64, String> {
    let invalid = || "atTime must be a local \"HH:MM\" time.".to_string();

    let (h_str, m_str) = at_time.split_once(':').ok_or_else(invalid)?;
    if h_str.is_empty() || h_str.len() > 2 || m_str.len() != 2 {
        return Err(invalid());
    }
    if !h_str.chars().all(|c| c.is_ascii_digit())
        || !m_str.chars().all(|c| c.is_ascii_digit())
    {
        return Err(invalid());
    }
    let hour: u32 = h_str.parse().map_err(|_| invalid())?;
    let minute: u32 = m_str.parse().map_err(|_| invalid())?;
    if hour > 23 || minute > 59 {
        return Err(invalid());
    }

    let now_local = Local
        .timestamp_millis_opt(now_ms)
        .single()
        .or_else(|| Local.timestamp_millis_opt(now_ms).earliest())
        .ok_or_else(invalid)?;
    let today = now_local.date_naive();

    let naive = today.and_hms_opt(hour, minute, 0).ok_or_else(invalid)?;
    let mut run_at = Local
        .from_local_datetime(&naive)
        .single()
        .or_else(|| Local.from_local_datetime(&naive).earliest())
        .ok_or_else(invalid)?
        .timestamp_millis();

    if run_at <= now_ms {
        let tomorrow = today
            .checked_add_days(chrono::Days::new(1))
            .ok_or_else(invalid)?;
        let naive2 = tomorrow.and_hms_opt(hour, minute, 0).ok_or_else(invalid)?;
        run_at = Local
            .from_local_datetime(&naive2)
            .single()
            .or_else(|| Local.from_local_datetime(&naive2).earliest())
            .ok_or_else(invalid)?
            .timestamp_millis();
    }
    Ok(run_at)
}

/// Validate a task input and compute its run time.
pub fn validate_task_input(
    input: &TaskAddInput,
    now_ms: i64,
) -> Result<(TaskKind, Vec<String>, i64), String> {
    if input.delay_minutes.is_some() && input.at_time.is_some() {
        return Err("Give either delayMinutes or atTime, not both.".into());
    }
    let system_ids = input.system_ids.clone().unwrap_or_default();

    if let Some(minutes) = input.delay_minutes {
        if !minutes.is_finite() || minutes <= 0.0 {
            return Err("delayMinutes must be a positive number of minutes.".into());
        }
        let run_at = compute_run_at_delay(now_ms, minutes);
        return Ok((input.kind, system_ids, run_at));
    }

    let Some(at) = input.at_time.as_deref() else {
        return Err("Give delayMinutes or atTime.".into());
    };
    let run_at = compute_run_at_time(now_ms, at)?;
    Ok((input.kind, system_ids, run_at))
}

/// Add a task to the store. Validates input and system ids.
pub fn add_task(
    conn: &Connection,
    input: &TaskAddInput,
    now_ms: i64,
    valid_system_ids: &[String],
) -> Result<Task, String> {
    let (kind, system_ids, run_at) = validate_task_input(input, now_ms)?;

    for id in &system_ids {
        if !valid_system_ids.iter().any(|v| v == id) {
            return Err(format!("system not found: {}", id));
        }
    }

    let id = next_task_id(conn);
    let kind_str = kind_to_str(kind);
    let system_ids_json = serde_json::to_string(&system_ids).unwrap_or_else(|_| "[]".to_string());

    conn.execute(
        "INSERT INTO trading_task \
         (id, kind, system_ids, run_at, status, resolved_at, time_created, time_updated) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            kind_str,
            system_ids_json,
            run_at,
            status_to_str(TaskStatus::Pending),
            Option::<i64>::None,
            now_ms,
            now_ms
        ],
    )
    .map_err(|e| e.to_string())?;

    Ok(Task {
        id,
        kind,
        system_ids,
        run_at,
        status: TaskStatus::Pending,
        resolved_at: None,
        time_created: now_ms,
        time_updated: now_ms,
    })
}

/// Cancel a pending task.
pub fn cancel_task(conn: &Connection, task_id: &str, now_ms: i64) -> Result<Task, String> {
    let row: Result<
        (String, String, i64, String, Option<i64>, Option<i64>, Option<i64>),
        _,
    > = conn.query_row(
        "SELECT kind, system_ids, run_at, status, resolved_at, time_created, time_updated \
         FROM trading_task WHERE id = ?1",
        params![task_id],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        },
    );

    let (kind_str, system_ids_raw, run_at, status_str, _resolved, time_created, _time_updated) =
        row.map_err(|_| format!("task not found: {}", task_id))?;

    let status = status_from_str(&status_str);
    if status != TaskStatus::Pending {
        return Err("Only pending tasks can be cancelled.".into());
    }

    conn.execute(
        "UPDATE trading_task SET status = ?1, resolved_at = ?2, time_updated = ?3 WHERE id = ?4",
        params![status_to_str(TaskStatus::Cancelled), now_ms, now_ms, task_id],
    )
    .map_err(|e| e.to_string())?;

    let system_ids: Vec<String> = serde_json::from_str(&system_ids_raw).unwrap_or_default();
    Ok(Task {
        id: task_id.to_string(),
        kind: kind_from_str(&kind_str),
        system_ids,
        run_at,
        status: TaskStatus::Cancelled,
        resolved_at: Some(now_ms),
        time_created: time_created.unwrap_or(now_ms),
        time_updated: now_ms,
    })
}

/// List tasks, newest first.
pub fn list_tasks(conn: &Connection, limit: usize) -> Result<Vec<Task>, String> {
    query_tasks(
        conn,
        "SELECT id, kind, system_ids, run_at, status, resolved_at, time_created, time_updated          FROM trading_task          ORDER BY time_created DESC, run_at DESC          LIMIT ?1",
        params![limit as i64],
    )
}

/// Every task still waiting for its time, soonest first — what the scheduler
/// runs. Unlike [`list_tasks`] it has no cap: a pending task must never fall
/// out of sight behind newer ones (the store keeps every task it ever held).
pub fn list_pending_tasks(conn: &Connection) -> Result<Vec<Task>, String> {
    query_tasks(
        conn,
        "SELECT id, kind, system_ids, run_at, status, resolved_at, time_created, time_updated          FROM trading_task          WHERE status = 'pending'          ORDER BY run_at ASC",
        params![],
    )
}

fn query_tasks(
    conn: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<Task>, String> {
    let mut stmt = conn.prepare(sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(args, |row| {
            let id: String = row.get(0)?;
            let kind_str: String = row.get(1)?;
            let system_ids_raw: String = row.get(2)?;
            let run_at: i64 = row.get(3)?;
            let status_str: String = row.get(4)?;
            let resolved_at: Option<i64> = row.get(5)?;
            let time_created: Option<i64> = row.get(6)?;
            let time_updated: Option<i64> = row.get(7)?;
            let system_ids: Vec<String> =
                serde_json::from_str(&system_ids_raw).unwrap_or_default();
            Ok(Task {
                id,
                kind: kind_from_str(&kind_str),
                system_ids,
                run_at,
                status: status_from_str(&status_str),
                resolved_at,
                time_created: time_created.unwrap_or(0),
                time_updated: time_updated.unwrap_or(0),
            })
        })
        .map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;
    use rusqlite::Connection;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE trading_task (\
                id TEXT PRIMARY KEY, kind TEXT NOT NULL, system_ids TEXT NOT NULL, \
                run_at INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'pending', \
                resolved_at INTEGER, time_created INTEGER, time_updated INTEGER);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn validation_errors_exact() {
        let now = 1_000_000i64;
        let both = TaskAddInput {
            kind: TaskKind::StopSystems,
            system_ids: None,
            delay_minutes: Some(5.0),
            at_time: Some("10:00".into()),
        };
        assert_eq!(
            validate_task_input(&both, now).unwrap_err(),
            "Give either delayMinutes or atTime, not both."
        );
        let neither = TaskAddInput {
            kind: TaskKind::StopSystems,
            system_ids: None,
            delay_minutes: None,
            at_time: None,
        };
        assert_eq!(
            validate_task_input(&neither, now).unwrap_err(),
            "Give delayMinutes or atTime."
        );
        let negative = TaskAddInput {
            kind: TaskKind::StopSystems,
            system_ids: None,
            delay_minutes: Some(-1.0),
            at_time: None,
        };
        assert_eq!(
            validate_task_input(&negative, now).unwrap_err(),
            "delayMinutes must be a positive number of minutes."
        );
        let bad_clock = TaskAddInput {
            kind: TaskKind::StopSystems,
            system_ids: None,
            delay_minutes: None,
            at_time: Some("25:00".into()),
        };
        assert_eq!(
            validate_task_input(&bad_clock, now).unwrap_err(),
            "atTime must be a local \"HH:MM\" time."
        );
        let bad_minute = TaskAddInput {
            kind: TaskKind::StopSystems,
            system_ids: None,
            delay_minutes: None,
            at_time: Some("10:5".into()),
        };
        assert_eq!(
            validate_task_input(&bad_minute, now).unwrap_err(),
            "atTime must be a local \"HH:MM\" time."
        );
    }

    #[test]
    fn compute_run_at_delay_works() {
        assert_eq!(compute_run_at_delay(1000i64, 2i64), 121000i64);
        assert_eq!(compute_run_at_delay(1000i64, 2.5f64), 151000i64);
    }

    #[test]
    fn compute_run_at_time_today_or_tomorrow() {
        // Build a now_ms anchored at 12:00 local today by using the current
        // wall clock's date; just exercise that a far-future time today stays
        // today and a past time moves to tomorrow.
        let now = Local::now();
        let now_ms = now.timestamp_millis();
        let future_hour = (now.hour() + 2) % 24;
        let future = format!("{:02}:{:02}", future_hour, now.minute());
        let run_future = compute_run_at_time(now_ms, &future).unwrap();
        // If we didn't wrap past midnight, run_future >= now_ms.
        if future_hour > now.hour() {
            assert!(run_future >= now_ms);
        }
        let past_hour = if now.hour() == 0 { 23 } else { now.hour() - 1 };
        let past = format!("{:02}:{:02}", past_hour, now.minute());
        let run_past = compute_run_at_time(now_ms, &past).unwrap();
        assert!(run_past > now_ms);
    }

    #[test]
    fn add_cancel_list_roundtrip() {
        let conn = setup();
        let now = 1_700_000_000_000i64;
        let input = TaskAddInput {
            kind: TaskKind::StartSystems,
            system_ids: Some(vec!["02".into()]),
            delay_minutes: Some(30.0),
            at_time: None,
        };
        let task = add_task(&conn, &input, now, &["02".to_string()]).unwrap();
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.system_ids, vec!["02".to_string()]);
        assert!(task.run_at > now);
        assert!(task.id.starts_with("trd_"));

        let tasks = list_tasks(&conn, 50).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, task.id);

        let cancelled = cancel_task(&conn, &task.id, now + 1).unwrap();
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
        assert_eq!(cancelled.resolved_at, Some(now + 1));

        // cancelling again fails
        assert!(cancel_task(&conn, &task.id, now + 2).is_err());
        // missing task fails
        let err = cancel_task(&conn, "trd_nope", now).unwrap_err();
        assert!(err.contains("task not found"));
    }

    #[test]
    fn a_pending_task_stays_in_sight_behind_newer_ones() {
        let conn = setup();
        let insert = |id: &str, status: &str, run_at: i64, created: i64| {
            conn.execute(
                "INSERT INTO trading_task                  (id, kind, system_ids, run_at, status, resolved_at, time_created, time_updated)                  VALUES (?1, 'stop-systems', '[]', ?2, ?3, NULL, ?4, ?4)",
                params![id, run_at, status, created],
            )
            .unwrap();
        };
        // An old pending task, then sixty newer ones already done.
        insert("old", "pending", 9_000, 1);
        for n in 0..60 {
            insert(&format!("done_{n}"), "ran", 100 + n, 10 + n);
        }
        assert!(list_tasks(&conn, 50).unwrap().iter().all(|t| t.id != "old"));
        let pending = list_pending_tasks(&conn).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "old");
    }

    #[test]
    fn task_ids_skip_rows_the_store_already_holds() {
        let conn = setup();
        // The engine restarts with a fresh counter while the DB keeps its
        // rows: plant the id the counter would hand out and make sure the
        // next mint skips it.
        let taken = next_task_id(&conn);
        conn.execute(
            "INSERT INTO trading_task \
             (id, kind, system_ids, run_at, status, resolved_at, time_created, time_updated) \
             VALUES (?1, 'start-systems', '[]', 1, 'cancelled', NULL, 1, 1)",
            params![taken],
        )
        .unwrap();
        let fresh = next_task_id(&conn);
        assert_ne!(fresh, taken);

        let input = TaskAddInput {
            kind: TaskKind::StartSystems,
            system_ids: Some(vec!["02".into()]),
            delay_minutes: Some(1.0),
            at_time: None,
        };
        let task = add_task(&conn, &input, 1_000, &["02".to_string()]).unwrap();
        assert_ne!(task.id, taken);
    }

    #[test]
    fn empty_pool_validation() {
        let conn = setup();
        let now = 1_000_000i64;
        let input = TaskAddInput {
            kind: TaskKind::StartSystems,
            system_ids: Some(vec!["nope".into()]),
            delay_minutes: Some(1.0),
            at_time: None,
        };
        let err = add_task(&conn, &input, now, &[]).unwrap_err();
        assert!(err.contains("system not found"));
        assert!(err.contains("nope"));
    }
}
