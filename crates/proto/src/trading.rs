//! Trading protocol types shared by the engine, the UI and the desktop tools.
//!
//! Ported from Openbot's `packages/schema/src/trading.ts`: field names and
//! nullability follow the TS schema exactly (`camelCase` on the wire, epoch-ms
//! numbers for timestamps). Optional-vs-null distinctions are preserved where
//! they matter to the ported oracle tests.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One broker platform a link (or its accounts) belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Broker {
    Ibkr,
    Nt,
    Mt5,
}

/// One registered watch: an IBKR TWS `host:port` whose socket the engine keeps
/// open, or an MT5 `mt5-<login>@<server>` the terminal-side bridge dials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    /// `host:port` for IBKR, `mt5-<login>@<server>` for MT5. Deduplicated per key.
    pub id: String,
    /// The TWS address. Optional: an MT5 watch dials no host of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The TWS API port. Optional: an MT5 watch dials no port of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The user's own name for the link; null when none was given (the primary's case).
    #[serde(default)]
    pub label: Option<String>,
    /// The broker platform the link speaks. Optional: older engines omit it (all IBKR).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broker: Option<Broker>,
    /// MT5 login for an MT5 link; absent on IBKR links.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mt5_login: Option<i64>,
    /// MT5 server name for an MT5 link; absent on IBKR links.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mt5_server: Option<String>,
    /// true while the link is feeding balances (its socket up, or its bridge's account arriving).
    pub connected: bool,
}

/// One broker account, as the shown account or inside `Snapshot.accounts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    /// Raw broker label from the engine ("IBKR · DU123…" style); a plain string in the TS schema.
    #[serde(rename = "type")]
    pub kind: String,
    pub cash: Option<f64>,
    pub equity: Option<f64>,
    pub pnl_day: Option<f64>,
    pub pnl_realized: Option<f64>,
    /// AvailableFunds account-summary tag (USD); null until IBKR sends the tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_funds: Option<f64>,
    /// Whether the account itself carries demo money (MT5 trade mode; IBKR omits it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demo: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buying_power: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin_used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excess_liquidity: Option<f64>,
    /// Epoch ms of the newest account fact; null until seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    /// The engine link the account lives on (its "host:port"); null when the engine's
    /// own state carries no link id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// One system: a `NN_name` folder in the pool the engine reads bots from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct System {
    pub id: String,
    pub name: String,
    /// Live bot process in the pool folder; the engine's own "active" is kept in sync.
    pub running: bool,
    pub active: bool,
    /// The bot's process id while running (engine-launched or detected at boot); null otherwise.
    pub pid: Option<u32>,
    /// Epoch ms when the engine (re)attached to the running bot; null while it is not running.
    pub started_at: Option<i64>,
    pub symbol: String,
    pub timeframe: String,
    /// The bot's unrealized PnL from its own registro.log portfolio updates; null until seen today.
    pub pnl_day: Option<f64>,
    /// Capital the person assigned to this system in its `system.json`; null when undeclared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capital: Option<f64>,
    /// `pnl_day` over `capital`, in percent; null without both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pnl_pct: Option<f64>,
    /// The bot's configured TWS CLIENT_ID from its config.py; null when the line is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<i64>,
    /// The config's session-close hour ("HH:MM:SS", New York's close); null when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_hour: Option<String>,
    /// Epoch ms of the bot's newest own record row today; null when it has not logged today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<i64>,
    /// The system's one-paragraph idea from its own reglas.json; null when the file is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The system's rules as key→text pairs; null when reglas.json is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<BTreeMap<String, String>>,
}

/// One open position row of the shown account (or of the aggregated exposure).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub symbol: String,
    pub side: PositionSide,
    pub size: f64,
    pub entry: Option<f64>,
    pub current: Option<f64>,
    pub pnl: Option<f64>,
    pub pnl_pct: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PositionSide {
    Long,
    Short,
}

/// The full close cycle's outcome; `message` is the engine's own plain-language
/// verdict (English fallback; `message_key`/`message_params` carry i18n).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PositionCloseResult {
    pub ok: bool,
    pub message: String,
    /// i18n key for the verdict, e.g. `trading.close.*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_params: Option<serde_json::Map<String, serde_json::Value>>,
}

