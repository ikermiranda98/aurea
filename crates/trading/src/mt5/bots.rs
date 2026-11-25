//! The bots on MetaTrader 5 (port of OpenBot's `servidor-bots.ts` + `pyshim`).
//!
//! Bots are written against IBKR's `ib_async`. A bot started while an MT5
//! account is connected gets OpenBot's `ib_async` stand-in first on its
//! `PYTHONPATH`: its `from ib_async import IB, …` resolves there, and every
//! call reaches this local API (`/bot/*`, loopback only, one token per
//! system) instead of TWS. The API hands each call to the [`Traductor`],
//! which turns orders into bridge commands on the account's terminal.
//!
//! Safety stays with the bots: the translator names a demo account `DU…` and
//! a real one `U…`, and the bots refuse a non-`DU` account unless their
//! `PERMITIR_CUENTA_REAL` says otherwise.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use super::escritorio::Mt5Escritorio;
use super::libro::{LibroMt5, ResultadoMt5};
use super::ordenes::{
    EntornoMt5, EstadoInicial, Nivel, PeticionContrato, PeticionOrden, Resultado, Traductor,
};

const IB_ASYNC_INIT: &str = include_str!("standin/ib_async_init.py");
const IB_ASYNC_UTIL: &str = include_str!("standin/ib_async_util.py");
const IB_INSYNC_INIT: &str = include_str!("standin/ib_insync_init.py");

/// How often the translator reconciles fills and protective orders.
const TICK: Duration = Duration::from_secs(1);
/// A request larger than this is refused (orders and contracts are tiny).
const MAX_BODY: usize = 64 * 1024;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the translator needs, read from the desk's MT5 accounts.
struct EntornoEscritorio {
    escritorio: Arc<Mt5Escritorio>,
}

#[async_trait]
impl EntornoMt5 for EntornoEscritorio {
    fn enlace_de(&self, _sistema: &str) -> std::result::Result<String, String> {
        self.escritorio.enlace_para_bots().ok_or_else(|| {
            "no hay ninguna cuenta de MetaTrader 5 conectada en Aurea (Conexiones → Añadir MT5)"
                .to_string()
        })
    }

    fn libro(&self, enlace: &str) -> Option<LibroMt5> {
        self.escritorio.enlace(enlace)?.libro()
    }

    async fn comando(&self, enlace: &str, verbo: &str, arg: &str) -> ResultadoMt5 {
        match self.escritorio.enlace(enlace) {
            Some(enlace) => enlace.puente().comando(verbo, arg).await,
            None => ResultadoMt5::fallo(-1, format!("la cuenta {enlace} no está conectada")),
        }
    }

    fn log(&self, nivel: Nivel, sistema: &str, mensaje: &str) {
        match nivel {
            Nivel::Info => tracing::info!(target: "mt5", "sistema {sistema}: {mensaje}"),
            Nivel::Warning => tracing::warn!(target: "mt5", "sistema {sistema}: {mensaje}"),
            Nivel::Error => tracing::error!(target: "mt5", "sistema {sistema}: {mensaje}"),
        }
        self.escritorio.anotar(nivel, sistema, mensaje);
    }
}

