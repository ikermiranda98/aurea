# CONTRACTS — Contratos proto/RPC de trading de Aurea

> **ESTADO: v1 — CONGELADO (2026-11-17 ~23:40)** — tipos en `crates/proto/src/trading.rs` +
> `warp.rs`, constantes en `crates/rpc/src/lib.rs`, brazos en `crates/engine/src/rpc.rs` y mock
> interactivo en `crates/trading/`. **`cargo check --workspace --all-targets` verde.**
> A partir de aquí: cualquier cambio requiere al orquestador y un bump de versión (§5).
> Fuente de comportamiento: `SPECS/A6.md` y `packages/schema/src/trading.ts` (Openbot).

## 1. Tipos (fuente de verdad = código)

`crates/proto/src/trading.rs` — port del schema TS, con estas decisiones:

| Tipo | Notas |
|---|---|
| `Broker` | `ibkr \| nt \| mt5` (lowercase). `nt` = NinjaTrader, previsto en el schema. |
| `Link` | `id, host?, port?, label (nullable), broker?, mt5_login? (NÚMERO), mt5_server?, connected (bool requerido)`. |
| `Account` | `type` es string libre (campo Rust `kind` con `rename = "type"`); `cash/equity/pnl_day/pnl_realized` requeridos-nullables; tags IBKR optional-nullable; `updated_at` epoch ms; `link?`. |
| `System` | `started_at`/`last_activity_at` epoch ms (`i64`); `pid: Option<u32>`; `client_id: Option<i64>`; `rules: Option<BTreeMap<String,String>>`. |
| `Position` | `symbol, side(long\|short), size, entry?, current?, pnl?, pnl_pct?` — exacto del schema, sin account_id ni closeable. |
| `PositionCloseResult` | `ok, message, message_key?, message_params?`. |
| `PanicResult` | `stopped: Vec<String>, close: PositionCloseResult, message, message_key?, message_params?`. |
| `Log` | `ts` epoch ms; `level: info\|warning\|error`; `system` requerido ("motor" = engine-wide); `event?: open\|close\|signal`. |
| `Task` | `id` (`trd_`), `kind: start-systems\|stop-systems` (**solo dos**), `system_ids` (vacío = todos), `run_at` epoch ms PC, `status: pending\|ran\|missed\|cancelled\|failed`, `resolved_at` nullable, `time_created/time_updated`. |
| `Trade` | `system, ts, pnl?, message` (de registro.log, cap 200, newest first). |
| `EquityPoint` | `ts, equity?, pnl_day?` (cada 15 s, cap 1000, oldest first). |
| `ProgramState` | `running \| opening \| closed \| missing`. |
| `Programs` | `{ tws: ProgramState, mt5?: ProgramState }` — forma exacta del schema. |
| `Snapshot` | `version: String`; `connected`; `client_in_use?`; `account` nullable; `systems`; `positions`; `open_positions?`; `closable_positions?`; `logs`; `tasks`; `trades?`; `equity?`; `accounts?`; `links?`; `systems_dir?`; `programs?`. |
| `Mt5Detected` | `login: i64, server, name?, active, live`. |
| `IbkrPort` | `host, port: u16, live, open` (`live` = 7496; `open` = algo aceptó TCP). |
| `PoolSelectResult` | `dir: String, systems: u32` — **recuento**, no lista. |
| `TradingError` | serde tag `code` snake_case: `system_not_found`, `invalid_task`, `task_not_found`, `invalid_request` + message. |

### Inputs de acciones (camelCase en el cable)

- `TaskAddInput`: `kind`, `systemIDs?` (mayúsculas exactas del core), `delayMinutes?` (número
  positivo), `atTime?` (**string "HH:MM"** local; mutuamente excluyentes). Validado en el mock
  igual que `trading.ts` (`Give either delayMinutes or atTime, not both.`).
- `LinkAddInput`: `host?`, `port?`, `live?`, `label?`, `broker?`, `mt5Login?` (**número**),
  `mt5Server?`.
- Params unarios definidos en el proto: `SystemIdParams{systemId}`, `TaskIdParams{taskId}`,
  `ClosePositionParams{symbol, requestId?}`, `RequestIdParams{requestId?}`,
  `AccountIdParams{accountId}`, `LinkIdParams{linkId}`, `Mt5DetectParams{probe?}`,
  `IbkrDetectParams{host?,port?}`, `PoolSelectParams{dir}`.