/// What a panic run did: which bots it stopped plus the close verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PanicResult {
    pub stopped: Vec<String>,
    pub close: PositionCloseResult,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_params: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Warning,
    Error,
}

/// The few entries that mark a position lifecycle beat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogEvent {
    Open,
    Close,
    Signal,
}

/// One engine log row. `system` is the owning system id, or "motor" for engine-wide events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Log {
    pub ts: i64,
    pub level: LogLevel,
    pub system: String,
    pub message: String,
    /// i18n key for the entry, e.g. `trading.log.*`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_params: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<LogEvent>,
}

/// `trd_`-prefixed task id; validity is checked by the engine on the way in.
pub type TaskId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskKind {
    StartSystems,
    StopSystems,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Pending,
    Ran,
    Missed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: TaskId,
    pub kind: TaskKind,
    /// System IDs the task acts on, all validated to exist in the pool; empty means every system.
    pub system_ids: Vec<String>,
    /// The run time as epoch ms on the server machine's local (PC) clock.
    pub run_at: i64,
    pub status: TaskStatus,
    /// Epoch ms when the task fired, was marked missed or was cancelled; null while pending.
    pub resolved_at: Option<i64>,
    pub time_created: i64,
    pub time_updated: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskAddInput {
    pub kind: TaskKind,
    /// System IDs to act on; empty or omitted means every system in the pool.
    #[serde(
        rename = "systemIDs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub system_ids: Option<Vec<String>>,
    /// Minutes from now on the PC clock; mutually exclusive with `at_time`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_minutes: Option<f64>,
    /// The next local wall-clock time "HH:MM" (PC clock; tomorrow when already past);
    /// mutually exclusive with `delay_minutes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_time: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkAddInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broker: Option<Broker>,
    /// MT5 registrations only: the MetaTrader 5 login the terminal account carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mt5_login: Option<i64>,
    /// MT5 registrations only: the server the terminal logs into; given with `mt5_login`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mt5_server: Option<String>,
}

/// One MT5 account a running terminal reports (or a config names).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mt5Detected {
    pub login: i64,
    pub server: String,
    /// The terminal's own account name; null outside a live answer.
    pub name: Option<String>,
    /// true when the config's own last login names this account, the natural pick.
    pub active: bool,
    /// true when the live terminal answered (it may have been opened by the search).
    pub live: bool,
}

/// One IBKR API address on this computer and whether something answers there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IbkrPort {
    pub host: String,
    pub port: u16,
    /// true on the live (real-money) default, 7496; false on its paper sibling 7497
    /// and on any other port.
    pub live: bool,
    /// true when something accepted the TCP connection within the knock's short deadline.
    pub open: bool,
}

/// The systems folder a pick switched the engine to, and how many systems it found there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolSelectResult {
    pub dir: String,
    pub systems: u32,
}

/// One closed trade, harvested from the bot's own registro.log rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Trade {
    pub system: String,
    pub ts: i64,
    pub pnl: Option<f64>,
    pub message: String,
}

/// One sampled point of the account's equity curve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EquityPoint {
    pub ts: i64,
    pub equity: Option<f64>,
    pub pnl_day: Option<f64>,
}

/// A desktop program the engine needs: running, opened by the engine and still loading,
/// installed but closed, or not installed on this computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgramState {
    Running,
    Opening,
    Closed,
    Missing,
}

/// The programs the engine opens on its own: TWS always, MetaTrader 5 while an MT5
/// account is linked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Programs {
    pub tws: ProgramState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mt5: Option<ProgramState>,
}