/// The running bot API.
pub struct BotsMt5 {
    traductor: Arc<Traductor>,
    url: String,
    shim: PathBuf,
    fichero_motor: PathBuf,
    /// token → system id.
    tokens: Mutex<HashMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeticionVelas {
    con_id: i64,
    #[serde(default)]
    end: Option<i64>,
    duration: String,
    bar_size: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeticionCancelar {
    order_id: i64,
}

impl BotsMt5 {
    /// Writes the stand-in, starts the translator and the API on a free
    /// loopback port. Needs a tokio runtime.
    pub async fn iniciar(escritorio: Arc<Mt5Escritorio>, dir: &Path) -> std::io::Result<Arc<Self>> {
        let shim = dir.join("pyshim");
        for (ruta, texto) in [
            (shim.join("ib_async").join("__init__.py"), IB_ASYNC_INIT),
            (shim.join("ib_async").join("util.py"), IB_ASYNC_UTIL),
            (shim.join("ib_insync").join("__init__.py"), IB_INSYNC_INIT),
        ] {
            if let Some(padre) = ruta.parent() {
                std::fs::create_dir_all(padre)?;
            }
            if std::fs::read_to_string(&ruta).ok().as_deref() != Some(texto) {
                std::fs::write(&ruta, texto)?;
            }
        }
        let traductor = Arc::new(Traductor::new(
            Arc::new(EntornoEscritorio { escritorio }),
            EstadoInicial::default(),
        ));
        let escucha = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", escucha.local_addr()?);
        // The stand-in re-reads this when the address stops answering.
        let fichero_motor = dir.join("motor.json");
        std::fs::write(&fichero_motor, serde_json::json!({ "api": url }).to_string())?;
        let bots = Arc::new(Self {
            traductor: Arc::clone(&traductor),
            url,
            shim,
            fichero_motor,
            tokens: Mutex::new(HashMap::new()),
        });
        {
            let bots = Arc::clone(&bots);
            tokio::spawn(async move {
                while let Ok((conexion, _)) = escucha.accept().await {
                    let bots = Arc::clone(&bots);
                    tokio::spawn(async move { bots.atender(conexion).await });
                }
            });
        }
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(TICK).await;
                traductor.tick().await;
            }
        });
        Ok(bots)
    }

    /// The environment a bot of `sistema` starts with to trade on MT5.
    pub fn entorno(&self, sistema: &str, python_path: Option<&str>) -> Vec<(String, String)> {
        let token = {
            let mut tokens = lock(&self.tokens);
            match tokens.iter().find(|(_, s)| s.as_str() == sistema) {
                Some((token, _)) => token.clone(),
                None => {
                    let token = format!(
                        "{}{}",
                        uuid::Uuid::new_v4().simple(),
                        uuid::Uuid::new_v4().simple()
                    );
                    tokens.insert(token.clone(), sistema.to_string());
                    token
                }
            }
        };
        let mut ruta = self.shim.to_string_lossy().into_owned();
        if let Some(extra) = python_path.filter(|p| !p.is_empty()) {
            ruta = format!("{ruta};{extra}");
        }
        vec![
            ("PYTHONPATH".into(), ruta),
            ("PYTHONIOENCODING".into(), "utf-8".into()),
            ("OPENBOT_BOT_TOKEN".into(), token),
            ("OPENBOT_ENGINE_URL".into(), self.url.clone()),
            ("OPENBOT_ENGINE_FILE".into(), self.fichero_motor.to_string_lossy().into_owned()),
            ("OPENBOT_SYSTEM_ID".into(), sistema.into()),
        ]
    }

    /// A stopped bot's resting orders go (the translator's own bookkeeping).
    pub async fn sistema_parado(&self, sistema: &str) {
        self.traductor.sistema_parado(sistema).await;
    }

    async fn atender(&self, conexion: TcpStream) {
        let (lector, mut escritor) = conexion.into_split();
        let mut lector = BufReader::new(lector);
        let mut linea = String::new();
        if lector.read_line(&mut linea).await.is_err() {
            return;
        }
        let mut partes = linea.split_whitespace();
        let metodo = partes.next().unwrap_or("").to_string();
        let ruta = partes.next().unwrap_or("").split('?').next().unwrap_or("").to_string();
        let mut largo = 0usize;
        let mut token = String::new();
        loop {
            let mut cabecera = String::new();
            match lector.read_line(&mut cabecera).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let cabecera = cabecera.trim_end();
            if cabecera.is_empty() {
                break;
            }
            if let Some((nombre, valor)) = cabecera.split_once(':') {
                let nombre = nombre.trim().to_ascii_lowercase();
                if nombre == "content-length" {
                    largo = valor.trim().parse().unwrap_or(0);
                } else if nombre == "x-openbot-bot" {
                    token = valor.trim().to_string();
                }
            }
        }
        let (codigo, cuerpo) = if largo > MAX_BODY {
            (413, serde_json::json!({ "error": "petición demasiado grande" }))
        } else {
            let mut texto = vec![0u8; largo];
            if lector.read_exact(&mut texto).await.is_err() {
                return;
            }
            let sistema = (token.len() >= 16)
                .then(|| lock(&self.tokens).get(&token).cloned())
                .flatten();
            match sistema {
                None => (401, serde_json::json!({ "error": "token de bot inválido" })),
                Some(sistema) => self.ruta(&metodo, &ruta, &sistema, &texto).await,
            }
        };
        let cuerpo = cuerpo.to_string();
        let respuesta = format!(
            "HTTP/1.1 {codigo} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{cuerpo}",
            if codigo == 200 { "OK" } else { "Error" },
            cuerpo.len()
        );
        let _ = escritor.write_all(respuesta.as_bytes()).await;
        let _ = escritor.shutdown().await;
    }

    async fn ruta(&self, metodo: &str, ruta: &str, sistema: &str, texto: &[u8]) -> (u16, serde_json::Value) {
        fn respuesta<T: serde::Serialize>(resultado: Resultado<T>) -> (u16, serde_json::Value) {
            match resultado {
                Ok(valor) => (200, serde_json::to_value(valor).unwrap_or(serde_json::Value::Null)),
                Err(error) => (409, serde_json::json!({ "error": error.0 })),
            }
        }
        fn leer<T: serde::de::DeserializeOwned>(texto: &[u8]) -> Result<T, (u16, serde_json::Value)> {
            serde_json::from_slice(texto)
                .map_err(|e| (409, serde_json::json!({ "error": format!("petición ilegible: {e}") })))
        }
        let t = &self.traductor;
        match (metodo, ruta) {
            ("GET", "/bot/hello") => respuesta(t.hello(sistema).await),
            ("POST", "/bot/qualify") => match leer::<PeticionContrato>(texto) {
                Ok(p) => respuesta(t.qualify(sistema, p).await),
                Err(e) => e,
            },
            ("POST", "/bot/bars") => match leer::<PeticionVelas>(texto) {
                Ok(p) => respuesta(t.bars(sistema, p.con_id, p.end, &p.duration, &p.bar_size).await),
                Err(e) => e,
            },
            ("POST", "/bot/order") => match leer::<PeticionOrden>(texto) {
                Ok(p) => respuesta(t.place(sistema, p).await),
                Err(e) => e,
            },
            ("POST", "/bot/cancel") => match leer::<PeticionCancelar>(texto) {
                Ok(p) => respuesta(t.cancel(sistema, p.order_id).await),
                Err(e) => e,
            },
            ("GET", "/bot/orders") => {
                let propias: Vec<_> =
                    t.ordenes().await.into_iter().filter(|o| o.system == sistema).collect();
                (200, serde_json::to_value(propias).unwrap_or_default())
            }
            ("GET", "/bot/fills") => {
                let propias: Vec<_> =
                    t.ejecuciones().await.into_iter().filter(|e| e.system == sistema).collect();
                (200, serde_json::to_value(propias).unwrap_or_default())
            }
            ("GET", "/bot/positions") => (200, serde_json::to_value(t.positions(sistema).await).unwrap_or_default()),
            ("GET", "/bot/account") => {
                (200, serde_json::to_value(t.account_summary(sistema).await).unwrap_or_default())
            }
            _ => (404, serde_json::json!({ "error": format!("ruta desconocida: {metodo} {ruta}") })),
        }
    }
}
