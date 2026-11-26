//! The MT5 translator: a bot's `ib_async` calls, on a MetaTrader 5 account.
//!
//! A direct port of `bots-mt5.ts` (`crearTraductor`). Bots are written against
//! IBKR; when one trades an MT5 link the engine sends its orders here, and the
//! translator turns them into bridge commands:
//!
//! | bot order                        | on MT5                                     |
//! |----------------------------------|--------------------------------------------|
//! | market, opening                  | `DEAL` with the bot's magic number         |
//! | market, against its position     | `CLOSEVOL` on its positions (oldest first) |
//! | stop / limit that opens or adds  | a pending order on the broker (`PENDING`)  |
//! | stop against its position        | the stop loss of each of its positions     |
//! | limit against its position       | the take profit of each of its positions   |
//!
//! So protection lives on the broker and keeps working while the app or the
//! bot is closed, and sizes keep the same money per point ([`super::lots`]).
//!
//! Everything that trades is serialised on one mutex: placing, cancelling and
//! reconciling never interleave, and [`Traductor::tick`] skips a turn while an
//! action holds it (the TS `enExclusiva`/`ocupado` pair).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::instruments::{
    con_id_de, duracion_segundos, es_forex, futuro, magic_por_defecto, marco, Instrumento, SUFIJOS,
};
use super::libro::{BarraMt5, LibroMt5, OperacionMt5, PosicionMt5, ResultadoMt5};
use super::lots::{lotes, lotes_por_unidad, precio_de, redondear, unidades};

/// How long a bot order is kept after it ends.
pub const RETENCION_MS: i64 = 14 * 86_400_000;
/// A DAY market order that could not execute gives up after this long.
const DIA_MS: i64 = 12 * 3_600_000;
/// Grace before a missing broker order or position is believed.
const GRACIA_MS: i64 = 6_000;
/// Broker answers that mean "not now" (market closed, no prices, busy): wait.
const REINTENTO: [i64; 8] = [-1, 10004, 10018, 10020, 10021, 10024, 10031, 10012];
/// The price is already beyond the stop or limit: it would trigger at once.
const CRUZADO: [i64; 2] = [10015, 10016];
/// MT5's `SYMBOL_TRADE_MODE_DISABLED`: the broker shows the symbol but this
/// account can't trade it.
const SYMBOL_TRADE_MODE_DISABLED: i64 = 0;

/// IB statuses of an order that can still execute.
pub fn activa(orden: &OrdenVirtual) -> bool {
    matches!(
        orden.status.as_str(),
        "PendingSubmit" | "PreSubmitted" | "Submitted"
    )
}

fn estado_de(orden: &mut OrdenVirtual, status: &str, mensaje: &str, ahora: i64) {
    orden.status = status.to_string();
    orden.message = mensaje.to_string();
    orden.updated = ahora;
}

/// A JS-style number for a bridge argument: `1`, `0.1`, no trailing zeros.
fn texto(valor: f64) -> String {
    if valor == valor.trunc() && valor.abs() < 1e15 {
        format!("{}", valor as i64)
    } else {
        format!("{valor}")
    }
}

/// `Number(x.toPrecision(6))`: six significant digits, then the shortest form.
fn seis(valor: f64) -> String {
    if !valor.is_finite() || valor == 0.0 {
        return texto(valor);
    }
    let redondeado: f64 = format!("{valor:.5e}").parse().unwrap_or(valor);
    texto(redondeado)
}

/// A bot's order as `ib_async` sees it, plus how it lives on MT5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrdenVirtual {
    pub order_id: i64,
    pub perm_id: i64,
    pub system: String,
    pub client_id: i64,
    pub con_id: i64,
    pub symbol: String,
    /// `BUY` or `SELL`.
    pub action: String,
    /// `MKT`, `STP` or `LMT`.
    pub order_type: String,
    pub qty: f64,
    pub aux_price: f64,
    pub lmt_price: f64,
    pub tif: String,
    pub oca_group: String,
    /// `market`, `entry`, `exit_sl` or `exit_tp`.
    pub role: String,
    pub status: String,
    pub filled: f64,
    pub avg_fill_price: f64,
    /// The broker's pending order (entry stops and limits).
    pub ticket: i64,
    /// Broker deals already counted for this order.
    pub deals: Vec<i64>,
    pub message: String,
    /// The MT5 link the order lives on.
    pub account: String,
    pub created: i64,
    pub updated: i64,
    /// Earliest next try of a waiting market order or entry.
    pub next_try: i64,
}

/// One execution, as `reqExecutions` / `fills` report it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ejecucion {
    pub exec_id: String,
    pub order_id: i64,
    pub perm_id: i64,
    pub system: String,
    pub client_id: i64,
    pub con_id: i64,
    pub symbol: String,
    /// `BOT` or `SLD`.
    pub side: String,
    pub shares: f64,
    pub price: f64,
    pub time: i64,
    pub account: String,
}

/// What a bot asked `qualifyContracts` for.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeticionContrato {
    #[serde(default)]
    pub sec_type: Option<String>,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub exchange: Option<String>,
    #[serde(default)]
    pub con_id: Option<i64>,
}

/// What `placeOrder` sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeticionOrden {
    #[serde(default)]
    pub order_id: Option<i64>,
    #[serde(default)]
    pub client_id: Option<i64>,
    pub con_id: i64,
    pub action: String,
    pub order_type: String,
    pub total_quantity: f64,
    #[serde(default)]
    pub aux_price: Option<f64>,
    #[serde(default)]
    pub lmt_price: Option<f64>,
    #[serde(default)]
    pub tif: Option<String>,
    #[serde(default)]
    pub oca_group: Option<String>,
}

/// The contract a bot sees after `qualifyContracts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContratoJson {
    pub con_id: i64,
    pub sec_type: String,
    pub symbol: String,
    pub currency: String,
    pub exchange: String,
    pub local_symbol: String,
    pub trading_class: String,
    /// Empty for forex, like `ib_async` prints it.
    pub multiplier: String,
    pub last_trade_date_or_contract_month: String,
    pub mt5: String,
    pub lots_per_unit: f64,
}

/// One position as `positions()` reports it: the bot's own units.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PosicionBot {
    pub account: String,
    pub contract: ContratoJson,
    pub position: f64,
    pub avg_cost: f64,
}

/// One `accountSummary` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumenCuenta {
    pub account: String,
    pub tag: String,
    pub value: String,
    pub currency: String,
    pub model_code: String,
}

/// The `hello` a bot gets when it connects to the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub system: String,
    pub accounts: Vec<String>,
    pub account: String,
    pub magic: i64,
    pub broker: String,
    pub server_time: i64,
}

/// How a panic ended the working orders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cancelacion {
    pub canceladas: usize,
    pub fallos: Vec<String>,
}

/// Log level the translator reports at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nivel {
    Info,
    Warning,
    Error,
}

/// A translator refusal: bad order, no link, unknown instrument.
#[derive(Debug, Clone, PartialEq)]
pub struct ErrorTraductor(pub String);

impl ErrorTraductor {
    pub fn new(mensaje: impl Into<String>) -> Self {
        Self(mensaje.into())
    }
}

impl fmt::Display for ErrorTraductor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ErrorTraductor {}

pub type Resultado<T> = std::result::Result<T, ErrorTraductor>;

/// What the translator needs from the engine.
#[async_trait]
pub trait EntornoMt5: Send + Sync {
    /// The MT5 link a system trades on, or the reason it has none.
    fn enlace_de(&self, sistema: &str) -> std::result::Result<String, String>;
    /// The link's last book; `None` while its bridge reads nothing.
    fn libro(&self, enlace: &str) -> Option<LibroMt5>;
    async fn comando(&self, enlace: &str, verbo: &str, arg: &str) -> ResultadoMt5;
    fn magic(&self, sistema: &str) -> i64 {
        magic_por_defecto(sistema)
    }
    /// The system's own symbol choice (`system.json` → `"symbol"`).
    fn simbolo(&self, _sistema: &str) -> Option<String> {
        None
    }
    fn log(&self, _nivel: Nivel, _sistema: &str, _mensaje: &str) {}
    fn guardar_orden(&self, _orden: &OrdenVirtual) {}
    fn guardar_ejecucion(&self, _ejecucion: &Ejecucion) {}
    fn guardar_instrumentos(&self, _lista: &[Instrumento]) {}
}

/// What the translator starts from (the persisted rows).
#[derive(Debug, Clone, Default)]
pub struct EstadoInicial {
    pub ordenes: Vec<OrdenVirtual>,
    pub ejecuciones: Vec<Ejecucion>,
    pub instrumentos: Vec<Instrumento>,
}

struct Estado {
    ordenes: BTreeMap<i64, OrdenVirtual>,
    ejecuciones: Vec<Ejecucion>,
    instrumentos: BTreeMap<i64, Instrumento>,
    siguiente: i64,
    /// Last SL/TP request per position ticket, so a refused one is not resent
    /// every second.
    sltp_enviado: HashMap<i64, i64>,
}

