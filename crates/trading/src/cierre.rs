//! `cerrarUna` — one real close cycle against TWS (A4, `SPECS/A4.md` §2.5).
//!
//! Port of `trading.ts:2900-3253`, guard for guard:
//!
//! 1. TWS live, the position has size, its contract has a `conId`, no other
//!    close of that contract is in flight;
//! 2. a lean contract (no exchange, or `SMART`) is completed with
//!    `REQ_CONTRACT_DATA`, which also yields the contract's `minTick`;
//! 3. the contract's open orders: a foreign one aborts the close with nothing
//!    cancelled; the desk's own are cancelled and must be confirmed gone;
//! 4. the position is re-read on the fixed account right before ordering;
//! 5. a market order (`DAY`, outside RTH, `transmit`), waited for up to
//!    `pasos × paso`; the position leaving the stream counts as the fill;
//! 6. a market order still alive is cancelled and must reach a terminal
//!    state, or no limits go out (they could double the close);
//! 7. whatever is left goes into two OCA `GTC` limits ±4 ticks around the
//!    last price, on the contract's tick grid.
//!
//! Order state is asked fresh each step, as Openbot does: open orders by id
//! **and** reference, else today's executions of this client summed by id and
//! reference. A TWS refusal of one of these orders (`RECHAZOS_ORDEN`) ends it
//! on the spot as `Inactive`, with TWS's reason in the verdict.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::sync::broadcast;
use zeron_proto::trading::{LogLevel, Position, PositionCloseResult, PositionSide};
use zeron_tws::inbound::IbInbound;
use zeron_tws::orders::{self, ContractSpec, OrderSpec};

use crate::claims;
use crate::service::TradingService;

/// How long the cycle waits at each step. Openbot's values by default; tests
/// shorten them.
#[derive(Debug, Clone, Copy)]
pub struct Tiempos {
    /// One poll of the market order's state (250 ms).
    pub paso: Duration,
    /// Polls per wait (20 → five seconds).
    pub pasos: usize,
    /// The pause after cancelling, and after placing the limits (500 ms).
    pub pausa: Duration,
    /// The longest a request may wait for its answer.
    pub respuesta: Duration,
}

impl Default for Tiempos {
    fn default() -> Self {
        Self {
            paso: Duration::from_millis(250),
            pasos: 20,
            pausa: Duration::from_millis(500),
            respuesta: Duration::from_secs(5),
        }
    }
}

/// TWS's codes for an order it refused or dropped (`RECHAZOS_ORDEN`,
/// `trading.ts`). Warnings that ride an accepted order (399, 2109, 10349)
/// are deliberately absent: those orders still work and may fill.
const RECHAZOS_ORDEN: &[i64] = &[
    103, 104, 105, 106, 107, 109, 110, 111, 113, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125,
    126, 129, 131, 132, 133, 134, 135, 136, 137, 140, 141, 144, 146, 147, 148, 151, 152, 153, 154,
    155, 156, 158, 159, 160, 163, 164, 166, 167, 168, 169, 200, 201, 202, 203, 312, 313, 314, 315,
    320, 321, 322, 323, 324, 325, 327, 328, 329, 383, 387, 388,
];

fn es_rechazo(code: i64) -> bool {
    RECHAZOS_ORDEN.contains(&code)
}

/// One close in flight per contract (`cierresEnVuelo`).
static EN_VUELO: LazyLock<Mutex<HashSet<i64>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
/// The tick each contract-details answer reported (`ticksPorConId`).
static TICKS: LazyLock<Mutex<HashMap<i64, f64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// Request ids for details/executions; far from the PnL and summary ids.
static REQ_ID: AtomicI64 = AtomicI64::new(70_000);

fn next_req_id() -> i64 {
    REQ_ID.fetch_add(1, Ordering::Relaxed)
}

struct EnVuelo(i64);
impl Drop for EnVuelo {
    fn drop(&mut self) {
        if let Ok(mut set) = EN_VUELO.lock() {
            set.remove(&self.0);
        }
    }
}

fn verdict(ok: bool, message: String, key: &str, params: Value) -> PositionCloseResult {
    PositionCloseResult {
        ok,
        message,
        message_key: Some(key.into()),
        message_params: params.as_object().cloned().or_else(|| Some(Map::new())),
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Waits for messages until `pick` says done, or `timeout` passes.
async fn recoger<T>(
    rx: &mut broadcast::Receiver<IbInbound>,
    timeout: Duration,
    mut pick: impl FnMut(IbInbound) -> Option<Result<T, String>>,
) -> Result<T, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let message = match tokio::time::timeout_at(deadline, rx.recv()).await {
            Err(_) => return Err("TWS no respondió a tiempo".into()),
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) => return Err("enlace cerrado".into()),
            Ok(Ok(message)) => message,
        };
        if let Some(done) = pick(message) {
            return done;
        }
    }
}

/// One open order as the cycle sees it.
#[derive(Debug, Clone)]
struct OrdenViva {
    order_id: i64,
    con_id: i64,
    order_ref: String,
    /// `None` when TWS sent no status for it: unknown, so treated as working.
    status: Option<String>,
    filled: f64,
}

impl OrdenViva {
    fn activa(&self) -> bool {
        !self.status.as_deref().is_some_and(orders::is_terminal)
    }
}