## 2. Métodos RPC (constantes en `crates/rpc/src/lib.rs`, brazos en `engine/rpc.rs`)

Stream: `WatchTradingSnapshot` (cada item = `Snapshot` completo).

Unary: `TradingSnapshot`, `TradingSystemStart`/`Stop`, `TradingTaskList`, `TradingTaskAdd`,
`TradingTaskCancel`, `TradingPositionClose`, `TradingPositionsCloseAll`, `TradingPanic`,
`TradingConnect`/`TradingDisconnect`, `TradingAccountSelect`, `TradingLinkAdd`/`LinkRemove`,
`TradingMt5Detect`/`TradingIbkrDetect`, `TradingPoolSelect`.

WARP: `WarpStatus`, `WarpRotate`, `WarpHistory` — **hoy devuelven `Unavailable`/vacío** hasta que
exista `crates/warp` (lane C); la superficie no miente ni finge un túnel.

## 3. Reglas de oro del wiring

1. `TradingPositionClose`/`TradingPositionsCloseAll`/`TradingPanic` con `requestId`: UI genera
   `ui:<id>`; chat del desk `chat:<sessionId>` → replay 24 h del veredicto (A4). HTTP sin
   requestId → id anónimo con sello (sin replay).
2. Gate de dinero v1: (a) `PermissionGate` inyectado en el engine para cierres; (b) confirmación
   nativa en UI (B7); (c) tools MCP **siempre** preguntan (sin always-allow).
3. Tool `state` recorta `logs` a 20; el RPC completo devuelve todo.
4. Cuenta watch = solo lectura (la UI no ofrece cierre); regla `vistaOpera` de A6.
5. `TradingConnect/Disconnect` devuelven la cuenta en ese instante (null sin balances aún).

## 4. Mock interactivo (desbloquea la UI antes del engine real)

`crates/trading/src/mock.rs`: snapshot vivo cada 2 s con 2 sistemas (MT5/IBKR), 3 posiciones,
1 cuenta demo, 2 tasks, 5 logs, 24 puntos de equity, 2 links y `programs` de TWS/MT5.

Las acciones **mutan** el fixture y republican de inmediato (el dashboard ya es interactivo):

| Acción | Efecto simulado |
|---|---|
| `TradingSystemStart/Stop` | flags `running/pid/started_at` + log `trading.log.system_started/stopped` |
| `TradingPositionClose` | retira la fila + veredicto `ok` + log `trading.close.ok` |
| `TradingPositionsCloseAll` | cierra todas + `trading.close.all_ok` |
| `TradingPanic` | para todos los bots y cierra todo + log error `trading.panic.done` |
| `TradingTaskAdd/Cancel` | valida HH:MM/minutos, crea `trd_mockNNNN` o cancela con `resolved_at` |
| `TradingConnect/Disconnect` | `connected` + vacía cuenta/posiciones al desconectar |
| `TradingAccountSelect` | acepta la cuenta demo, error `invalid_request` si no existe |
| `TradingLinkAdd/Remove` | construye `host:port` o `mt5-login@server`, retira del snapshot |
| `TradingMt5Detect` / `TradingIbkrDetect` | 1 cuenta demo / knock 7496 cerrado + 7497 abierto |
| `TradingPoolSelect` | cambia `systems_dir` y devuelve el recuento |

## 5. Versionado

| Versión | Fecha | Cambio |
|---|---|---|
| v0.1 | 2026-11-17 | Borrador desde SPECS/A6 + schema TS |
| v0.2 | 2026-11-17 | Tipos exactos del schema en `proto/trading.rs` + `warp.rs` + constantes RPC |
| v0.3 | 2026-11-17 | Correcciones del barrido completo: `Broker::Nt`, `Link.connected`/`mt5_login` numérico, `IbkrPort.open`, `Programs` exacto, `PoolSelectResult.systems` recuento, `TaskAddInput` camelCase con `atTime` "HH:MM"; brazos RPC (20 + 3 WARP) y mock interactivo |
| **v1** | 2026-11-17 | **Congelado** con el workspace compilando verde (`cargo check --workspace --all-targets`). Sin cambios de forma respecto a v0.3: solo se certifica lo ya implementado. |
