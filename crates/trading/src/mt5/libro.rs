//! What the MT5 bridge publishes and answers, as Rust types.
//!
//! The bridge (`puente.py`) writes one JSON object per line: the `bot` book
//! (account, positions, pending orders and the deals that explain them), the
//! `cuenta` snapshot and one `resultado` per command. These are the typed
//! versions the translator reads; keys keep the bridge's own snake_case names.

use serde::{Deserialize, Serialize};

/// One open position the broker holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PosicionMt5 {
    pub ticket: i64,
    pub symbol: String,
    /// `long` or `short`.
    pub side: String,
    pub volume: f64,
    pub entry: f64,
    pub current: f64,
    #[serde(default)]
    pub sl: f64,
    #[serde(default)]
    pub tp: f64,
    pub magic: i64,
    /// Position time, epoch ms in UTC (the bridge already removed the server offset).
    pub time: i64,
}

/// One working pending order on the broker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendienteMt5 {
    pub ticket: i64,
    pub symbol: String,
    /// `buy_stop`, `sell_stop`, `buy_limit`, `sell_limit` or `other`.
    pub kind: String,
    pub volume: f64,
    pub price: f64,
    pub magic: i64,
}

/// One broker deal (an execution or part of one).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperacionMt5 {
    pub deal: i64,
    pub order: i64,
    pub position: i64,
    pub symbol: String,
    pub side: String,
    pub volume: f64,
    pub price: f64,
    pub pnl: f64,
    pub magic: i64,
    pub time: i64,
    /// `in`, `out`, `inout` or `out_by`.
    pub entry: String,
    /// `sl`, `tp`, `so` or `other`.
    pub reason: String,
}

/// The link's book: the `bot` line the bridge repeats every half second.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LibroMt5 {
    pub login: i64,
    pub server: String,
    pub demo: bool,
    pub currency: String,
    pub leverage: i64,
    #[serde(default)]
    pub balance: Option<f64>,
    #[serde(default)]
    pub equity: Option<f64>,
    #[serde(default)]
    pub margin: Option<f64>,
    #[serde(default)]
    pub margin_free: Option<f64>,
    #[serde(default)]
    pub profit: Option<f64>,
    #[serde(default)]
    pub positions: Vec<PosicionMt5>,
    #[serde(default)]
    pub orders: Vec<PendienteMt5>,
    /// Three days of deals; only present when the book signature changed or
    /// the bridge's two-second window came round.
    #[serde(default)]
    pub deals: Option<Vec<OperacionMt5>>,
}

/// The `cuenta` line: the account and its floating P/L.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CuentaMt5 {
    pub login: i64,
    pub name: String,
    pub server: String,
    pub trade_mode: i64,
    pub balance: Option<f64>,
    pub equity: Option<f64>,
    pub profit: Option<f64>,
    pub margin_free: Option<f64>,
    pub margin_used: Option<f64>,
    pub pnl_day: Option<f64>,
    pub pnl_realized: Option<f64>,
    #[serde(default)]
    pub positions: Vec<PosicionCuenta>,
    #[serde(default)]
    pub closes: Vec<CierreMt5>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PosicionCuenta {
    pub symbol: String,
    pub side: String,
    pub size: Option<f64>,
    pub entry: Option<f64>,
    pub current: Option<f64>,
    pub pnl: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CierreMt5 {
    pub symbol: String,
    pub ts: i64,
    pub volume: Option<f64>,
    pub price: Option<f64>,
    pub pnl: Option<f64>,
}

/// One command's answer: `{"type":"resultado","id","ok","retcode","message","data"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultadoMt5 {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    pub ok: bool,
    pub retcode: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

impl ResultadoMt5 {
    pub fn bien(data: Option<serde_json::Value>) -> Self {
        Self {
            id: String::new(),
            ok: true,
            retcode: 0,
            message: String::new(),
            data,
        }
    }

    pub fn fallo(retcode: i64, message: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            ok: false,
            retcode,
            message: message.into(),
            data: None,
        }
    }

    /// A numeric field of `data`, or `defecto` when it is absent or not a number.
    pub fn numero(&self, clave: &str, defecto: f64) -> f64 {
        self.data
            .as_ref()
            .and_then(|datos| datos.get(clave))
            .and_then(serde_json::Value::as_f64)
            .filter(|valor| valor.is_finite())
            .unwrap_or(defecto)
    }

    /// An integer field of `data`, or `defecto`.
    pub fn entero(&self, clave: &str, defecto: i64) -> i64 {
        self.data
            .as_ref()
            .and_then(|datos| datos.get(clave))
            .and_then(|valor| {
                valor
                    .as_i64()
                    .or_else(|| valor.as_f64().map(|n| n as i64))
            })
            .unwrap_or(defecto)
    }
}

/// A `RATES` row: `[timeMs, open, high, low, close, tickVolume]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BarraMt5(pub i64, pub f64, pub f64, pub f64, pub f64, pub i64);