async fn ordenes_abiertas(svc: &TradingService, t: &Tiempos) -> Result<Vec<OrdenViva>, String> {
    let mut rx = svc.subscribe_inbound();
    if !svc.send(orders::req_all_open_orders()) {
        return Err("sin enlace con TWS".into());
    }
    let mut abiertas: Vec<OrdenViva> = Vec::new();
    let mut estados: HashMap<i64, (String, f64)> = HashMap::new();
    recoger(&mut rx, t.respuesta, |message| match message {
        IbInbound::OpenOrder(row) => {
            abiertas.push(OrdenViva {
                order_id: row.order_id,
                con_id: row.contract.con_id,
                order_ref: row.order_ref,
                status: None,
                filled: 0.0,
            });
            None
        }
        IbInbound::OrderStatus(row) => {
            estados.insert(row.order_id, (row.status, row.filled));
            None
        }
        IbInbound::OpenOrderEnd => Some(Ok(())),
        _ => None,
    })
    .await?;
    for orden in &mut abiertas {
        if let Some((status, filled)) = estados.get(&orden.order_id) {
            orden.status = Some(status.clone());
            orden.filled = *filled;
        }
    }
    Ok(abiertas)
}

async fn ejecuciones(
    svc: &TradingService,
    t: &Tiempos,
) -> Result<Vec<orders::ExecutionRow>, String> {
    let req_id = next_req_id();
    let mut rx = svc.subscribe_inbound();
    if !svc.send(orders::req_executions(req_id, svc.client_id())) {
        return Err("sin enlace con TWS".into());
    }
    let mut filas = Vec::new();
    recoger(&mut rx, t.respuesta, |message| match message {
        IbInbound::Execution(row) if row.req_id == req_id => {
            filas.push(row);
            None
        }
        IbInbound::ExecutionEnd { req_id: end } if end == req_id => Some(Ok(())),
        _ => None,
    })
    .await?;
    Ok(filas)
}

/// The completed contract and its tick, or the reason TWS gave.
async fn resolver_contrato(
    svc: &TradingService,
    version: i32,
    base: &ContractSpec,
    t: &Tiempos,
) -> Result<Option<(ContractSpec, Option<f64>)>, String> {
    let req_id = next_req_id();
    let request = orders::req_contract_details(version, req_id, &base.details_query())
        .map_err(|error| error.to_string())?;
    let mut rx = svc.subscribe_inbound();
    if !svc.send(request) {
        return Err("sin enlace con TWS".into());
    }
    let mut encontrado = None;
    recoger(&mut rx, t.respuesta, |message| match message {
        IbInbound::ContractDetails(row) if row.req_id == req_id => {
            if row.contract.con_id == base.con_id {
                let mut contract = ContractSpec::from_contract(&row.contract);
                if !row.last_trade_date.is_empty() {
                    contract.last_trade_date = row.last_trade_date.clone();
                }
                encontrado = Some((contract, row.min_tick));
            }
            None
        }
        IbInbound::ContractDetailsEnd { req_id: end } if end == req_id => Some(Ok(())),
        IbInbound::Error { req_id: id, code, message } if id == req_id => {
            Some(Err(format!("{code}: {message}")))
        }
        _ => None,
    })
    .await?;
    Ok(encontrado)
}

/// What the cycle knows about its own orders (`ordenesIds`, `rechazosOrden`).
#[derive(Default)]
struct Seguimiento {
    estados: HashMap<i64, (String, f64)>,
    rechazos: HashMap<i64, String>,
    propias: HashSet<i64>,
}

