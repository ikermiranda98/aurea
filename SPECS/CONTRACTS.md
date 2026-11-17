# CONTRACTS — Contratos proto/RPC de trading de Aurea

> **ESTADO: BORRADOR v0.1** (2026-11-17). Se **congela** cuando: (1) quitas Fase 0 integradas,
> (2) `cargo check --workspace` verde. Tras congelar, cualquier cambio aquí pasa por el orquestador.
> Fuente de comportamiento: `SPECS/A6.md` (superficie exacta del engine TS) y `SPECS/A1/A4/A5.md`.
> Nombres: internos y de ficheros siguen la convención Zeron; los métodos RPC son públicos.

## 1. Tipos — `crates/proto/src/trading.rs` (nuevo, wire `pub mod trading;` en proto lib)

Convenciones: `serde {rename_all = "camelCase"}`, `DateTime<Utc>` de chrono, `Option` con
`#[serde(default, skip_serializing_if = "Option::is_none")]`. La Lane A ajusta cualquier `Option`
al schema TS original (`packages/schema/src/trading.ts`) y reporta desviaciones.

```rust
pub struct System { pub id: String, pub name: String, pub running: bool, pub active: bool,
    pub pid: Option<u32>, pub started_at: Option<DateTime<Utc>>, pub symbol: Option<String>,
    pub timeframe: Option<String>, pub pnl_day: f64, pub client_id: Option<String>,
    pub close_hour: Option<u8>, pub last_activity_at: Option<DateTime<Utc>>,
    pub description: Option<String>, pub rules: Option<serde_json::Value> }

pub struct Account { pub id: String, pub kind: AccountKind /* live|paper|demo|watch */,
    pub cash: f64, pub equity: f64, pub pnl_day: f64, pub pnl_realized: f64,
    pub available_funds: Option<f64>, pub demo: Option<bool>, pub buying_power: Option<f64>,
    pub margin_used: Option<f64>, pub excess_liquidity: Option<f64>,
    pub updated_at: Option<DateTime<Utc>>, pub link: Option<String> }

pub struct Task { pub id: String /* trd_* */, pub kind: TaskKind, pub system_ids: Vec<String>,
    pub run_at: DateTime<Utc>, pub status: TaskStatus, pub resolved_at: Option<DateTime<Utc>>,
    pub time_created: DateTime<Utc>, pub time_updated: DateTime<Utc> }
pub enum TaskKind { Stop, CloseAll, Panic } // según schema TS
pub enum TaskStatus { Pending, Ran, Missed, Cancelled, Failed }

pub struct Position { pub symbol: String, pub side: PositionSide, pub quantity: f64,
    pub avg_price: Option<f64>, pub market_price: Option<f64>, pub unrealized_pnl: Option<f64>,
    pub account_id: Option<String>, pub system_id: Option<String>, pub closeable: bool }
pub enum PositionSide { Long, Short }

pub struct PositionCloseResult { pub ok: bool, pub message: String,
    pub message_key: Option<String>, pub message_params: Option<serde_json::Value> }
pub struct PanicResult { pub stopped: Vec<String>, pub close: PositionCloseResult,
    pub message: String, pub message_key: Option<String>, pub message_params: Option<serde_json::Value> }

pub struct Log { pub ts: DateTime<Utc>, pub level: LogLevel, pub system: Option<String>,
    pub message: String, pub message_key: Option<String>, pub message_params: Option<serde_json::Value>,
    pub event: Option<String> }
pub enum LogLevel { Debug, Info, Warn, Error }

pub struct Link { pub id: String, pub label: Option<String>, pub host: Option<String>,
    pub port: Option<u16>, pub live: Option<bool>, pub broker: Option<String>,
    pub mt5_login: Option<String>, pub mt5_server: Option<String> }

pub struct Snapshot { pub version: u32, pub connected: bool, pub client_in_use: Option<String>,
    pub account: Option<Account>, pub systems: Vec<System>, pub positions: Vec<Position>,
    pub open_positions: Option<Vec<Position>>, pub closable_positions: Option<Vec<Position>>,
    pub logs: Vec<Log>, pub tasks: Vec<Task>, pub trades: Option<Vec<serde_json::Value>>,
    pub equity: Option<Vec<EquityPoint>>, pub accounts: Option<Vec<Account>>,
    pub links: Option<Vec<Link>>, pub systems_dir: Option<String>,
    pub programs: Option<Vec<ProgramStatus /* proto::program_status */>> }
pub struct EquityPoint { pub t: DateTime<Utc>, pub equity: f64 }

pub struct TaskAddInput { pub kind: TaskKind, pub system_ids: Option<Vec<String>>,
    pub delay_minutes: Option<i64>, pub at_time: Option<DateTime<Utc>> } // delay/at excluyentes
pub struct LinkAddInput { pub host: Option<String>, pub port: Option<u16>, pub live: Option<bool>,
    pub label: Option<String>, pub broker: Option<String>, pub mt5_login: Option<String>, pub mt5_server: Option<String> }
pub struct PoolSelectResult { pub dir: String, pub systems: Vec<System> }
pub struct Mt5Detected { /* según schema TS: login, server, terminal, path… */ }
pub struct IbkrPort { pub host: String, pub port: u16, pub live: bool }

/// Errores del motor, serializados como `{code, message, params?}` en la `err` del RPC.
pub enum TradingError { SystemNotFound { system_id: String }, InvalidTask { message: String },
    TaskNotFound { task_id: String }, InvalidRequest { message: String } }
```