/// The translator: one per engine.
pub struct Traductor {
    entorno: Arc<dyn EntornoMt5>,
    estado: Mutex<Estado>,
    reloj: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl Traductor {
    pub fn new(entorno: Arc<dyn EntornoMt5>, inicial: EstadoInicial) -> Self {
        Self::with_clock(entorno, inicial, Arc::new(ahora_ms))
    }

    /// Same translator on an explicit clock, so tests can move time.
    pub fn with_clock(
        entorno: Arc<dyn EntornoMt5>,
        inicial: EstadoInicial,
        reloj: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        let ahora = reloj();
        let desde = ahora - RETENCION_MS;
        let mut ordenes = BTreeMap::new();
        for orden in inicial.ordenes {
            if activa(&orden) || orden.updated >= desde {
                ordenes.insert(orden.order_id, orden);
            }
        }
        let maximo = ordenes.keys().copied().max().unwrap_or(999);
        let estado = Estado {
            ordenes,
            ejecuciones: inicial
                .ejecuciones
                .into_iter()
                .filter(|f| f.time >= desde)
                .collect(),
            instrumentos: inicial
                .instrumentos
                .into_iter()
                .map(|i| (i.con_id, i))
                .collect(),
            siguiente: maximo.max(999) + 1,
            sltp_enviado: HashMap::new(),
        };
        Self {
            entorno,
            estado: Mutex::new(estado),
            reloj,
        }
    }

    fn ahora(&self) -> i64 {
        (self.reloj)()
    }

    fn entorno(&self) -> &dyn EntornoMt5 {
        self.entorno.as_ref()
    }

    /// The order as it stands, for the bot API and the tournament's own reads.
    pub async fn orden(&self, order_id: i64) -> Option<OrdenVirtual> {
        self.estado.lock().await.ordenes.get(&order_id).cloned()
    }

    /// Every order the translator keeps (active and recent).
    pub async fn ordenes(&self) -> Vec<OrdenVirtual> {
        self.estado.lock().await.ordenes.values().cloned().collect()
    }

    /// Every execution it kept.
    pub async fn ejecuciones(&self) -> Vec<Ejecucion> {
        self.estado.lock().await.ejecuciones.clone()
    }

    fn nombre_cuenta(&self, enlace: &str) -> String {
        match self.entorno().libro(enlace) {
            Some(libro) => format!(
                "{}{}",
                if libro.demo { "DU" } else { "U" },
                libro.login
            ),
            None => enlace.to_string(),
        }
    }

    fn enlace_o_error(&self, sistema: &str) -> Resultado<String> {
        let enlace = self
            .entorno()
            .enlace_de(sistema)
            .map_err(ErrorTraductor::new)?;
        if self.entorno().libro(&enlace).is_none() {
            return Err(ErrorTraductor::new(format!(
                "la cuenta {enlace} no tiene el terminal de MetaTrader 5 conectado"
            )));
        }
        Ok(enlace)
    }

    fn posiciones_bot(&self, enlace: &str, magic: i64, simbolo: &str) -> Vec<PosicionMt5> {
        let mut posiciones: Vec<PosicionMt5> = self
            .entorno()
            .libro(enlace)
            .map(|libro| libro.positions)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.magic == magic && p.symbol == simbolo)
            .collect();
        posiciones.sort_by_key(|p| p.time);
        posiciones
    }

    fn operaciones(&self, enlace: &str) -> Vec<OperacionMt5> {
        self.entorno()
            .libro(enlace)
            .and_then(|libro| libro.deals)
            .unwrap_or_default()
    }

    fn guardar(&self, estado: &mut Estado, orden: &OrdenVirtual) {
        estado.ordenes.insert(orden.order_id, orden.clone());
        self.entorno().guardar_orden(orden);
    }

    fn instrumento(&self, estado: &Estado, con_id: i64) -> Resultado<Instrumento> {
        estado
            .instrumentos
            .get(&con_id)
            .cloned()
            .ok_or_else(|| ErrorTraductor::new(format!("conId {con_id} sin cualificar")))
    }

    /// `placeOrder`: a new order, or a change to one still working.
    pub async fn place(
        &self,
        sistema: &str,
        peticion: PeticionOrden,
    ) -> Resultado<OrdenVirtual> {
        let ahora = self.ahora();
        let mut estado = self.estado.lock().await;
        let i = self.instrumento(&estado, peticion.con_id)?;
        let enlace = self.enlace_o_error(sistema)?;
        let accion = peticion.action.to_uppercase();
        if accion != "BUY" && accion != "SELL" {
            return Err(ErrorTraductor::new(format!(
                "acción desconocida: {}",
                peticion.action
            )));
        }
        let tipo = peticion.order_type.to_uppercase();
        if tipo != "MKT" && tipo != "STP" && tipo != "LMT" {
            return Err(ErrorTraductor::new(format!(
                "tipo de orden no soportado con MT5: {}",
                peticion.order_type
            )));
        }
        let cantidad = peticion.total_quantity;
        if !(cantidad > 0.0) {
            return Err(ErrorTraductor::new("la cantidad debe ser mayor que 0"));
        }
        // A stop or limit at 0 or below is a mistake: MT5 refuses it as an
        // invalid price, which must not read as "already crossed" (market).
        let precio_pedido = match tipo.as_str() {
            "STP" => peticion.aux_price.unwrap_or(0.0),
            "LMT" => peticion.lmt_price.unwrap_or(0.0),
            _ => 1.0,
        };
        if !(precio_pedido > 0.0) {
            return Err(ErrorTraductor::new(format!(
                "precio no válido para una orden {tipo}: {precio_pedido}"
            )));
        }
        // MT5 trades whole volume steps: say so when the bot's size doesn't fit.
        let enviadas = unidades(&i, lotes(&i, cantidad));
        if (enviadas - cantidad).abs() > cantidad * 0.005 {
            self.entorno().log(
                Nivel::Warning,
                sistema,
                &format!(
                    "{} unidades de {} no caben en lotes de {}: se envían {} lotes = {} unidades",
                    cantidad,
                    i.mt5,
                    if i.volume_step > 0.0 { i.volume_step } else { 0.01 },
                    lotes(&i, cantidad),
                    enviadas
                ),
            );
        }
        let magic = self.entorno().magic(sistema);
        let aux = precio_de(&i, peticion.aux_price.unwrap_or(0.0));
        let lmt = precio_de(&i, peticion.lmt_price.unwrap_or(0.0));
        let pedido = peticion.order_id.unwrap_or(0);

        // A change to an order that is still working.
        if pedido > 0 {
            let existente = estado.ordenes.get(&pedido).cloned();
            if let Some(existente) = existente {
                if existente.system == sistema && activa(&existente) {
                    let mut o = existente;
                    let cambia_precio =
                        (o.aux_price - aux).abs() > 1e-9 || (o.lmt_price - lmt).abs() > 1e-9;
                    let cambia_cantidad = (o.qty - cantidad).abs() > 1e-9;
                    o.qty = cantidad;
                    o.aux_price = aux;
                    o.lmt_price = lmt;
                    o.oca_group = peticion.oca_group.clone().unwrap_or_default();
                    o.updated = ahora;
                    if o.role == "entry" && o.ticket > 0 && (cambia_precio || cambia_cantidad) {
                        let r = self
                            .entorno()
                            .comando(&o.account, "DELETE", &o.ticket.to_string())
                            .await;
                        if r.ok {
                            o.ticket = 0;
                            self.poner_entrada(&mut estado, &mut o, &i, magic, ahora)
                                .await;
                        }
                    } else if o.role == "exit_sl" || o.role == "exit_tp" {
                        self.guardar(&mut estado, &o);
                        let enlace = o.account.clone();
                        let con_id = o.con_id;
                        self.proteger(&mut estado, &enlace, sistema, con_id, magic, ahora)
                            .await;
                        return Ok(estado.ordenes.get(&o.order_id).cloned().unwrap_or(o));
                    }
                    self.guardar(&mut estado, &o);
                    return Ok(o);
                }
            }
        }

        let id = if pedido > 0 && !estado.ordenes.contains_key(&pedido) {
            pedido.max(estado.siguiente)
        } else {
            estado.siguiente
        };
        estado.siguiente = id + 1;
        let mut o = OrdenVirtual {
            order_id: id,
            perm_id: id,
            system: sistema.to_string(),
            client_id: peticion.client_id.unwrap_or(0),
            con_id: i.con_id,
            symbol: i.mt5.clone(),
            action: accion.clone(),
            order_type: tipo.clone(),
            qty: cantidad,
            aux_price: aux,
            lmt_price: lmt,
            tif: match peticion.tif.as_deref() {
                Some(tif) if !tif.is_empty() => tif.to_uppercase(),
                _ => "DAY".to_string(),
            },
            oca_group: peticion.oca_group.clone().unwrap_or_default(),
            role: "entry".to_string(),
            status: "PendingSubmit".to_string(),
            filled: 0.0,
            avg_fill_price: 0.0,
            ticket: 0,
            deals: Vec::new(),
            message: String::new(),
            account: enlace.clone(),
            created: ahora,
            updated: ahora,
            next_try: 0,
        };
        let neto = self.neto_de(&estado, &o, magic);
        let contra = (accion == "BUY" && neto < -1e-9) || (accion == "SELL" && neto > 1e-9);
        o.role = if tipo == "MKT" {
            "market".to_string()
        } else if contra {
            if tipo == "STP" {
                "exit_sl".to_string()
            } else {
                "exit_tp".to_string()
            }
        } else {
            "entry".to_string()
        };
        self.guardar(&mut estado, &o);
        let como = match o.role.as_str() {
            "market" => "a mercado".to_string(),
            "entry" => format!(
                "{} pendiente en {}",
                if tipo == "STP" { "stop" } else { "límite" },
                texto(if tipo == "STP" { o.aux_price } else { o.lmt_price })
            ),
            "exit_sl" => format!("stop de salida en {}", texto(o.aux_price)),
            _ => format!("objetivo en {}", texto(o.lmt_price)),
        };
        self.entorno().log(
            Nivel::Info,
            sistema,
            &format!(
                "Orden {} {} {} {} {} ({} lotes){}",
                o.order_id,
                o.action,
                texto(o.qty),
                o.symbol,
                como,
                texto(lotes(&i, o.qty)),
                if o.oca_group.is_empty() {
                    String::new()
                } else {
                    format!(" · OCA {}", o.oca_group)
                }
            ),
        );
        match o.role.as_str() {
            "market" => {
                self.ejecutar_mercado(&mut estado, &mut o, &i, magic, ahora)
                    .await;
            }
            "entry" => {
                self.poner_entrada(&mut estado, &mut o, &i, magic, ahora)
                    .await;
            }
            _ => {
                estado_de(&mut o, "Submitted", "", ahora);
                self.guardar(&mut estado, &o);
                let enlace = o.account.clone();
                let con_id = o.con_id;
                self.proteger(&mut estado, &enlace, sistema, con_id, magic, ahora)
                    .await;
                return Ok(estado.ordenes.get(&o.order_id).cloned().unwrap_or(o));
            }
        }
        self.guardar(&mut estado, &o);
        Ok(o)
    }