impl Seguimiento {
    /// Folds in what TWS said on its own since the last look: live status
    /// updates and refusals of the cycle's orders.
    fn drenar(&mut self, rx: &mut broadcast::Receiver<IbInbound>) {
        loop {
            match rx.try_recv() {
                Ok(IbInbound::OrderStatus(row)) if self.propias.contains(&row.order_id) => {
                    self.estados.insert(row.order_id, (row.status, row.filled));
                }
                Ok(IbInbound::Error { req_id, code, message })
                    if self.propias.contains(&req_id) && es_rechazo(code) =>
                {
                    self.rechazos.insert(req_id, format!("{code}: {message}"));
                    let filled = self.llenado(req_id);
                    self.estados.insert(req_id, ("Inactive".into(), filled));
                }
                Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    }

    fn estado(&self, id: i64) -> Option<String> {
        self.estados.get(&id).map(|(status, _)| status.clone())
    }

    fn llenado(&self, id: i64) -> f64 {
        self.estados
            .get(&id)
            .map(|(_, filled)| *filled)
            .filter(|filled| filled.is_finite())
            .unwrap_or(0.0)
    }
}

/// `refrescarOrden`: TWS's word on one of the cycle's orders, asked fresh.
async fn refrescar(
    svc: &TradingService,
    seg: &mut Seguimiento,
    rx: &mut broadcast::Receiver<IbInbound>,
    id: i64,
    referencia: &str,
    cantidad: f64,
    t: &Tiempos,
) {
    seg.drenar(rx);
    if seg.rechazos.contains_key(&id) {
        return;
    }
    if let Ok(abiertas) = ordenes_abiertas(svc, t).await
        && let Some(viva) = abiertas
            .iter()
            .find(|orden| orden.order_id == id && orden.order_ref == referencia)
    {
        let status = viva.status.clone().unwrap_or_else(|| "Submitted".into());
        seg.estados.insert(id, (status, viva.filled));
        return;
    }
    let Ok(filas) = ejecuciones(svc, t).await else {
        return;
    };
    let llenado: f64 = filas
        .iter()
        .filter(|fila| fila.order_id == id && fila.order_ref == referencia)
        .map(|fila| fila.shares)
        .sum();
    if llenado <= 0.0 {
        return;
    }
    let status = if llenado >= cantidad {
        "Filled".to_string()
    } else {
        seg.estado(id).unwrap_or_else(|| "Submitted".into())
    };
    seg.estados.insert(id, (status, llenado));
}

fn viva_en(svc: &TradingService, account: &str, symbol: &str) -> Option<Position> {
    svc.account_positions(account)
        .into_iter()
        .find(|row| row.symbol == symbol && row.size > 0.0)
}

/// Sends one order; the id on success, else why not.
fn enviar(
    svc: &TradingService,
    seg: &mut Seguimiento,
    version: i32,
    contract: &ContractSpec,
    order: &OrderSpec,
) -> Result<i64, String> {
    let id = svc
        .allocate_order_id()
        .ok_or_else(|| "TWS aún no ha dado un id de orden".to_string())?;
    let frame = orders::place_order(version, id, contract, order).map_err(|e| e.to_string())?;
    seg.propias.insert(id);
    if !svc.send(frame) {
        return Err("sin enlace con TWS".into());
    }
    Ok(id)
}

fn cancelar(svc: &TradingService, version: i32, id: i64) {
    if let Ok(frame) = orders::cancel_order(version, id) {
        svc.send(frame);
    }
}

fn js_qty(value: f64) -> String {
    format!("{value}")
}

/// One full close of `position` on `account`, with Openbot's timings.
pub async fn cerrar_una(
    position: &Position,
    account: &str,
    svc: &TradingService,
) -> PositionCloseResult {
    cerrar_una_con(position, account, svc, &Tiempos::default()).await
}

/// [`cerrar_una`] with explicit timings.
pub async fn cerrar_una_con(
    position: &Position,
    account: &str,
    svc: &TradingService,
    t: &Tiempos,
) -> PositionCloseResult {
    let symbol = position.symbol.clone();
    let params = |extra: Value| {
        let mut base = json_obj(&[("symbol", Value::String(symbol.clone()))]);
        if let Value::Object(more) = extra {
            base.extend(more);
        }
        Value::Object(base)
    };

    // 1. A live link.
    let conectado = svc.live() && svc.tws_client().is_some_and(|client| client.is_connected());
    if !conectado {
        return verdict(
            false,
            format!("TWS not connected for {symbol}."),
            "trading.close.no_tws",
            params(Value::Null),
        );
    }
    // 2. Something to close.
    if !(position.size > 0.0) {
        return verdict(
            false,
            format!("No size to close for {symbol}."),
            "trading.close.no_size",
            params(Value::Null),
        );
    }
    // 3. The full contract.
    let Some(contrato_pos) = svc
        .position_contract(account, &symbol)
        .filter(|contract| contract.con_id != 0)
    else {
        return verdict(
            false,
            format!("I don't have the full contract for {symbol}: refresh the position and retry."),
            "trading.close.no_contract",
            params(Value::Null),
        );
    };
    let con_id = contrato_pos.con_id;
    let Some(version) = svc
        .server_version()
        .filter(|v| (orders::MIN_ORDER_SERVER_VERSION..=orders::MAX_ORDER_SERVER_VERSION).contains(v))
    else {
        let found = svc.server_version().map(|v| v.to_string()).unwrap_or_else(|| "?".into());
        return verdict(
            false,
            format!("TWS server version {found} is not supported for orders: close {symbol} in TWS."),
            "trading.close.wire_unsupported",
            params(serde_json::json!({ "version": found })),
        );
    };
    // 4. One close per contract.
    {
        let mut set = EN_VUELO.lock().unwrap_or_else(|p| p.into_inner());
        if !set.insert(con_id) {
            svc.push_log(
                LogLevel::Warning,
                format!("Cierre de {symbol} · ya hay otro cierre en curso para este contrato"),
            );
            return claims::ocupado(&symbol);
        }
    }
    let _en_vuelo = EnVuelo(con_id);

    // 5. Complete a lean contract.
    let mut contrato = ContractSpec::from_contract(&contrato_pos);
    if contrato.exchange.is_empty() || contrato.exchange == "SMART" {
        match resolver_contrato(svc, version, &contrato, t).await {
            Err(error) => {
                return verdict(
                    false,
                    format!("Could not complete the definition of {symbol}: {error}"),
                    "trading.close.def_failed",
                    params(serde_json::json!({ "error": error })),
                );
            }
            Ok(None) => {
                return verdict(
                    false,
                    format!("TWS did not return the full definition of {symbol}: close it from TWS."),
                    "trading.close.def_incomplete",
                    params(Value::Null),
                );
            }
            Ok(Some((resuelto, min_tick))) => {
                if let Some(tick) = min_tick.filter(|tick| tick.is_finite() && *tick > 0.0) {
                    TICKS.lock().unwrap_or_else(|p| p.into_inner()).insert(con_id, tick);
                }
                contrato = resuelto;
            }
        }
    }
    let contrato = contrato.for_order();
    let tick = claims::tick_de_contrato(
        TICKS.lock().unwrap_or_else(|p| p.into_inner()).get(&con_id).copied(),
        &symbol,
    );
    svc.push_log(
        LogLevel::Info,
        format!(
            "Cierre de {symbol} · órdenes a la cuenta {account} (tick {})",
            claims::texto_precio(tick, tick)
        ),
    );

    // 6. The contract's open orders.
    let es_propia = |orden: &OrdenViva| orden.order_ref.starts_with("escritorio-cierre");
    let leidas = match ordenes_abiertas(svc, t).await {
        Ok(leidas) => leidas,
        Err(error) => {
            return verdict(
                false,
                format!("Could not read the open orders: {error}"),
                "trading.close.read_orders_failed",
                params(serde_json::json!({ "error": error })),
            );
        }
    };
    let pendientes: Vec<OrdenViva> = leidas
        .into_iter()
        .filter(|orden| orden.con_id == con_id && orden.activa())
        .collect();
    let ajenas: Vec<&OrdenViva> = pendientes.iter().filter(|orden| !es_propia(orden)).collect();
    if !ajenas.is_empty() {
        let mut quien: Vec<String> = Vec::new();
        for orden in &ajenas {
            let nombre = if orden.order_ref.trim().is_empty() {
                "orden sin referencia".to_string()
            } else {
                orden.order_ref.trim().to_string()
            };
            if !quien.contains(&nombre) {
                quien.push(nombre);
            }
        }
        let quien = quien.join(", ");
        svc.push_log(
            LogLevel::Error,
            format!(
                "Cierre de {symbol} · {} orden(es) del contrato no son de la mesa ({quien}): no se canceló nada ajeno y no se envió el cierre",
                ajenas.len()
            ),
        );
        return verdict(
            false,
            format!(
                "Not closed: {symbol} has {} open order(s) that are not the desk's ({quien}). Cancel them in TWS (or stop the bot that placed them) and retry.",
                ajenas.len()
            ),
            "trading.close.foreign_orders",
            params(serde_json::json!({ "count": ajenas.len(), "who": quien })),
        );
    }
    let mut canceladas = 0usize;
    if !pendientes.is_empty() {
        for orden in &pendientes {
            cancelar(svc, version, orden.order_id);
        }
        tokio::time::sleep(t.pausa).await;
        let releidas = match ordenes_abiertas(svc, t).await {
            Ok(releidas) => releidas,
            Err(error) => {
                return verdict(
                    false,
                    format!("Could not verify the cancellation: {error}"),
                    "trading.close.cancel_verify_failed",
                    params(serde_json::json!({ "error": error })),
                );
            }
        };
        let restantes = releidas
            .iter()
            .filter(|orden| orden.con_id == con_id && orden.activa())
            .count();
        if restantes > 0 {
            svc.push_log(
                LogLevel::Error,
                format!("Cierre de {symbol} · {restantes} orden(es) propia(s) sin cancelación confirmada, no se envió ninguna orden"),
            );
            return verdict(
                false,
                format!("Not closed: {restantes} desk order(s) on {symbol} remain without confirmed cancellation. Cancel them in TWS and retry."),
                "trading.close.cancel_pending",
                params(serde_json::json!({ "count": restantes })),
            );
        }
        canceladas = pendientes.len();
    }
    let extra = if canceladas > 0 {
        format!(" · {canceladas} orden(es) propia(s) cancelada(s)")
    } else {
        String::new()
    };
    let progreso = |mensaje: String, nivel: LogLevel| {
        svc.push_log(nivel, format!("Cierre de {symbol}{extra} · {mensaje}"));
    };

    // 7. The live position, right before ordering.
    let Some(viva) = viva_en(svc, account, &symbol) else {
        progreso("la posición desapareció durante la preparación · nada que cerrar".into(), LogLevel::Info);
        return verdict(
            true,
            format!("The position of {symbol} is no longer open: it closed while preparing the close{extra}."),
            "trading.close.gone_while_prep",
            params(serde_json::json!({ "extra": extra })),
        );
    };
    if viva.side != position.side || viva.size != position.size {
        progreso(
            "la posición cambió de lado o de tamaño durante la preparación · no se envió orden alguna".into(),
            LogLevel::Warning,
        );
        return verdict(
            false,
            format!("Not closed: the position of {symbol} changed side or size while preparing. Check Orders and retry if still open."),
            "trading.close.changed_while_prep",
            params(Value::Null),
        );
    }
    let accion = if viva.side == PositionSide::Long { "SELL" } else { "BUY" };
    let cantidad = viva.size;

    // 8. The market order.
    let mut rx = svc.subscribe_inbound();
    let mut seg = Seguimiento::default();
    let sello = now_ms();
    let ref_mercado = format!("escritorio-cierre-{con_id}-{sello}");
    let orden_mercado = OrderSpec {
        action: accion.into(),
        quantity: cantidad,
        order_type: "MKT".into(),
        lmt_price: None,
        tif: "DAY".into(),
        oca_group: None,
        oca_type: None,
        account: account.into(),
        order_ref: ref_mercado.clone(),
        outside_rth: true,
        transmit: true,
    };
    let mut error_mercado = String::new();
    let id_mercado = match enviar(svc, &mut seg, version, &contrato, &orden_mercado) {
        Ok(id) => Some(id),
        Err(error) => {
            error_mercado = error;
            None
        }
    };
    let fuera = || viva_en(svc, account, &symbol).is_none();
    let mut estado: Option<String> = None;
    if let Some(id) = id_mercado {
        for _ in 0..t.pasos {
            tokio::time::sleep(t.paso).await;
            refrescar(svc, &mut seg, &mut rx, id, &ref_mercado, cantidad, t).await;
            estado = seg.estado(id);
            if estado.as_deref() != Some("Filled") && fuera() {
                estado = Some("Filled".into());
            }
            if estado.as_deref().is_some_and(orders::is_terminal) {
                break;
            }
        }
        if estado.as_deref() != Some("Filled") {
            estado = seg.estado(id);
        }
        if let Some(rechazo) = seg.rechazos.get(&id) {
            error_mercado = rechazo.clone();
            svc.push_log(
                LogLevel::Error,
                format!("Cierre de {symbol} · TWS rechazó la orden a mercado {id}: {rechazo}"),
            );
        }
    }

    // 9. Filled.
    if estado.as_deref() == Some("Filled") {
        let id = id_mercado.unwrap_or_default();
        progreso(format!("ejecutado · {accion} {} (orden {id})", js_qty(cantidad)), LogLevel::Info);
        return verdict(
            true,
            format!("Close executed: {accion} {} {symbol} (order {id}){extra}.", js_qty(cantidad)),
            "trading.close.executed_market",
            params(serde_json::json!({
                "action": accion, "quantity": cantidad, "order": id, "extra": extra,
            })),
        );
    }

    // 10. Still alive or unknown: cancel, and only go on once it is terminal.
    if let Some(id) = id_mercado
        && !estado.as_deref().is_some_and(orders::is_terminal)
    {
        cancelar(svc, version, id);
        for _ in 0..t.pasos {
            refrescar(svc, &mut seg, &mut rx, id, &ref_mercado, cantidad, t).await;
            estado = seg.estado(id);
            if estado.as_deref() == Some("Filled") || fuera() {
                progreso(format!("la orden a mercado acabó cubriendo la posición (orden {id})"), LogLevel::Info);
                return verdict(
                    true,
                    format!("Close executed: order {id} of {symbol}."),
                    "trading.close.executed_order",
                    params(serde_json::json!({ "order": id })),
                );
            }
            if estado.as_deref().is_some_and(orders::is_terminal) {
                break;
            }
            tokio::time::sleep(t.paso).await;
        }
        estado = seg.estado(id);
        if estado.as_deref() != Some("Filled") && !estado.as_deref().is_some_and(orders::is_terminal) {
            let mostrado = estado.clone().unwrap_or_else(|| "desconocido".into());
            progreso(
                format!("la orden a mercado {id} no se ejecutó y no se pudo cancelar (estado {mostrado}) · sin límites para evitar un doble cierre"),
                LogLevel::Error,
            );
            return verdict(
                false,
                format!("The market order of {symbol} did not execute and could not be cancelled (state {}). No limits were sent to avoid a double close; check Orders and TWS.", estado.as_deref().unwrap_or("unknown")),
                "trading.close.market_not_cancelled",
                params(serde_json::json!({ "state": estado.as_deref().unwrap_or("unknown") })),
            );
        }
    }

    // 11. What is left goes to two OCA limits.
    let llenado = id_mercado.map(|id| seg.llenado(id)).unwrap_or(0.0);
    let mut restante = cantidad - cantidad.min(llenado);
    if restante <= 0.0 {
        progreso("la posición quedó cerrada durante el envío".into(), LogLevel::Info);
        return verdict(
            true,
            format!("The position of {symbol} closed during the send{extra}."),
            "trading.close.closed_during_send",
            params(serde_json::json!({ "extra": extra })),
        );
    }
    let Some(viva_final) = viva_en(svc, account, &symbol) else {
        progreso("la posición desapareció durante la espera · cerrada".into(), LogLevel::Info);
        return verdict(
            true,
            format!("The position of {symbol} closed during the wait{extra}."),
            "trading.close.closed_during_wait",
            params(serde_json::json!({ "extra": extra })),
        );
    };
    if viva_final.side != position.side {
        progreso("la posición cambió de lado durante la espera · sin límites".into(), LogLevel::Error);
        return verdict(
            false,
            format!("No limits sent: the position of {symbol} changed side during the wait. Check Orders and TWS."),
            "trading.close.side_changed_wait",
            params(Value::Null),
        );
    }
    restante = restante.min(viva_final.size);
    if restante <= 0.0 {
        progreso("la posición quedó cerrada durante la espera".into(), LogLevel::Info);
        return verdict(
            true,
            format!("The position of {symbol} closed during the wait{extra}."),
            "trading.close.closed_during_wait",
            params(serde_json::json!({ "extra": extra })),
        );
    }
    let motivo = if error_mercado.is_empty() {
        format!(" (estado {})", estado.as_deref().unwrap_or("desconocido"))
    } else {
        format!(" ({error_mercado})")
    };
    let Some(precio) = viva_final.current.or(position.current) else {
        progreso(
            format!("TWS no aceptó el cierre a mercado{motivo}, sin precio actual para los límites"),
            LogLevel::Error,
        );
        return verdict(
            false,
            format!("TWS did not accept the market close of {symbol}{motivo} and there is no current price to compute the limits. Retry when a quote arrives or close it in TWS."),
            "trading.close.no_price_for_limits",
            params(serde_json::json!({ "reason": motivo })),
        );
    };
    let salto = tick * 4.0;
    let arriba = claims::en_rejilla(precio + salto, tick);
    let abajo = claims::en_rejilla(precio - salto, tick);
    if !(abajo > 0.0 && arriba > abajo) {
        let texto = claims::texto_precio(precio, tick);
        return verdict(
            false,
            format!("I cannot compute two valid limits for {symbol} around {texto}."),
            "trading.close.invalid_limits",
            params(serde_json::json!({ "price": texto })),
        );
    }
    let grupo = format!("cierre-{con_id}-{sello}");
    let ref_limite = format!("escritorio-cierre-limite-{con_id}-{sello}");
    let mut limites: Vec<(f64, Option<i64>, String)> = Vec::new();
    for precio_limite in [arriba, abajo] {
        let limite = OrderSpec {
            action: accion.into(),
            quantity: restante,
            order_type: "LMT".into(),
            lmt_price: Some(precio_limite),
            tif: "GTC".into(),
            oca_group: Some(grupo.clone()),
            oca_type: Some(1),
            account: account.into(),
            order_ref: ref_limite.clone(),
            outside_rth: true,
            transmit: true,
        };
        match enviar(svc, &mut seg, version, &contrato, &limite) {
            Ok(id) => limites.push((precio_limite, Some(id), String::new())),
            Err(error) => {
                svc.push_log(
                    LogLevel::Error,
                    format!(
                        "Cierre de {symbol} · TWS rechazó el límite {}: {error}",
                        claims::texto_precio(precio_limite, tick)
                    ),
                );
                limites.push((precio_limite, None, error));
            }
        }
    }
    tokio::time::sleep(t.pausa).await;
    for (_, id, _) in &limites {
        if let Some(id) = id {
            refrescar(svc, &mut seg, &mut rx, *id, &ref_limite, restante, t).await;
        }
    }
    seg.drenar(&mut rx);
    let sigue = |id: i64| !seg.estado(id).as_deref().is_some_and(orders::is_terminal);
    let aceptados: Vec<&(f64, Option<i64>, String)> = limites
        .iter()
        .filter(|(_, id, _)| id.is_some_and(sigue))
        .collect();
    let rechazados: Vec<&(f64, Option<i64>, String)> = limites
        .iter()
        .filter(|(_, id, _)| !id.is_some_and(sigue))
        .collect();
    if aceptados.is_empty() && !rechazados.iter().any(|(_, id, _)| id.is_some()) {
        progreso(format!("el cierre a mercado falló{motivo} y TWS tampoco aceptó los límites"), LogLevel::Error);
        return verdict(
            false,
            format!("The market close of {symbol} failed{motivo} and TWS did not accept the limits either. Check Orders and TWS."),
            "trading.close.market_failed_no_limits",
            params(serde_json::json!({ "reason": motivo })),
        );
    }
    if aceptados.is_empty() {
        let estados = rechazados
            .iter()
            .map(|(precio, id, error)| {
                let razon = if !error.is_empty() {
                    error.clone()
                } else {
                    id.and_then(|id| seg.rechazos.get(&id).cloned().or_else(|| seg.estado(id)))
                        .unwrap_or_default()
                };
                format!("{}: {razon}", claims::texto_precio(*precio, tick))
            })
            .collect::<Vec<_>>()
            .join(", ");
        progreso(format!("el cierre a mercado falló{motivo}; límites rechazados ({estados})"), LogLevel::Error);
        return verdict(
            false,
            format!("The market close of {symbol} failed{motivo} and TWS did not accept the limits ({estados}). Check Orders and TWS."),
            "trading.close.market_failed_limits_rejected",
            params(serde_json::json!({ "reason": motivo, "states": estados })),
        );
    }
    let precios = aceptados
        .iter()
        .map(|(precio, _, _)| claims::texto_precio(*precio, tick))
        .collect::<Vec<_>>()
        .join(" y ");
    let ids = aceptados
        .iter()
        .filter_map(|(_, id, _)| id.map(|id| id.to_string()))
        .collect::<Vec<_>>()
        .join(", ");
    let qty = js_qty(restante);
    if aceptados.len() == 1 {
        progreso(
            format!("el cierre a mercado falló{motivo}; enviado un límite {accion} {qty} a {precios} GTC (orden {ids})"),
            LogLevel::Warning,
        );
        return verdict(
            true,
            format!("El cierre a mercado falló{motivo}; enviado un límite {accion} {qty} {symbol} a {precios} GTC (orden {ids}). Revisa Órdenes."),
            "trading.close.limit_sent",
            params(serde_json::json!({
                "reason": motivo, "action": accion, "quantity": restante, "prices": precios, "orders": ids,
            })),
        );
    }
    progreso(
        format!("el cierre a mercado falló{motivo}; enviados 2 límites OCA {accion} {qty} a {precios} ({ids}), uno cancela al otro"),
        LogLevel::Warning,
    );
    verdict(
        true,
        format!("El cierre a mercado falló{motivo}; enviados 2 límites OCA {accion} {qty} {symbol} a {precios} GTC (órdenes {ids}). Uno cancela al otro. Revisa Órdenes."),
        "trading.close.oca_sent",
        params(serde_json::json!({
            "reason": motivo, "action": accion, "quantity": restante, "prices": precios, "orders": ids,
        })),
    )
}

fn json_obj(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    //! The cycle against a scripted TWS: the frames it sends are the
    //! library's, and each verdict follows the wire the way Openbot's does.

    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use zeron_tws::client::IbClientConfig;
    use zeron_tws::codec::{Decoder, encode_message};

    const ACCOUNT: &str = "DU1";
    const CON_ID: &str = "711712";

    /// One close per contract is process-wide by design: these tests share
    /// the contract, so they take turns.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// How the scripted TWS answers a market order.
    #[derive(Clone, Copy, PartialEq)]
    enum Mercado {
        /// Fills at once: status Filled and the position goes flat.
        Llena,
        /// Works until cancelled; the cancel is confirmed.
        NoLlena,
    }

    #[derive(Default)]
    struct Libro {
        /// Every frame the client sent after `START_API`.
        recibidas: Vec<Vec<String>>,
        /// (orderId, orderRef, status) of the orders TWS holds.
        ordenes: Vec<(i64, String, String)>,
        /// An order on the contract that is not the desk's.
        ajena: bool,
    }

    fn frame(fields: &[&str]) -> Vec<u8> {
        encode_message(fields)
    }

    fn position(size: &str) -> Vec<u8> {
        // Lean contract: no exchange, so the cycle must ask for details.
        frame(&[
            "61", "3", ACCOUNT, CON_ID, "MNQ", "FUT", "20261218", "0", "", "2", "", "USD", "MNQZ6",
            "MNQ", size, "37000",
        ])
    }

    fn open_order(id: i64, order_ref: &str) -> Vec<u8> {
        let id = id.to_string();
        frame(&[
            "5", &id, CON_ID, "MNQ", "FUT", "20261218", "0", "", "2", "CME", "USD", "MNQZ6", "MNQ",
            "SELL", "1", "MKT", "", "", "DAY", "", ACCOUNT, "", "0", order_ref, "1000", "99",
        ])
    }

    fn status(id: i64, status: &str, filled: &str) -> Vec<u8> {
        let id = id.to_string();
        frame(&["3", &id, status, filled, "0", "0", "99", "0", "0", "1000", "", ""])
    }

    /// A TWS that speaks server `version` and answers the close cycle.
    async fn fake_tws(version: &'static str, mercado: Mercado, libro: Arc<Mutex<Libro>>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else { return };
            serve(socket, version, mercado, libro).await;
        });
        port
    }