## 2. Métodos RPC — constantes en `crates/rpc/src/lib.rs` (`pub mod methods`)

Stream (el patrón `Watch*` de Zeron; cada item un `Snapshot` completo):

```rust
pub const WATCH_TRADING_SNAPSHOT: &str = "WatchTradingSnapshot";
```

Unary (params → ok):

```rust
pub const TRADING_SNAPSHOT:          &str = "TradingSnapshot";          // {} → Snapshot
pub const TRADING_SYSTEM_START:      &str = "TradingSystemStart";      // {systemId} → System   (engine: activate)
pub const TRADING_SYSTEM_STOP:       &str = "TradingSystemStop";       // {systemId} → System   (engine: pause)
pub const TRADING_TASK_LIST:         &str = "TradingTaskList";         // {} → [Task]
pub const TRADING_TASK_ADD:          &str = "TradingTaskAdd";          // TaskAddInput → Task
pub const TRADING_TASK_CANCEL:       &str = "TradingTaskCancel";       // {taskId} → Task
pub const TRADING_POSITION_CLOSE:    &str = "TradingPositionClose";    // {symbol, requestId?} → PositionCloseResult
pub const TRADING_POSITIONS_CLOSE_ALL: &str = "TradingPositionsCloseAll"; // {requestId?} → PositionCloseResult
pub const TRADING_PANIC:             &str = "TradingPanic";            // {} → PanicResult
pub const TRADING_CONNECT:           &str = "TradingConnect";          // {} → Account|null
pub const TRADING_DISCONNECT:        &str = "TradingDisconnect";       // {} → null
pub const TRADING_ACCOUNT_SELECT:    &str = "TradingAccountSelect";    // {accountId} → Account|null
pub const TRADING_LINK_ADD:          &str = "TradingLinkAdd";          // LinkAddInput → Link
pub const TRADING_LINK_REMOVE:       &str = "TradingLinkRemove";       // {linkId} → [Link]
pub const TRADING_MT5_DETECT:        &str = "TradingMt5Detect";        // {probe?} → [Mt5Detected]
pub const TRADING_IBKR_DETECT:       &str = "TradingIbkrDetect";       // {host?, port?} → [IbkrPort]
pub const TRADING_POOL_SELECT:       &str = "TradingPoolSelect";       // {dir} → PoolSelectResult
```

WARP (lane C, tipos en `crates/proto/src/warp.rs` — mínimo):

```rust
pub const WARP_STATUS:  &str = "WarpStatus";   // {} → {state: connected|connecting|disconnected|unavailable, day?, prefix?, lastRotateAt?}
pub const WARP_ROTATE:  &str = "WarpRotate";   // {} → {state, prefix}
pub const WARP_HISTORY: &str = "WarpHistory";  // {} → [{at, from, to, reason}]
```

## 3. Reglas de oro del wiring (orquestador)

1. `TradingPositionClose` / `TradingPositionsCloseAll` / `TradingPanic` llevan `requestId` opcional:
   UI genera `ui:<id>`; el chat del desk usa `chat:<sessionId>` → replay 24 h del veredicto (A4).
2. El gate de dinero en v1: (a) el motor exige `PermissionGate` inyectado en acciones de cierre;
   (b) la UI B7 muestra confirmación nativa; (c) las tools MCP **siempre** preguntan (sin always-allow).
3. `snapshot()` del tool `state` recorta `logs` a las últimas 20 — el RPC `TradingSnapshot` devuelve todo;
   el recorte es cosa del caller.
4. La cuenta en `Snapshot.account` puede ser watch (solo lectura): la UI no ofrece cierre para ella
   (regla `vistaOpera` de A6 §2.2, ahora nativa en la UI).
5. `TradingConnect`/`Disconnect` devuelven el estado en ese instante (null si aún no hay balances).

## 4. Mock (para que la UI vuele antes del engine)

`crates/trading/src/mock.rs` (orquestador): si el engine trading no está configurado, el match arm de
`WatchTradingSnapshot` sirve un stream con datos fixture (2 sistemas, 3 posiciones, 1 cuenta demo,
2 tasks, log de 5 entradas) y las acciones unarias devuelven respuestas simuladas inmediatas + evento
en el log del mock. UI indistinguible del real → QA visual (lane E) puede empezar sin engine.

## 5. Congelado

Al integrar (quitas + toolchain verde): escribir `proto/trading.rs` + `warp.rs`, constantes, mock,
match arms en `engine/src/rpc.rs` → `cargo check --workspace` verde → commit `contratos: trading proto/RPC`
→ cambiar ESTE encabezado a **CONGELADO v1**. Cambios posteriores: solo orquestador, con bump de versión
y nota en la tabla inferior.

| Versión | Fecha | Cambio |
|---|---|---|
| v0.1 | 2026-11-17 | Borrador desde SPECS/A6 + schema TS |