    /// `cancelOrder`.
    pub async fn cancel(&self, sistema: &str, order_id: i64) -> Resultado<OrdenVirtual> {
        let ahora = self.ahora();
        let mut estado = self.estado.lock().await;
        let previa = estado
            .ordenes
            .get(&order_id)
            .cloned()
            .ok_or_else(|| ErrorTraductor::new("orden desconocida"))?;
        if previa.system != sistema {
            return Err(ErrorTraductor::new("orden desconocida"));
        }
        let mut o = previa;
        if !activa(&o) {
            return Ok(o);
        }
        let magic = self.entorno().magic(sistema);
        if o.role == "entry" && o.ticket > 0 {
            let r = self
                .entorno()
                .comando(&o.account, "DELETE", &o.ticket.to_string())
                .await;
            // Gone already: it may have filled; the next tick books it.
            if r.ok {
                estado_de(&mut o, "Cancelled", "", ahora);
            } else {
                o.message = r.message;
            }
        } else if o.role == "exit_sl" || o.role == "exit_tp" {
            estado_de(&mut o, "Cancelled", "", ahora);
            self.guardar(&mut estado, &o);
            self.quitar_proteccion(&mut estado, &o, magic, ahora)
                .await;
        } else {
            estado_de(&mut o, "Cancelled", "", ahora);
        }
        self.guardar(&mut estado, &o);
        if o.status == "Cancelled" {
            self.entorno().log(
                Nivel::Info,
                sistema,
                &format!("Orden {} cancelada", o.order_id),
            );
        }
        Ok(o)
    }

    /// Every second: entries the broker filled, exits it executed, protection
    /// on new positions, and market orders waiting for the market to open.
    /// Skipped while another action holds the translator.
    pub async fn tick(&self) {
        let Ok(mut estado) = self.estado.try_lock() else {
            return;
        };
        self.tick_con(&mut estado).await;
    }

    async fn tick_con(&self, estado: &mut Estado) {
        let ahora = self.ahora();
        let vivas: Vec<OrdenVirtual> = estado
            .ordenes
            .values()
            .filter(|orden| activa(orden))
            .cloned()
            .collect();
        if vivas.is_empty() {
            podar(estado, ahora);
            return;
        }
        let mut grupos: BTreeMap<String, (String, String, i64)> = BTreeMap::new();
        for mut o in vivas {
            // Terminal offline: nothing can be known or done now.
            if self.entorno().libro(&o.account).is_none() {
                continue;
            }
            let Some(i) = estado.instrumentos.get(&o.con_id).cloned() else {
                continue;
            };
            let magic = self.entorno().magic(&o.system);
            match o.role.as_str() {
                "market" => {
                    if o.tif == "DAY" && ahora - o.created > DIA_MS {
                        estado_de(&mut o, "Cancelled", "no se pudo ejecutar en el día", ahora);
                        self.guardar(estado, &o);
                    } else if ahora >= o.next_try {
                        self.ejecutar_mercado(estado, &mut o, &i, magic, ahora)
                            .await;
                        self.guardar(estado, &o);
                    }
                }
                "entry" if o.ticket == 0 => {
                    if ahora >= o.next_try {
                        self.poner_entrada(estado, &mut o, &i, magic, ahora).await;
                        self.guardar(estado, &o);
                    }
                }
                "entry" => {
                    let ds: Vec<OperacionMt5> = self
                        .operaciones(&o.account)
                        .into_iter()
                        .filter(|d| d.order == o.ticket && !o.deals.contains(&d.deal))
                        .collect();
                    for d in &ds {
                        let units = unidades(&i, d.volume)
                            .min(o.qty - o.filled)
                            .max(0.0);
                        self.sumar_ejecucion(estado, &mut o, d.deal, units, d.price, d.time);
                    }
                    let pendiente = self
                        .entorno()
                        .libro(&o.account)
                        .map(|libro| libro.orders.iter().any(|p| p.ticket == o.ticket))
                        .unwrap_or(false);
                    if !pendiente && o.filled > 0.0 {
                        estado_de(&mut o, "Filled", "", ahora);
                        self.entorno().log(
                            Nivel::Info,
                            &o.system,
                            &format!(
                                "Orden {} ejecutada por MT5 a {}",
                                o.order_id, o.avg_fill_price
                            ),
                        );
                        self.guardar(estado, &o);
                    } else if !pendiente && ahora - o.updated > GRACIA_MS {
                        estado_de(
                            &mut o,
                            "Cancelled",
                            "la orden ya no está en MT5 (caducada o borrada)",
                            ahora,
                        );
                        self.entorno().log(
                            Nivel::Warning,
                            &o.system,
                            &format!("Orden {}: {}", o.order_id, o.message),
                        );
                        self.guardar(estado, &o);
                    } else if !ds.is_empty() {
                        self.guardar(estado, &o);
                    }
                }
                _ => {
                    grupos.insert(
                        format!("{}|{}|{}", o.account, o.system, o.con_id),
                        (o.account.clone(), o.system.clone(), o.con_id),
                    );
                }
            }
        }
        for (_, (enlace, sistema, con_id)) in grupos {
            let magic = self.entorno().magic(&sistema);
            self.proteger(estado, &enlace, &sistema, con_id, magic, ahora)
                .await;
        }
    }

    /// Cancels a stopped system's waiting market orders: nobody is left to
    /// follow them.
    pub async fn sistema_parado(&self, sistema: &str) {
        let ahora = self.ahora();
        let mut estado = self.estado.lock().await;
        let vivas: Vec<OrdenVirtual> = estado.ordenes.values().cloned().collect();
        for mut o in vivas {
            if o.system != sistema || !activa(&o) || o.role != "market" {
                continue;
            }
            estado_de(&mut o, "Cancelled", "el sistema se paró", ahora);
            self.guardar(&mut estado, &o);
        }
    }

    /// Panic: every working bot order ends — pending entries leave the broker —
    /// so nothing reopens after the positions close.
    pub async fn cancelar_todo(&self) -> Cancelacion {
        let ahora = self.ahora();
        let mut estado = self.estado.lock().await;
        let mut canceladas = 0;
        let mut fallos = Vec::new();
        let vivas: Vec<OrdenVirtual> = estado
            .ordenes
            .values()
            .filter(|orden| activa(orden))
            .cloned()
            .collect();
        for mut o in vivas {
            if o.role == "entry" && o.ticket > 0 {
                let r = self
                    .entorno()
                    .comando(&o.account, "DELETE", &o.ticket.to_string())
                    .await;
                if !r.ok && r.retcode != 10036 {
                    fallos.push(format!("{}: {}", o.order_id, r.message));
                    continue;
                }
            }
            estado_de(&mut o, "Cancelled", "pánico", ahora);
            self.guardar(&mut estado, &o);
            canceladas += 1;
        }
        Cancelacion {
            canceladas,
            fallos,
        }
    }

    /// `qualifyContracts`: resolves what a bot asked for on its MT5 account.
    pub async fn qualify(
        &self,
        sistema: &str,
        peticion: PeticionContrato,
    ) -> Resultado<ContratoJson> {
        let enlace = self.enlace_o_error(sistema)?;
        let con_id = peticion.con_id.unwrap_or(0);
        let simbolo = peticion.symbol.clone().unwrap_or_default();
        if con_id != 0 && simbolo.is_empty() {
            let conocido = {
                let estado = self.estado.lock().await;
                estado
                    .instrumentos
                    .get(&con_id)
                    .cloned()
                    .ok_or_else(|| ErrorTraductor::new(format!("conId {con_id} desconocido")))?
            };
            let i = if conocido.account == enlace {
                conocido
            } else {
                self.resolver(
                    sistema,
                    &enlace,
                    &conocido.root,
                    &conocido.sec_type,
                    &conocido.exchange,
                    &conocido.currency,
                )
                .await?
            };
            return Ok(contrato_json(&i, peticion.sec_type.as_deref().unwrap_or("")));
        }
        let sec = peticion.sec_type.clone().unwrap_or_default().to_uppercase();
        let sym = simbolo.to_uppercase();
        let moneda = peticion.currency.clone().unwrap_or_default();
        let root = if sec == "CASH" || sec == "CFD" {
            if sym.len() == 3 && moneda.len() == 3 {
                format!("{sym}{}", moneda.to_uppercase())
            } else {
                sym.replace('.', "")
            }
        } else {
            sym
        };
        if root.is_empty() {
            return Err(ErrorTraductor::new("contrato sin símbolo"));
        }
        let i = self
            .resolver(
                sistema,
                &enlace,
                &root,
                &sec,
                peticion.exchange.as_deref().unwrap_or(""),
                &moneda,
            )
            .await?;
        Ok(contrato_json(&i, peticion.sec_type.as_deref().unwrap_or("")))
    }

    /// `reqHistoricalData`: the MT5 bars of the instrument's symbol.
    pub async fn bars(
        &self,
        sistema: &str,
        con_id: i64,
        fin_segundos: Option<i64>,
        duracion: &str,
        tamano: &str,
    ) -> Resultado<Vec<BarraMt5>> {
        let i = {
            let estado = self.estado.lock().await;
            self.instrumento(&estado, con_id)?
        };
        let enlace = self.enlace_o_error(sistema)?;
        let tf = marco(tamano).ok_or_else(|| {
            ErrorTraductor::new(format!("tamaño de vela no soportado con MT5: {tamano}"))
        })?;
        let hasta = fin_segundos.unwrap_or_else(|| self.ahora() / 1000);
        let lapso = duracion_segundos(duracion)
            .ok_or_else(|| ErrorTraductor::new(format!("duración no soportada: {duracion}")))?;
        let r = self
            .entorno()
            .comando(
                &enlace,
                "RATES",
                &format!("{};{};{};{}", i.mt5, tf, hasta - lapso, hasta),
            )
            .await;
        if !r.ok {
            // No bars in that window (before the history starts) reads as an
            // empty answer, like TWS.
            if let Some(serde_json::Value::Array(filas)) = &r.data {
                if filas.is_empty() && r.retcode != -1 && r.retcode != -2 {
                    return Ok(Vec::new());
                }
            }
            return Err(ErrorTraductor::new(r.message));
        }
        serde_json::from_value(r.data.unwrap_or_else(|| serde_json::Value::Array(Vec::new())))
            .map_err(|error| ErrorTraductor::new(format!("RATES ilegible: {error}")))
    }