    async fn serve(mut socket: TcpStream, version: &str, mercado: Mercado, libro: Arc<Mutex<Libro>>) {
        // Handshake: `API\0` + length + `v100..193`.
        let mut head = [0u8; 8];
        if socket.read_exact(&mut head).await.is_err() {
            return;
        }
        let len = u32::from_be_bytes([head[4], head[5], head[6], head[7]]) as usize;
        let mut range = vec![0u8; len];
        let _ = socket.read_exact(&mut range).await;
        let _ = socket.write_all(&frame(&[version, "20261119 10:00:00 CET"])).await;
        let mut decoder = Decoder::new();
        let mut buf = [0u8; 4096];
        let mut opened = false;
        loop {
            let Ok(n) = socket.read(&mut buf).await else { return };
            if n == 0 {
                return;
            }
            decoder.push(&buf[..n]);
            while let Some(fields) = decoder.next_message() {
                let mut out: Vec<Vec<u8>> = Vec::new();
                match fields[0].as_str() {
                    "71" if !opened => {
                        opened = true;
                        out.push(frame(&["15", "1", ACCOUNT]));
                        out.push(frame(&["9", "1", "100"]));
                        out.push(position("1"));
                        out.push(frame(&[
                            "7", "8", CON_ID, "MNQ", "FUT", "20261218", "0", "", "2", "", "USD",
                            "MNQZ6", "MNQ", "1", "18500", "37000", "37000", "0", "0", ACCOUNT,
                        ]));
                    }
                    "9" => {
                        let req = fields[2].clone();
                        out.push(frame(&[
                            "10", &req, "MNQ", "FUT", "20261218 08:30:00 US/Central", "20261218",
                            "0", "", "CME", "USD", "MNQZ6", "MNQ", "MNQ", CON_ID, "0.25", "2",
                            "LMT,MKT", "CME", "1", "0", "Micro E-Mini Nasdaq-100", "", "202612",
                        ]));
                        out.push(frame(&["52", "1", &req]));
                    }
                    "16" => {
                        let libro = libro.lock().unwrap();
                        if libro.ajena {
                            out.push(open_order(7, "stop-manual"));
                            out.push(status(7, "Submitted", "0"));
                        }
                        for (id, order_ref, st) in &libro.ordenes {
                            if !orders::is_terminal(st) {
                                out.push(open_order(*id, order_ref));
                                out.push(status(*id, st, "0"));
                            }
                        }
                        out.push(frame(&["53", "1"]));
                    }
                    "7" => out.push(frame(&["55", "1", &fields[2]])),
                    "3" => {
                        let id: i64 = fields[1].parse().unwrap();
                        let order_type = fields[18].clone();
                        let order_ref = fields[26].clone();
                        let mut libro = libro.lock().unwrap();
                        if order_type == "MKT" && mercado == Mercado::Llena {
                            libro.ordenes.push((id, order_ref, "Filled".into()));
                            out.push(status(id, "Filled", "1"));
                            out.push(position("0"));
                        } else {
                            libro.ordenes.push((id, order_ref, "Submitted".into()));
                            out.push(status(id, "Submitted", "0"));
                        }
                    }
                    "4" => {
                        let id: i64 = fields[1].parse().unwrap();
                        let mut libro = libro.lock().unwrap();
                        for orden in libro.ordenes.iter_mut().filter(|o| o.0 == id) {
                            orden.2 = "Cancelled".into();
                        }
                        out.push(status(id, "Cancelled", "0"));
                    }
                    _ => {}
                }
                if opened {
                    libro.lock().unwrap().recibidas.push(fields);
                }
                for bytes in out {
                    if socket.write_all(&bytes).await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    fn fast() -> Tiempos {
        Tiempos {
            paso: Duration::from_millis(20),
            pasos: 5,
            pausa: Duration::from_millis(20),
            respuesta: Duration::from_secs(2),
        }
    }

    async fn linked(
        version: &'static str,
        mercado: Mercado,
        libro: Arc<Mutex<Libro>>,
    ) -> TradingService {
        let port = fake_tws(version, mercado, libro).await;
        let svc = TradingService::new();
        svc.connect_to(IbClientConfig {
            port,
            client_id: 1000,
            reconnect_interval: Duration::from_millis(50),
            connection_watchdog_interval: Duration::from_secs(5),
            stability_window: Duration::from_millis(40),
            ..IbClientConfig::default()
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let rows = svc.account_positions(ACCOUNT);
                let ready = svc.tws_client().is_some_and(|c| c.is_connected())
                    && rows.first().is_some_and(|row| row.current.is_some());
                if ready {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the fake TWS must deliver the position");
        svc
    }

    fn placed(libro: &Arc<Mutex<Libro>>) -> Vec<Vec<String>> {
        libro
            .lock()
            .unwrap()
            .recibidas
            .iter()
            .filter(|f| f[0] == "3")
            .cloned()
            .collect()
    }

    #[tokio::test]
    async fn a_filled_market_close_sends_the_library_frame_on_the_resolved_contract() {
        let _turn = SERIAL.lock().await;
        let libro = Arc::new(Mutex::new(Libro::default()));
        let svc = linked("193", Mercado::Llena, libro.clone()).await;
        let pos = svc.account_positions(ACCOUNT)[0].clone();
        let v = cerrar_una_con(&pos, ACCOUNT, &svc, &fast()).await;
        assert!(v.ok, "{v:?}");
        assert_eq!(v.message_key.as_deref(), Some("trading.close.executed_market"));

        let orders = placed(&libro);
        assert_eq!(orders.len(), 1, "one market order, no limits");
        let o = &orders[0];
        assert_eq!(o.len(), 118);
        assert_eq!(o[1], "100", "the id TWS handed out");
        assert_eq!(o[2], CON_ID);
        assert_eq!(o[5], "20261218", "expiry trimmed for the order");
        assert_eq!(o[9], "CME", "exchange from the details answer");
        assert_eq!((o[16].as_str(), o[17].as_str(), o[18].as_str()), ("SELL", "1", "MKT"));
        assert_eq!(
            (o[21].as_str(), o[23].as_str(), o[27].as_str(), o[33].as_str()),
            ("DAY", ACCOUNT, "1", "1")
        );
        assert!(o[26].starts_with("escritorio-cierre-711712-"));
        svc.disconnect().await;
    }

    #[tokio::test]
    async fn a_foreign_order_on_the_contract_aborts_with_nothing_sent() {
        let _turn = SERIAL.lock().await;
        let libro = Arc::new(Mutex::new(Libro {
            ajena: true,
            ..Libro::default()
        }));
        let svc = linked("193", Mercado::Llena, libro.clone()).await;
        let pos = svc.account_positions(ACCOUNT)[0].clone();
        let v = cerrar_una_con(&pos, ACCOUNT, &svc, &fast()).await;
        assert!(!v.ok);
        assert_eq!(v.message_key.as_deref(), Some("trading.close.foreign_orders"));
        assert!(placed(&libro).is_empty());
        assert!(
            !libro.lock().unwrap().recibidas.iter().any(|f| f[0] == "4"),
            "nothing foreign cancelled"
        );
        svc.disconnect().await;
    }

    #[tokio::test]
    async fn an_unfilled_market_order_is_cancelled_then_two_oca_limits_go_out_on_the_grid() {
        let _turn = SERIAL.lock().await;
        let libro = Arc::new(Mutex::new(Libro::default()));
        let svc = linked("193", Mercado::NoLlena, libro.clone()).await;
        let pos = svc.account_positions(ACCOUNT)[0].clone();
        let v = cerrar_una_con(&pos, ACCOUNT, &svc, &fast()).await;
        assert!(v.ok, "{v:?}");
        assert_eq!(v.message_key.as_deref(), Some("trading.close.oca_sent"));

        let recibidas = libro.lock().unwrap().recibidas.clone();
        let cancel_at = recibidas
            .iter()
            .position(|f| f[0] == "4")
            .expect("the market order is cancelled");
        assert_eq!(recibidas[cancel_at], vec!["4", "100", "", "", "2147483647"]);
        let orders = placed(&libro);
        assert_eq!(orders.len(), 3);
        let limit_at = recibidas.iter().rposition(|f| f[0] == "3").unwrap();
        assert!(limit_at > cancel_at, "limits only after the cancel");
        // Price 18500, tick 0.25 from the details answer: ±4 ticks.
        let (up, down) = (&orders[1], &orders[2]);
        assert_eq!((up[18].as_str(), up[19].as_str(), up[21].as_str()), ("LMT", "18501", "GTC"));
        assert_eq!(down[19], "18499");
        assert_eq!(up[22], down[22], "one OCA group");
        assert!(up[22].starts_with("cierre-711712-"));
        assert_eq!((up[46].as_str(), down[46].as_str()), ("1", "1"));
        svc.disconnect().await;
    }

    #[tokio::test]
    async fn an_unsupported_server_version_never_encodes_an_order() {
        let _turn = SERIAL.lock().await;
        let libro = Arc::new(Mutex::new(Libro::default()));
        let svc = linked("176", Mercado::Llena, libro.clone()).await;
        let pos = svc.account_positions(ACCOUNT)[0].clone();
        let v = cerrar_una_con(&pos, ACCOUNT, &svc, &fast()).await;
        assert!(!v.ok);
        assert_eq!(v.message_key.as_deref(), Some("trading.close.wire_unsupported"));
        assert!(placed(&libro).is_empty());
        svc.disconnect().await;
    }
}