/// The full engine state the trading surface renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub version: String,
    /// true while the engine's primary TWS socket is up; account and positions are empty otherwise.
    pub connected: bool,
    /// The engine's own TWS client id while TWS refuses it as already taken; absent while free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_in_use: Option<i64>,
    pub account: Option<Account>,
    pub systems: Vec<System>,
    pub positions: Vec<Position>,
    /// Every open position the engine can see (all links) — what the quit gate watches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_positions: Option<Vec<Position>>,
    /// The subset of `open_positions` the panic button would actually close right now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closable_positions: Option<Vec<Position>>,
    pub logs: Vec<Log>,
    /// Recent scheduled tasks, newest first.
    pub tasks: Vec<Task>,
    /// Closed trades harvested from the bots' logs, newest first, capped to 200.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trades: Option<Vec<Trade>>,
    /// The account's equity curve, oldest first, sampled every 15 s, capped to 1000 points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equity: Option<Vec<EquityPoint>>,
    /// Every account the engine's links have discovered, linked through `link`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounts: Option<Vec<Account>>,
    /// The engine's links, the panel's primary one first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<Link>>,
    /// The folder the engine reads its systems from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub systems_dir: Option<String>,
    /// TWS's and MetaTrader 5's state on this computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub programs: Option<Programs>,
    /// Money actions an agent asked for, newest first: they run only when
    /// the person approves them on the desk (`TradingProposalResolve`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposals: Option<Vec<TradingProposal>>,
}

/// What an agent may ask the desk to do. Every variant moves money or a
/// bot, so none runs without the person's approval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProposalAction {
    ClosePosition { symbol: String },
    CloseAll,
    Panic,
    StartSystem { system_id: String },
    StopSystem { system_id: String },
}

/// Where a proposal stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalStatus {
    /// Waiting for the person.
    Pending,
    /// The person said no; nothing ran.
    Rejected,
    /// Approved and run; `result` holds the verdict.
    Done,
    /// Approved but the action failed; `result` says why.
    Failed,
}

/// One agent request on the desk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradingProposal {
    pub id: String,
    pub action: ProposalAction,
    /// The agent's one-line reason, shown beside the buttons.
    #[serde(default)]
    pub reason: String,
    /// The chat that asked, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    pub status: ProposalStatus,
    /// Epoch ms.
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<i64>,
    /// The action's verdict once it ran (or the refusal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

/// `TradingPropose` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposeParams {
    pub action: ProposalAction,
    #[serde(default)]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
}

/// `TradingProposalResolve` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveProposalParams {
    pub id: String,
    pub approve: bool,
}

/// Errors the engine's action surface returns; carried as the RPC error payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum TradingError {
    #[error("system not found: {system_id}")]
    SystemNotFound { system_id: String },
    #[error("{message}")]
    InvalidTask { message: String },
    #[error("task not found: {task_id}")]
    TaskNotFound { task_id: String },
    #[error("{message}")]
    InvalidRequest { message: String },
}

// ---------------------------------------------------------------------------
// RPC params (wire camelCase; `parse_params` deserializes them in the engine).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemIdParams {
    pub system_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskIdParams {
    pub task_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosePositionParams {
    pub symbol: String,
    /// `ui:<id>` from the dashboard, `chat:<sessionId>` from the desk chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestIdParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountIdParams {
    pub account_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkIdParams {
    pub link_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mt5DetectParams {
    /// `true` also reads the live terminal's logged-in account.
    #[serde(default)]
    pub probe: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IbkrDetectParams {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolSelectParams {
    pub dir: String,
}

/// Systems that live in one folder of a git repository (Settings →
/// Sistemas): Aurea keeps a sparse copy of just that folder and uses it as
/// the systems folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoSource {
    /// `https://…` or `git@…` clone URL.
    pub url: String,
    /// Branch to follow; the repository's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The folder inside the repository holding the `NN_name` systems
    /// (`/`-separated, relative); empty for the repository's root.
    #[serde(default)]
    pub subdir: String,
}

/// What the last download or update of the repository brought.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoSyncReport {
    /// The local systems folder the copy provides.
    pub dir: String,
    pub systems: u32,
    /// The commit now checked out (short hash).
    pub commit: String,
    /// Files under the folder that changed in this update (empty on a first download).
    pub changed: Vec<String>,
    /// Epoch ms of the update.
    pub at: i64,
}

/// Settings → Sistemas: where the systems come from.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RepoSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<RepoSyncReport>,
}

/// `TradingRepoUpdate`: `force` updates even with systems running.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoUpdateParams {
    #[serde(default)]
    pub force: bool,
}