    /// The bot's open positions per instrument it qualified, in its own units.
    pub async fn positions(&self, sistema: &str) -> Vec<PosicionBot> {
        let Ok(enlace) = self.entorno().enlace_de(sistema) else {
            return Vec::new();
        };
        if self.entorno().libro(&enlace).is_none() {
            return Vec::new();
        }
        let estado = self.estado.lock().await;
        let magic = self.entorno().magic(sistema);
        let mut vistos = HashSet::new();
        let mut salida = Vec::new();
        for i in estado.instrumentos.values().filter(|i| i.account == enlace) {
            if !vistos.insert(i.mt5.clone()) {
                continue;
            }
            let ps = self.posiciones_bot(&enlace, magic, &i.mt5);
            if ps.is_empty() {
                continue;
            }
            let lots = neto(&ps);
            let volumen: f64 = ps.iter().map(|p| p.volume).sum();
            let media = if volumen > 0.0 {
                ps.iter().map(|p| p.entry * p.volume).sum::<f64>() / volumen
            } else {
                0.0
            };
            let signo = if lots > 0.0 {
                1.0
            } else if lots < 0.0 {
                -1.0
            } else {
                0.0
            };
            salida.push(PosicionBot {
                account: self.nombre_cuenta(&enlace),
                contract: contrato_json(i, ""),
                position: unidades(i, lots.abs()) * signo,
                avg_cost: media * if i.forex { 1.0 } else { i.multiplier },
            });
        }
        salida
    }

    /// `accountSummary` tags from the MT5 account.
    pub async fn account_summary(&self, sistema: &str) -> Vec<ResumenCuenta> {
        let Ok(enlace) = self.entorno().enlace_de(sistema) else {
            return Vec::new();
        };
        let Some(libro) = self.entorno().libro(&enlace) else {
            return Vec::new();
        };
        let nombre = self.nombre_cuenta(&enlace);
        let filas: Vec<(&str, Option<f64>)> = vec![
            ("NetLiquidation", libro.equity),
            ("TotalCashValue", libro.balance),
            ("EquityWithLoanValue", libro.equity),
            ("AvailableFunds", libro.margin_free),
            ("ExcessLiquidity", libro.margin_free),
            ("FullInitMarginReq", libro.margin),
            ("InitMarginReq", libro.margin),
            ("MaintMarginReq", libro.margin),
            ("UnrealizedPnL", libro.profit),
            (
                "BuyingPower",
                match (libro.margin_free, libro.leverage) {
                    (Some(libre), leverage) if leverage != 0 => Some(libre * leverage as f64),
                    _ => None,
                },
            ),
        ];
        let mut salida: Vec<ResumenCuenta> = filas
            .into_iter()
            .filter_map(|(tag, valor)| {
                valor.map(|valor| ResumenCuenta {
                    account: nombre.clone(),
                    tag: tag.to_string(),
                    value: format!("{valor:.2}"),
                    currency: libro.currency.clone(),
                    model_code: String::new(),
                })
            })
            .collect();
        salida.push(ResumenCuenta {
            account: nombre,
            tag: "AccountType".to_string(),
            value: if libro.demo { "PAPER" } else { "INDIVIDUAL" }.to_string(),
            currency: String::new(),
            model_code: String::new(),
        });
        salida
    }

    /// The `hello` a bot gets when it connects to the engine.
    pub async fn hello(&self, sistema: &str) -> Resultado<Hello> {
        let enlace = self.enlace_o_error(sistema)?;
        let nombre = self.nombre_cuenta(&enlace);
        let magic = self.entorno().magic(sistema);
        self.entorno().log(
            Nivel::Info,
            sistema,
            &format!(
                "Bot conectado al motor: opera en {nombre} ({enlace}) con magic {magic}"
            ),
        );
        Ok(Hello {
            system: sistema.to_string(),
            accounts: vec![nombre],
            account: enlace,
            magic,
            broker: "mt5".to_string(),
            server_time: self.ahora(),
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn resolver(
        &self,
        sistema: &str,
        enlace: &str,
        root: &str,
        sec: &str,
        exchange: &str,
        currency: &str,
    ) -> Resultado<Instrumento> {
        let fut = futuro(root);
        let forex = fut.is_none() && es_forex(root);
        let mut nombres: Vec<String> = Vec::new();
        // The system's own choice (system.json "symbol") wins over the list.
        if let Some(elegido) = self.entorno().simbolo(sistema) {
            nombres.push(elegido);
        }
        let bases: Vec<&str> = match fut {
            Some((_, lista)) => lista.to_vec(),
            None => vec![root],
        };
        for sufijo in SUFIJOS {
            for base in &bases {
                nombres.push(format!("{base}{sufijo}"));
            }
        }
        // The first symbol the broker lets this account trade; failing that,
        // the first it has at all (its bars still work), with a warning.
        let mut spec: Option<serde_json::Value> = None;
        let mut bloqueado: Option<serde_json::Value> = None;
        for nombre in &nombres {
            let r = self.entorno().comando(enlace, "SYMBOL", nombre).await;
            if r.ok && r.data.as_ref().is_some_and(|d| d.is_object()) {
                let deshabilitado = r
                    .data
                    .as_ref()
                    .and_then(|d| d.get("trade_mode"))
                    .and_then(serde_json::Value::as_i64)
                    == Some(SYMBOL_TRADE_MODE_DISABLED);
                if !deshabilitado {
                    spec = r.data;
                    break;
                }
                if bloqueado.is_none() {
                    bloqueado = r.data;
                }
                continue;
            }
            if r.retcode == -1 || r.retcode == -2 {
                return Err(ErrorTraductor::new(r.message));
            }
        }
        if spec.is_none() {
            if let Some(datos) = bloqueado {
                let nombre = datos.get("symbol").and_then(serde_json::Value::as_str).unwrap_or(root);
                self.entorno().log(
                    Nivel::Warning,
                    sistema,
                    &format!(
                        "{nombre} no se puede operar en esta cuenta: el bróker lo tiene deshabilitado. El sistema leerá velas pero sus órdenes serán rechazadas; usa una cuenta que permita {root} o pon otro símbolo en \"symbol\" del system.json."
                    ),
                );
                spec = Some(datos);
            }
        }
        let spec = spec.ok_or_else(|| {
            ErrorTraductor::new(format!(
                "MT5 no tiene ningún símbolo para {root} (probados {}). Pon el nombre exacto en \"symbol\" del system.json del sistema.",
                nombres.iter().take(8).cloned().collect::<Vec<_>>().join(", ")
            ))
        })?;
        let numero = |clave: &str| {
            spec.get(clave)
                .and_then(serde_json::Value::as_f64)
                .filter(|valor| valor.is_finite())
        };
        let i = Instrumento {
            con_id: con_id_de(root),
            root: root.to_string(),
            sec_type: if sec.is_empty() {
                if forex { "CASH" } else { "FUT" }.to_string()
            } else {
                sec.to_string()
            },
            exchange: exchange.to_string(),
            currency: if currency.is_empty() {
                spec.get("currency_profit")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("USD")
                    .to_string()
            } else {
                currency.to_string()
            },
            multiplier: fut.map(|(multiplier, _)| multiplier).unwrap_or(1.0),
            forex,
            account: enlace.to_string(),
            mt5: spec
                .get("symbol")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| root.to_string()),
            digits: numero("digits").unwrap_or(5.0) as i32,
            contract_size: numero("contract_size").unwrap_or(0.0),
            tick_size: numero("tick_size").unwrap_or(0.0),
            tick_value: numero("tick_value").unwrap_or(0.0),
            volume_min: numero("volume_min").unwrap_or(0.0),
            volume_step: numero("volume_step").unwrap_or(0.0),
            volume_max: numero("volume_max").unwrap_or(0.0),
        };
        {
            let mut estado = self.estado.lock().await;
            estado.instrumentos.insert(i.con_id, i.clone());
            let lista: Vec<Instrumento> = estado.instrumentos.values().cloned().collect();
            self.entorno().guardar_instrumentos(&lista);
        }
        self.entorno().log(
            Nivel::Info,
            sistema,
            &format!(
                "{} → {} en MT5 ({} del bot)",
                root,
                i.mt5,
                if i.forex {
                    format!("{} lotes por 100.000 unidades", texto(lotes_por_unidad(&i) * 100_000.0))
                } else {
                    format!("{} lotes por contrato", seis(lotes_por_unidad(&i)))
                }
            ),
        );
        Ok(i)
    }

    /// Market order: closes the bot's opposite positions first (oldest first),
    /// then opens the rest.
    async fn ejecutar_mercado(
        &self,
        estado: &mut Estado,
        o: &mut OrdenVirtual,
        i: &Instrumento,
        magic: i64,
        ahora: i64,
    ) {
        let paso = if i.volume_step > 0.0 {
            i.volume_step
        } else {
            0.01
        };
        let mut falta = lotes(i, o.qty - o.filled);
        let compra = o.action == "BUY";
        let opuestas: Vec<PosicionMt5> = self
            .posiciones_bot(&o.account, magic, &o.symbol)
            .into_iter()
            .filter(|p| (p.side == "long") != compra)
            .collect();
        let mut fallo: Option<ResultadoMt5> = None;
        for p in opuestas {
            if falta < paso / 2.0 {
                break;
            }
            let volumen = redondear(p.volume.min(falta), 8);
            let r = self
                .entorno()
                .comando(
                    &o.account,
                    "CLOSEVOL",
                    &format!("{};{}", p.ticket, texto(volumen)),
                )
                .await;
            if r.ok {
                let precio = if r.numero("price", 0.0) > 0.0 {
                    r.numero("price", 0.0)
                } else {
                    p.current
                };
                let hecho = if r.numero("volume", 0.0) > 0.0 {
                    r.numero("volume", 0.0)
                } else {
                    volumen
                };
                self.sumar_ejecucion(estado, o, r.entero("deal", 0), unidades(i, hecho), precio, ahora);
                falta = redondear(falta - hecho, 8);
            } else if r.retcode != 10036 {
                fallo = Some(r);
                break;
            }
        }
        if fallo.is_none() && falta >= paso / 2.0 {
            let lado = if compra { "buy" } else { "sell" };
            let r = self
                .entorno()
                .comando(
                    &o.account,
                    "DEAL",
                    &format!(
                        "{};{};{};{};{}",
                        o.symbol,
                        lado,
                        texto(falta),
                        magic,
                        comentario(o)
                    ),
                )
                .await;
            if r.ok {
                let hecho = if r.numero("volume", 0.0) > 0.0 {
                    r.numero("volume", 0.0)
                } else {
                    falta
                };
                self.sumar_ejecucion(
                    estado,
                    o,
                    r.entero("deal", 0),
                    unidades(i, hecho),
                    r.numero("price", 0.0),
                    ahora,
                );
            } else {
                fallo = Some(r);
            }
        }
        let Some(fallo) = fallo else {
            // Float noise counts as done; a size MT5 could not trade in whole
            // volume steps keeps what it really executed.
            if (o.qty - o.filled).abs() <= o.qty.abs() * 1e-6 {
                o.filled = o.filled.max(o.qty);
            }
            estado_de(o, "Filled", "", ahora);
            self.entorno().log(
                Nivel::Info,
                &o.system,
                &format!(
                    "Orden {} ejecutada a {} en {}",
                    o.order_id, o.avg_fill_price, o.symbol
                ),
            );
            return;
        };
        if REINTENTO.contains(&fallo.retcode) {
            o.next_try = ahora + 10_000;
            if o.status != "Submitted" || o.message != fallo.message {
                self.entorno().log(
                    Nivel::Warning,
                    &o.system,
                    &format!(
                        "Orden {} en espera: {} (se reintenta)",
                        o.order_id, fallo.message
                    ),
                );
            }
            estado_de(o, "Submitted", &fallo.message, ahora);
            return;
        }
        self.entorno().log(
            Nivel::Error,
            &o.system,
            &format!(
                "Orden {} rechazada por MT5: {} ({})",
                o.order_id, fallo.message, fallo.retcode
            ),
        );
        let nuevo = if o.filled > 0.0 { "Cancelled" } else { "Inactive" };
        estado_de(o, nuevo, &fallo.message, ahora);
    }

    /// Entry stop or limit: a pending order on the broker. Already crossed →
    /// it triggers now, at market.
    async fn poner_entrada(
        &self,
        estado: &mut Estado,
        o: &mut OrdenVirtual,
        i: &Instrumento,
        magic: i64,
        ahora: i64,
    ) {
        let lado = if o.action == "BUY" { "buy" } else { "sell" };
        let (tipo, precio) = if o.order_type == "STP" {
            ("stop", o.aux_price)
        } else {
            ("limit", o.lmt_price)
        };
        let lots = lotes(i, o.qty - o.filled);
        let r = self
            .entorno()
            .comando(
                &o.account,
                "PENDING",
                &format!(
                    "{};{};{};{};{};{};{};{}",
                    o.symbol,
                    lado,
                    tipo,
                    texto(lots),
                    texto(precio),
                    magic,
                    comentario(o),
                    if o.tif == "DAY" { "1" } else { "0" }
                ),
            )
            .await;
        if r.ok {
            o.ticket = r.entero("order", 0);
            estado_de(o, "Submitted", "", ahora);
        } else if CRUZADO.contains(&r.retcode) {
            self.entorno().log(
                Nivel::Info,
                &o.system,
                &format!(
                    "Orden {}: el precio ya pasó {}; se ejecuta a mercado",
                    o.order_id,
                    texto(precio)
                ),
            );
            self.ejecutar_mercado(estado, o, i, magic, ahora).await;
        } else if REINTENTO.contains(&r.retcode) {
            o.next_try = ahora + 10_000;
            estado_de(o, "Submitted", &r.message, ahora);
        } else {
            self.entorno().log(
                Nivel::Error,
                &o.system,
                &format!(
                    "Orden {} rechazada por MT5: {} ({})",
                    o.order_id, r.message, r.retcode
                ),
            );
            estado_de(o, "Inactive", &r.message, ahora);
        }
    }

    fn salidas_activas(&self, estado: &Estado, sistema: &str, con_id: i64) -> Vec<OrdenVirtual> {
        let mut salidas: Vec<OrdenVirtual> = estado
            .ordenes
            .values()
            .filter(|o| {
                o.system == sistema
                    && o.con_id == con_id
                    && activa(o)
                    && (o.role == "exit_sl" || o.role == "exit_tp")
            })
            .cloned()
            .collect();
        salidas.sort_by_key(|o| o.updated);
        salidas
    }

    /// Takes a cancelled exit's stop or target off the bot's positions.
    async fn quitar_proteccion(
        &self,
        estado: &mut Estado,
        o: &OrdenVirtual,
        magic: i64,
        _ahora: i64,
    ) {
        let siguen = self.salidas_activas(estado, &o.system, o.con_id);
        let sl_vigente = siguen
            .iter()
            .rev()
            .find(|x| x.role == "exit_sl")
            .map(|x| x.aux_price);
        let tp_vigente = siguen
            .iter()
            .rev()
            .find(|x| x.role == "exit_tp")
            .map(|x| x.lmt_price);
        // The terminal applies each SLTP asynchronously, so the book may still
        // carry the sibling's old value: cancelling the target and then the
        // stop would re-arm the target with the stale price it reads back. A
        // role the bot has ever ordered on this instrument is the bot's to
        // clear; roles it never touched keep whatever the book says.
        let hay_sl = estado.ordenes.values().any(|x| {
            x.system == o.system && x.con_id == o.con_id && x.role == "exit_sl"
        });
        let hay_tp = estado.ordenes.values().any(|x| {
            x.system == o.system && x.con_id == o.con_id && x.role == "exit_tp"
        });
        for p in self.posiciones_bot(&o.account, magic, &o.symbol) {
            let sl = if hay_sl {
                sl_vigente.unwrap_or(0.0)
            } else {
                p.sl
            };
            let tp = if hay_tp {
                tp_vigente.unwrap_or(0.0)
            } else {
                p.tp
            };
            self.entorno()
                .comando(
                    &o.account,
                    "SLTP",
                    &format!("{};{};{}", p.ticket, texto(sl), texto(tp)),
                )
                .await;
        }
    }

    /// Keeps every position of the bot on that instrument carrying its working
    /// exit stop and target, and books those exits when MT5 executed them.
    async fn proteger(
        &self,
        estado: &mut Estado,
        enlace: &str,
        sistema: &str,
        con_id: i64,
        magic: i64,
        ahora: i64,
    ) {
        let salidas = self.salidas_activas(estado, sistema, con_id);
        if salidas.is_empty() {
            return;
        }
        let Some(i) = estado.instrumentos.get(&con_id).cloned() else {
            return;
        };
        let posiciones = self.posiciones_bot(enlace, magic, &i.mt5);

        // 1) Exits MT5 already executed (stop loss / take profit deals of this
        //    bot since the order).
        let primera = salidas.iter().map(|o| o.created).min().unwrap_or(0);
        let usadas: HashSet<i64> = estado
            .ordenes
            .values()
            .flat_map(|o| o.deals.iter().copied())
            .collect();
        let golpes: Vec<OperacionMt5> = self
            .operaciones(enlace)
            .into_iter()
            .filter(|d| {
                d.magic == magic
                    && d.symbol == i.mt5
                    && d.entry != "in"
                    && d.time >= primera - 2_000
                    && !usadas.contains(&d.deal)
                    && (d.reason == "sl" || d.reason == "tp")
            })
            .collect();
        for motivo in ["sl", "tp"] {
            let rol = if motivo == "sl" { "exit_sl" } else { "exit_tp" };
            let ds: Vec<OperacionMt5> = golpes
                .iter()
                .filter(|d| d.reason == motivo)
                .cloned()
                .collect();
            if ds.is_empty() {
                continue;
            }
            let Some(mut o) = salidas.iter().rev().find(|x| x.role == rol).cloned() else {
                continue;
            };
            let lots: f64 = ds.iter().map(|d| d.volume).sum();
            let px = ds.iter().map(|d| d.price * d.volume).sum::<f64>() / lots.max(1e-12);
            let at = ds.iter().map(|d| d.time).max().unwrap_or(ahora);
            let units = unidades(&i, lots).min(o.qty - o.filled).max(0.0);
            o.deals.extend(ds.iter().map(|d| d.deal));
            self.sumar_ejecucion(estado, &mut o, 0, units, px, at);
            if posiciones.is_empty() || o.filled >= o.qty - 1e-9 {
                o.filled = o.qty;
                estado_de(&mut o, "Filled", "", ahora);
                self.entorno().log(
                    Nivel::Info,
                    sistema,
                    &format!(
                        "Orden {} ({}) ejecutada por MT5 a {px}",
                        o.order_id,
                        if motivo == "sl" { "stop" } else { "objetivo" }
                    ),
                );
            }
            self.guardar(estado, &o);
        }

        // 2) No position left: the other exits (OCA siblings or orphans) end,
        //    like an OCA group on TWS.
        if posiciones.is_empty() {
            for mut o in self.salidas_activas(estado, sistema, con_id) {
                if ahora - o.created < GRACIA_MS && golpes.is_empty() {
                    continue;
                }
                let mensaje = if golpes.is_empty() {
                    "la posición ya no está abierta"
                } else {
                    "OCA: el stop o el objetivo del grupo se ejecutó"
                };
                estado_de(&mut o, "Cancelled", mensaje, ahora);
                self.guardar(estado, &o);
            }
            return;
        }

        // 3) Each position carries the working stop and target.
        let vigentes: Vec<OrdenVirtual> = self
            .salidas_activas(estado, sistema, con_id)
            .into_iter()
            .rev()
            .collect();
        let sl = vigentes
            .iter()
            .find(|o| o.role == "exit_sl")
            .map(|o| o.aux_price);
        let tp = vigentes
            .iter()
            .find(|o| o.role == "exit_tp")
            .map(|o| o.lmt_price);
        let tolerancia = (i.tick_size / 2.0).max(1e-9);
        for p in posiciones {
            let quiere_sl = sl.unwrap_or(p.sl);
            let quiere_tp = tp.unwrap_or(p.tp);
            if (p.sl - quiere_sl).abs() < tolerancia && (p.tp - quiere_tp).abs() < tolerancia {
                continue;
            }
            if ahora - estado.sltp_enviado.get(&p.ticket).copied().unwrap_or(0) < 3_000 {
                continue;
            }
            estado.sltp_enviado.insert(p.ticket, ahora);
            let r = self
                .entorno()
                .comando(
                    enlace,
                    "SLTP",
                    &format!(
                        "{};{};{}",
                        p.ticket,
                        texto(quiere_sl),
                        texto(quiere_tp)
                    ),
                )
                .await;
            if r.ok || !CRUZADO.contains(&r.retcode) {
                if !r.ok {
                    self.entorno().log(
                        Nivel::Warning,
                        sistema,
                        &format!(
                            "MT5 no aceptó stop/objetivo en {}: {}",
                            p.ticket, r.message
                        ),
                    );
                }
                continue;
            }
            // Refused because the price is already beyond it: it would have
            // triggered on TWS. Close now.
            let larga = p.side == "long";
            let toca_sl = quiere_sl > 0.0
                && (if larga {
                    p.current <= quiere_sl
                } else {
                    p.current >= quiere_sl
                });
            let toca_tp = quiere_tp > 0.0
                && (if larga {
                    p.current >= quiere_tp
                } else {
                    p.current <= quiere_tp
                });
            if !(toca_sl || toca_tp) {
                continue;
            }
            let rol = if toca_sl { "exit_sl" } else { "exit_tp" };
            let cierre = self
                .entorno()
                .comando(
                    enlace,
                    "CLOSEVOL",
                    &format!("{};{}", p.ticket, texto(p.volume)),
                )
                .await;
            if !cierre.ok {
                continue;
            }
            let Some(mut o) = self
                .salidas_activas(estado, sistema, con_id)
                .into_iter()
                .rev()
                .find(|x| x.role == rol)
            else {
                continue;
            };
            let precio = if cierre.numero("price", 0.0) > 0.0 {
                cierre.numero("price", 0.0)
            } else {
                p.current
            };
            let units = unidades(&i, p.volume).min(o.qty - o.filled);
            self.sumar_ejecucion(
                estado,
                &mut o,
                cierre.entero("deal", 0),
                units,
                precio,
                ahora,
            );
            let quedan = self
                .posiciones_bot(enlace, magic, &i.mt5)
                .into_iter()
                .filter(|q| q.ticket != p.ticket)
                .count();
            if o.filled >= o.qty - 1e-9 || quedan == 0 {
                o.filled = o.qty;
                estado_de(&mut o, "Filled", "", ahora);
            }
            self.guardar(estado, &o);
            self.entorno().log(
                Nivel::Info,
                sistema,
                &format!(
                    "Orden {}: el precio ya había pasado el nivel; cerrada a mercado a {precio}",
                    o.order_id
                ),
            );
        }
    }

    /// Adds an execution of `units` at `price` to the order and books the fill.
    fn sumar_ejecucion(
        &self,
        estado: &mut Estado,
        o: &mut OrdenVirtual,
        deal: i64,
        units: f64,
        price: f64,
        at: i64,
    ) {
        if units <= 0.0 {
            return;
        }
        let total = o.filled + units;
        o.avg_fill_price = if total > 0.0 {
            (o.avg_fill_price * o.filled + price * units) / total
        } else {
            price
        };
        o.filled = total;
        if deal != 0 {
            o.deals.push(deal);
        }
        let f = Ejecucion {
            exec_id: format!("{deal}.{}", o.order_id),
            order_id: o.order_id,
            perm_id: o.perm_id,
            system: o.system.clone(),
            client_id: o.client_id,
            con_id: o.con_id,
            symbol: o.symbol.clone(),
            side: if o.action == "BUY" { "BOT" } else { "SLD" }.to_string(),
            shares: units,
            price,
            time: at,
            account: self.nombre_cuenta(&o.account),
        };
        estado.ejecuciones.push(f.clone());
        self.entorno().guardar_ejecucion(&f);
    }

    /// The bot's signed position on the order's symbol in lots, from MT5, or
    /// from its own fills of the last seconds while MT5 has not reported the
    /// new position yet.
    fn neto_de(&self, estado: &Estado, o: &OrdenVirtual, magic: i64) -> f64 {
        let n = neto(&self.posiciones_bot(&o.account, magic, &o.symbol));
        if n.abs() > 1e-9 {
            return n;
        }
        let reciente = self.ahora() - 15_000;
        let unidades: f64 = estado
            .ejecuciones
            .iter()
            .filter(|f| f.system == o.system && f.con_id == o.con_id && f.time >= reciente)
            .map(|f| if f.side == "BOT" { f.shares } else { -f.shares })
            .sum();
        match estado.instrumentos.get(&o.con_id) {
            Some(i) => unidades * lotes_por_unidad(i),
            None => 0.0,
        }
    }
}

fn neto(posiciones: &[PosicionMt5]) -> f64 {
    posiciones
        .iter()
        .map(|p| if p.side == "short" { -p.volume } else { p.volume })
        .sum()
}

fn comentario(o: &OrdenVirtual) -> String {
    format!("OB{}#{}", o.system, o.order_id)
}

fn contrato_json(i: &Instrumento, sec_type: &str) -> ContratoJson {
    let (symbol, currency) = if i.forex && i.root.len() == 6 {
        (i.root[..3].to_string(), i.root[3..].to_string())
    } else {
        (i.root.clone(), i.currency.clone())
    };
    ContratoJson {
        con_id: i.con_id,
        sec_type: if sec_type.is_empty() {
            i.sec_type.clone()
        } else {
            sec_type.to_string()
        },
        symbol,
        currency,
        exchange: i.exchange.clone(),
        local_symbol: i.mt5.clone(),
        trading_class: i.root.clone(),
        multiplier: if i.forex {
            String::new()
        } else {
            texto(i.multiplier)
        },
        last_trade_date_or_contract_month: String::new(),
        mt5: i.mt5.clone(),
        lots_per_unit: lotes_por_unidad(i),
    }
}

fn podar(estado: &mut Estado, ahora: i64) {
    let antes = ahora - RETENCION_MS;
    estado
        .ordenes
        .retain(|_, o| activa(o) || o.updated >= antes);
    estado.ejecuciones.retain(|f| f.time >= antes);
    estado
        .sltp_enviado
        .retain(|_, enviado| ahora - *enviado < 60_000);
}

fn ahora_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mt5::libro::PendienteMt5;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Mutex as StdMutex;

    const ENLACE: &str = "mt5-1@OpenBot-Sim";

    struct Falso {
        libros: StdMutex<HashMap<String, LibroMt5>>,
        comandos: StdMutex<Vec<(String, String)>>,
        responder: Box<dyn Fn(&str, &str) -> ResultadoMt5 + Send + Sync>,
        ejecuciones: StdMutex<Vec<Ejecucion>>,
        logs: StdMutex<Vec<String>>,
    }

    #[async_trait]
    impl EntornoMt5 for Falso {
        fn enlace_de(&self, sistema: &str) -> std::result::Result<String, String> {
            if sistema == "7" {
                Ok(ENLACE.to_string())
            } else {
                Err(format!("el sistema {sistema} no opera en MT5"))
            }
        }

        fn libro(&self, enlace: &str) -> Option<LibroMt5> {
            self.libros.lock().unwrap().get(enlace).cloned()
        }

        async fn comando(&self, _enlace: &str, verbo: &str, arg: &str) -> ResultadoMt5 {
            self.comandos
                .lock()
                .unwrap()
                .push((verbo.to_string(), arg.to_string()));
            (self.responder)(verbo, arg)
        }

        fn magic(&self, _sistema: &str) -> i64 {
            7_700_007
        }

        fn log(&self, _nivel: Nivel, _sistema: &str, mensaje: &str) {
            self.logs.lock().unwrap().push(mensaje.to_string());
        }

        fn guardar_ejecucion(&self, ejecucion: &Ejecucion) {
            self.ejecuciones.lock().unwrap().push(ejecucion.clone());
        }
    }

    fn instrumento() -> Instrumento {
        Instrumento {
            con_id: 1,
            root: "MNQ".to_string(),
            sec_type: "FUT".to_string(),
            exchange: String::new(),
            currency: "USD".to_string(),
            multiplier: 2.0,
            forex: false,
            account: ENLACE.to_string(),
            mt5: "US100".to_string(),
            digits: 2,
            contract_size: 1.0,
            tick_size: 0.01,
            tick_value: 0.01,
            volume_min: 0.01,
            volume_step: 0.01,
            volume_max: 100.0,
        }
    }

    fn libro(
        positions: Vec<PosicionMt5>,
        orders: Vec<PendienteMt5>,
        deals: Vec<OperacionMt5>,
    ) -> LibroMt5 {
        LibroMt5 {
            login: 5_550_001,
            server: "OpenBot-Sim".to_string(),
            demo: true,
            currency: "USD".to_string(),
            leverage: 100,
            balance: Some(100_000.0),
            equity: Some(100_000.0),
            margin: Some(0.0),
            margin_free: Some(100_000.0),
            profit: Some(0.0),
            positions,
            orders,
            deals: Some(deals),
        }
    }

    fn posicion(ticket: i64, side: &str, volume: f64, entry: f64, current: f64) -> PosicionMt5 {
        PosicionMt5 {
            ticket,
            symbol: "US100".to_string(),
            side: side.to_string(),
            volume,
            entry,
            current,
            sl: 0.0,
            tp: 0.0,
            magic: 7_700_007,
            time: 1,
        }
    }

    fn posicion_con_sltp(ticket: i64, sl: f64, tp: f64) -> PosicionMt5 {
        PosicionMt5 {
            sl,
            tp,
            ..posicion(ticket, "long", 2.0, 100.0, 101.0)
        }
    }

    fn falso(
        responder: impl Fn(&str, &str) -> ResultadoMt5 + Send + Sync + 'static,
    ) -> Arc<Falso> {
        let mut libros = HashMap::new();
        libros.insert(ENLACE.to_string(), libro(vec![], vec![], vec![]));
        Arc::new(Falso {
            libros: StdMutex::new(libros),
            comandos: StdMutex::new(Vec::new()),
            responder: Box::new(responder),
            ejecuciones: StdMutex::new(Vec::new()),
            logs: StdMutex::new(Vec::new()),
        })
    }

    fn reloj(inicio: i64) -> Arc<AtomicI64> {
        Arc::new(AtomicI64::new(inicio))
    }

    fn traductor(entorno: &Arc<Falso>, reloj: Arc<AtomicI64>) -> Traductor {
        Traductor::with_clock(
            entorno.clone(),
            EstadoInicial {
                instrumentos: vec![instrumento()],
                ..EstadoInicial::default()
            },
            Arc::new(move || reloj.load(Ordering::SeqCst)),
        )
    }

    fn ok(data: serde_json::Value) -> ResultadoMt5 {
        ResultadoMt5::bien(Some(data))
    }

    fn fallo(retcode: i64) -> ResultadoMt5 {
        ResultadoMt5::fallo(retcode, format!("error {retcode}"))
    }

    fn peticion(accion: &str, tipo: &str, qty: f64) -> PeticionOrden {
        PeticionOrden {
            order_id: None,
            client_id: Some(1),
            con_id: 1,
            action: accion.to_string(),
            order_type: tipo.to_string(),
            total_quantity: qty,
            aux_price: None,
            lmt_price: None,
            tif: None,
            oca_group: None,
        }
    }

    fn comandos(entorno: &Arc<Falso>) -> Vec<(String, String)> {
        entorno.comandos.lock().unwrap().clone()
    }

    fn poner_posiciones(entorno: &Arc<Falso>, posiciones: Vec<PosicionMt5>) {
        entorno
            .libros
            .lock()
            .unwrap()
            .get_mut(ENLACE)
            .unwrap()
            .positions = posiciones;
    }

    fn poner_libro(entorno: &Arc<Falso>, nuevo: LibroMt5) {
        entorno
            .libros
            .lock()
            .unwrap()
            .insert(ENLACE.to_string(), nuevo);
    }

    #[tokio::test]
    async fn a_stop_or_limit_at_zero_or_below_is_refused_and_sends_nothing() {
        let entorno = falso(|verbo, _| panic!("nothing may reach MT5, got {verbo}"));
        let traductor = traductor(&entorno, reloj(1_000_000));
        for (tipo, precio) in [("LMT", 0.0), ("LMT", -1.0), ("STP", 0.0)] {
            let mut p = peticion("BUY", tipo, 1.0);
            if tipo == "LMT" {
                p.lmt_price = Some(precio);
            } else {
                p.aux_price = Some(precio);
            }
            let error = traductor.place("7", p).await.expect_err("refused");
            assert!(error.to_string().contains("precio no válido"), "{error}");
        }
        assert!(comandos(&entorno).is_empty());
    }

    #[tokio::test]
    async fn a_market_order_opens_the_position_and_books_its_fill() {
        let entorno = falso(|verbo, _| {
            assert_eq!(verbo, "DEAL");
            ok(serde_json::json!({"deal": 11, "price": 100.5, "volume": 2.0}))
        });
        let traductor = traductor(&entorno, reloj(1_000_000));

        let orden = traductor
            .place("7", peticion("BUY", "MKT", 1.0))
            .await
            .expect("place");

        assert_eq!(orden.order_id, 1_000);
        assert_eq!(orden.role, "market");
        assert_eq!(orden.status, "Filled");
        assert_eq!(orden.filled, 1.0);
        assert_eq!(orden.avg_fill_price, 100.5);
        assert_eq!(orden.deals, vec![11]);
        assert_eq!(
            comandos(&entorno),
            vec![(
                "DEAL".to_string(),
                "US100;buy;2;7700007;OB7#1000".to_string()
            )]
        );
        let ejecuciones = entorno.ejecuciones.lock().unwrap().clone();
        assert_eq!(ejecuciones.len(), 1);
        assert_eq!(ejecuciones[0].shares, 1.0);
        assert_eq!(ejecuciones[0].price, 100.5);
        assert_eq!(ejecuciones[0].side, "BOT");
        assert_eq!(ejecuciones[0].account, "DU5550001");
    }

    #[tokio::test]
    async fn an_entry_stop_rests_on_the_broker_with_its_ticket() {
        let entorno = falso(|verbo, arg| {
            assert_eq!(verbo, "PENDING");
            assert_eq!(arg, "US100;sell;stop;2;99;7700007;OB7#1000;1");
            ok(serde_json::json!({"order": 555}))
        });
        let traductor = traductor(&entorno, reloj(1_000_000));
        let mut peticion = peticion("SELL", "STP", 1.0);
        peticion.aux_price = Some(99.0);

        let orden = traductor.place("7", peticion).await.expect("place");

        assert_eq!(orden.role, "entry");
        assert_eq!(orden.ticket, 555);
        assert_eq!(orden.status, "Submitted");
        assert_eq!(orden.filled, 0.0);
    }

    #[tokio::test]
    async fn a_stop_against_the_position_becomes_its_stop_loss() {
        let entorno = falso(|verbo, arg| {
            assert_eq!(verbo, "SLTP");
            assert_eq!(arg, "1;99;0");
            ok(serde_json::json!({}))
        });
        poner_posiciones(&entorno, vec![posicion(1, "long", 2.0, 100.0, 101.0)]);
        let traductor = traductor(&entorno, reloj(1_000_000));
        let mut peticion = peticion("SELL", "STP", 1.0);
        peticion.aux_price = Some(99.0);

        let orden = traductor.place("7", peticion).await.expect("place");

        assert_eq!(orden.role, "exit_sl");
        assert_eq!(orden.status, "Submitted");
        assert_eq!(orden.ticket, 0);
        assert_eq!(comandos(&entorno).len(), 1);
    }

    #[tokio::test]
    async fn cancelling_a_target_takes_the_protection_off_the_position() {
        let entorno = falso(|verbo, _| {
            assert_eq!(verbo, "SLTP");
            ok(serde_json::json!({}))
        });
        poner_posiciones(&entorno, vec![posicion(1, "long", 2.0, 100.0, 101.0)]);
        let traductor = traductor(&entorno, reloj(1_000_000));
        let mut peticion = peticion("SELL", "LMT", 1.0);
        peticion.lmt_price = Some(110.0);
        let orden = traductor.place("7", peticion).await.expect("place");
        assert_eq!(orden.role, "exit_tp");
        let con_proteccion = comandos(&entorno);
        assert_eq!(con_proteccion[0].1, "1;0;110");

        let cancelada = traductor.cancel("7", orden.order_id).await.expect("cancel");

        assert_eq!(cancelada.status, "Cancelled");
        let despues = comandos(&entorno);
        assert_eq!(despues.len(), 2);
        assert_eq!(despues[1], ("SLTP".to_string(), "1;0;0".to_string()));
    }

    #[tokio::test]
    async fn cancelling_both_exits_does_not_rearm_the_one_the_book_still_shows() {
        // MT5 applies each SLTP asynchronously: when the stop is cancelled a
        // moment after the target, the book still carries the target's price.
        // Reading it back would re-arm the target that was just removed.
        let entorno = falso(|verbo, _| {
            assert_eq!(verbo, "SLTP");
            ok(serde_json::json!({}))
        });
        poner_posiciones(&entorno, vec![posicion(1, "long", 2.0, 100.0, 101.0)]);
        let traductor = traductor(&entorno, reloj(1_000_000));

        let mut objetivo = peticion("SELL", "LMT", 1.0);
        objetivo.lmt_price = Some(110.0);
        let objetivo = traductor.place("7", objetivo).await.expect("place target");
        let mut stop = peticion("SELL", "STP", 1.0);
        stop.aux_price = Some(99.0);
        let stop = traductor.place("7", stop).await.expect("place stop");

        // Both on the position, as the terminal left it.
        poner_posiciones(&entorno, vec![posicion_con_sltp(1, 99.0, 110.0)]);
        traductor.cancel("7", objetivo.order_id).await.expect("cancel target");
        // The target's removal has not landed in the book yet.
        poner_posiciones(&entorno, vec![posicion_con_sltp(1, 99.0, 110.0)]);
        traductor.cancel("7", stop.order_id).await.expect("cancel stop");

        let despues = comandos(&entorno);
        assert_eq!(despues.last().unwrap().1, "1;0;0");
    }

    #[tokio::test]
    async fn a_crossed_entry_runs_at_market_instead_of_resting() {
        let entorno = falso(|verbo, _| match verbo {
            "PENDING" => fallo(10015),
            "DEAL" => ok(serde_json::json!({"deal": 12, "price": 99.5, "volume": 2.0})),
            otro => panic!("verbo inesperado: {otro}"),
        });
        let traductor = traductor(&entorno, reloj(1_000_000));
        let mut peticion = peticion("SELL", "STP", 1.0);
        peticion.aux_price = Some(99.0);

        let orden = traductor.place("7", peticion).await.expect("place");

        assert_eq!(orden.status, "Filled");
        assert_eq!(orden.filled, 1.0);
        assert_eq!(
            comandos(&entorno)
                .into_iter()
                .map(|(verbo, _)| verbo)
                .collect::<Vec<_>>(),
            vec!["PENDING", "DEAL"]
        );
    }

    #[tokio::test]
    async fn a_retry_waits_ten_seconds_before_trying_again() {
        let intentos = Arc::new(AtomicI64::new(0));
        let contador = Arc::clone(&intentos);
        let entorno = falso(move |verbo, _| {
            assert_eq!(verbo, "DEAL");
            if contador.fetch_add(1, Ordering::SeqCst) == 0 {
                fallo(10004)
            } else {
                ok(serde_json::json!({"deal": 13, "price": 100.0, "volume": 2.0}))
            }
        });
        let reloj = reloj(1_000_000);
        let traductor = traductor(&entorno, Arc::clone(&reloj));

        let orden = traductor
            .place("7", peticion("BUY", "MKT", 1.0))
            .await
            .expect("place");
        assert_eq!(orden.status, "Submitted");
        assert_eq!(orden.next_try, 1_010_000);

        traductor.tick().await;
        assert_eq!(comandos(&entorno).len(), 1, "no reintenta antes de tiempo");

        reloj.fetch_add(10_000, Ordering::SeqCst);
        traductor.tick().await;

        let orden = traductor.orden(1_000).await.expect("orden");
        assert_eq!(orden.status, "Filled");
        assert_eq!(comandos(&entorno).len(), 2);
    }

    #[tokio::test]
    async fn the_tick_books_the_fill_of_a_resting_entry() {
        let entorno = falso(|verbo, _| {
            assert_eq!(verbo, "PENDING");
            ok(serde_json::json!({"order": 555}))
        });
        let reloj = reloj(1_000_000);
        let traductor = traductor(&entorno, Arc::clone(&reloj));
        let mut peticion = peticion("SELL", "STP", 1.0);
        peticion.aux_price = Some(99.0);
        traductor.place("7", peticion).await.expect("place");

        poner_libro(
            &entorno,
            libro(
                vec![posicion(1, "short", 2.0, 99.0, 98.0)],
                vec![],
                vec![OperacionMt5 {
                    deal: 77,
                    order: 555,
                    position: 1,
                    symbol: "US100".to_string(),
                    side: "short".to_string(),
                    volume: 2.0,
                    price: 99.0,
                    pnl: 0.0,
                    magic: 7_700_007,
                    time: 1_000_500,
                    entry: "in".to_string(),
                    reason: "other".to_string(),
                }],
            ),
        );
        reloj.fetch_add(1_000, Ordering::SeqCst);
        traductor.tick().await;

        let orden = traductor.orden(1_000).await.expect("orden");
        assert_eq!(orden.status, "Filled");
        assert_eq!(orden.filled, 1.0);
        assert_eq!(orden.deals, vec![77]);
        let ejecuciones = entorno.ejecuciones.lock().unwrap().clone();
        assert_eq!(ejecuciones.len(), 1);
        assert_eq!(ejecuciones[0].shares, 1.0);
    }

    #[tokio::test]
    async fn a_vanished_entry_is_cancelled_after_the_grace_period() {
        let entorno = falso(|verbo, _| match verbo {
            "PENDING" => ok(serde_json::json!({"order": 555})),
            otro => panic!("verbo inesperado: {otro}"),
        });
        let reloj = reloj(1_000_000);
        let traductor = traductor(&entorno, Arc::clone(&reloj));
        let mut peticion = peticion("SELL", "STP", 1.0);
        peticion.aux_price = Some(99.0);
        traductor.place("7", peticion).await.expect("place");

        reloj.fetch_add(7_000, Ordering::SeqCst);
        traductor.tick().await;

        let orden = traductor.orden(1_000).await.expect("orden");
        assert_eq!(orden.status, "Cancelled");
        assert_eq!(orden.message, "la orden ya no está en MT5 (caducada o borrada)");
    }

    #[tokio::test]
    async fn a_market_order_closes_the_oldest_opposite_position_first() {
        let entorno = falso(|verbo, arg| match verbo {
            "CLOSEVOL" => {
                assert_eq!(arg, "9;1");
                ok(serde_json::json!({"deal": 12, "price": 100.0, "volume": 1.0}))
            }
            "DEAL" => {
                assert_eq!(arg, "US100;sell;5;7700007;OB7#1000");
                ok(serde_json::json!({"deal": 13, "price": 99.5, "volume": 5.0}))
            }
            otro => panic!("verbo inesperado: {otro}"),
        });
        poner_posiciones(&entorno, vec![posicion(9, "long", 1.0, 100.0, 100.0)]);
        let traductor = traductor(&entorno, reloj(1_000_000));

        let orden = traductor
            .place("7", peticion("SELL", "MKT", 3.0))
            .await
            .expect("place");

        assert_eq!(orden.status, "Filled");
        assert_eq!(orden.filled, 3.0);
        assert_eq!(
            comandos(&entorno)
                .into_iter()
                .map(|(verbo, _)| verbo)
                .collect::<Vec<_>>(),
            vec!["CLOSEVOL", "DEAL"]
        );
        let ejecuciones = entorno.ejecuciones.lock().unwrap().clone();
        assert_eq!(ejecuciones.len(), 2);
        assert_eq!(ejecuciones[0].shares, 0.5);
        assert_eq!(ejecuciones[1].shares, 2.5);
    }

    #[tokio::test]
    async fn panic_deletes_pending_entries_and_ends_every_order() {
        let entorno = falso(|verbo, arg| match verbo {
            "PENDING" => ok(serde_json::json!({"order": 555})),
            "DELETE" => {
                assert_eq!(arg, "555");
                ok(serde_json::json!({}))
            }
            otro => panic!("verbo inesperado: {otro}"),
        });
        let traductor = traductor(&entorno, reloj(1_000_000));
        let mut peticion = peticion("SELL", "STP", 1.0);
        peticion.aux_price = Some(99.0);
        traductor.place("7", peticion).await.expect("place");

        let cancelacion = traductor.cancelar_todo().await;

        assert_eq!(cancelacion.canceladas, 1);
        assert!(cancelacion.fallos.is_empty());
        let orden = traductor.orden(1_000).await.expect("orden");
        assert_eq!(orden.status, "Cancelled");
        assert_eq!(orden.message, "pánico");
    }

    #[tokio::test]
    async fn the_bridge_books_keep_the_bots_own_units() {
        let entorno = falso(|verbo, _| match verbo {
            "SYMBOL" => ok(serde_json::json!({})),
            otro => panic!("verbo inesperado: {otro}"),
        });
        poner_posiciones(&entorno, vec![posicion(1, "long", 2.0, 100.0, 101.0)]);
        let traductor = traductor(&entorno, reloj(1_000_000));

        let ps = traductor.positions("7").await;

        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].account, "DU5550001");
        assert_eq!(ps[0].position, 1.0);
        assert_eq!(ps[0].avg_cost, 200.0);
    }

    #[tokio::test]
    async fn qualify_resolves_the_first_symbol_the_broker_has() {
        let entorno = falso(|verbo, arg| {
            assert_eq!(verbo, "SYMBOL");
            if arg == "USTEC" {
                ok(serde_json::json!({
                    "symbol": "USTEC",
                    "digits": 2,
                    "contract_size": 1.0,
                    "tick_size": 0.01,
                    "tick_value": 0.01,
                    "volume_min": 0.1,
                    "volume_step": 0.1,
                    "volume_max": 250.0,
                    "currency_profit": "USD"
                }))
            } else {
                fallo(4301)
            }
        });
        let traductor = traductor(&entorno, reloj(1_000_000));

        let contrato = traductor
            .qualify(
                "7",
                PeticionContrato {
                    sec_type: Some("FUT".to_string()),
                    symbol: Some("MNQ".to_string()),
                    ..PeticionContrato::default()
                },
            )
            .await
            .expect("qualify");

        assert_eq!(contrato.mt5, "USTEC");
        assert_eq!(contrato.trading_class, "MNQ");
        assert_eq!(contrato.con_id, con_id_de("MNQ"));
        assert_eq!(contrato.multiplier, "2");
        assert_eq!(contrato.lots_per_unit, 2.0);
        assert_eq!(contrato.sec_type, "FUT");
        assert_eq!(
            comandos(&entorno)
                .into_iter()
                .map(|(_, arg)| arg)
                .collect::<Vec<_>>(),
            vec!["US100", "USTEC"]
        );
    }

    #[tokio::test]
    async fn qualify_skips_a_symbol_the_account_cannot_trade() {
        let entorno = falso(|_, arg| {
            let modo = match arg {
                "USTEC" => 0,
                "NAS100" => 4,
                _ => return fallo(4301),
            };
            ok(serde_json::json!({
                "symbol": arg,
                "digits": 2,
                "contract_size": 1.0,
                "tick_size": 0.01,
                "tick_value": 0.01,
                "volume_min": 0.1,
                "volume_step": 0.1,
                "volume_max": 250.0,
                "currency_profit": "USD",
                "trade_mode": modo
            }))
        });
        let traductor = traductor(&entorno, reloj(1_000_000));
        let peticion = || PeticionContrato {
            sec_type: Some("FUT".to_string()),
            symbol: Some("MNQ".to_string()),
            ..PeticionContrato::default()
        };

        let contrato = traductor.qualify("7", peticion()).await.expect("qualify");

        assert_eq!(contrato.mt5, "NAS100");
    }

    #[tokio::test]
    async fn qualify_keeps_a_disabled_symbol_when_it_is_the_only_one() {
        let entorno = falso(|_, arg| {
            if arg != "USTEC" {
                return fallo(4301);
            }
            ok(serde_json::json!({
                "symbol": "USTEC",
                "digits": 2,
                "contract_size": 1.0,
                "tick_size": 0.01,
                "tick_value": 0.01,
                "volume_min": 0.1,
                "volume_step": 0.1,
                "volume_max": 250.0,
                "currency_profit": "USD",
                "trade_mode": 0
            }))
        });
        let traductor = traductor(&entorno, reloj(1_000_000));

        let contrato = traductor
            .qualify(
                "7",
                PeticionContrato {
                    sec_type: Some("FUT".to_string()),
                    symbol: Some("MNQ".to_string()),
                    ..PeticionContrato::default()
                },
            )
            .await
            .expect("qualify");

        assert_eq!(contrato.mt5, "USTEC");
    }
}

