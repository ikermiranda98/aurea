//! The translator's environment: the running bridges, the systems on each and
//! where the translator keeps state.
//!
//! [`EnlaceMt5`] owns one [`PuenteMt5`] and mirrors its published book, account
//! and status into plain caches, because [`EntornoMt5::libro`] is synchronous
//! (the translator reads it while holding its own lock, never `.await`ing
//! between a decision and the command that applies it).
//!
//! [`Enlaces`] is the [`EntornoMt5`] the engine hands to [`Traductor`]: which
//! link each system trades on, its magic number, the symbol `system.json`
//! insisted on, and where orders, fills and instruments are persisted.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;

use super::instruments::{magic_por_defecto, Instrumento};
use super::libro::{CuentaMt5, LibroMt5, ResultadoMt5};
use super::ordenes::{Ejecucion, EntornoMt5, Nivel, OrdenVirtual};
use super::puente_cliente::PuenteMt5;

/// How often the mirror copies what the bridge published.
const REFRESCO: Duration = Duration::from_millis(250);

/// Where the translator keeps what must survive a restart.
///
/// The engine implements it on top of A1's database; [`SinAlmacen`] is the
/// in-memory default so the translator also runs in probes and tests.
pub trait AlmacenMt5: Send + Sync {
    fn orden(&self, _orden: &OrdenVirtual) {}
    fn ejecucion(&self, _ejecucion: &Ejecucion) {}
    fn instrumentos(&self, _lista: &[Instrumento]) {}
    fn log(&self, _nivel: Nivel, _sistema: &str, _mensaje: &str) {}
}

/// Nothing is persisted (the translator still works while it runs).
pub struct SinAlmacen;

impl AlmacenMt5 for SinAlmacen {}

fn leer<T: Clone>(lock: &RwLock<T>) -> T {
    match lock.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

fn escribir<T>(lock: &RwLock<T>, valor: T) {
    match lock.write() {
        Ok(mut guard) => *guard = valor,
        Err(poisoned) => *poisoned.into_inner() = valor,
    }
}

/// What identifies and configures one MT5 link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguracionMt5 {
    pub login: i64,
    pub servidor: String,
    /// The exact symbol `system.json` asked for, when it did.
    pub simbolo: Option<String>,
    /// The link's magic override (the system's own wins over it).
    pub magic: Option<i64>,
}

impl ConfiguracionMt5 {
    /// The link id the dashboard and the systems use: `mt5-<login>@<server>`.
    pub fn id(&self) -> String {
        format!("mt5-{}@{}", self.login, self.servidor)
    }
}

/// One running bridge plus the last thing it said.
pub struct EnlaceMt5 {
    pub id: String,
    pub login: i64,
    pub servidor: String,
    pub simbolo: Option<String>,
    pub magic: Option<i64>,
    puente: Arc<PuenteMt5>,
    libro: Arc<RwLock<Option<LibroMt5>>>,
    cuenta: Arc<RwLock<Option<CuentaMt5>>>,
    motivo: Arc<RwLock<Option<String>>>,
    /// The bridge process ended: this link reads nothing until it is rebuilt.
    terminado: Arc<AtomicBool>,
}

impl EnlaceMt5 {
    /// Starts the Python bridge for this account and mirrors its output.
    pub fn arrancar(
        ejecutable: &Path,
        script: &Path,
        desfase: &Path,
        config: ConfiguracionMt5,
        variables: &[(String, String)],
    ) -> std::io::Result<Self> {
        let puente = Arc::new(PuenteMt5::arrancar(
            ejecutable,
            script,
            &config.login.to_string(),
            Some(&config.servidor),
            desfase,
            variables,
        )?);
        let libro = Arc::new(RwLock::new(None));
        let cuenta = Arc::new(RwLock::new(None));
        let motivo = Arc::new(RwLock::new(None));
        let terminado = Arc::new(AtomicBool::new(false));

        {
            let terminado = Arc::clone(&terminado);
            let puente = Arc::clone(&puente);
            let libro = Arc::clone(&libro);
            let cuenta = Arc::clone(&cuenta);
            let motivo = Arc::clone(&motivo);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(REFRESCO).await;
                    if !puente.vivo().await {
                        escribir(
                            &motivo,
                            Some("el puente de MetaTrader 5 terminó".to_string()),
                        );
                        terminado.store(true, Ordering::SeqCst);
                        return;
                    }
                    if let Some(leido) = puente.libro().await {
                        escribir(&libro, Some(leido));
                    }
                    if let Some(leida) = puente.cuenta().await {
                        escribir(&cuenta, Some(leida));
                    }
                    escribir(&motivo, puente.motivo().await);
                }
            });
        }

        Ok(Self {
            id: config.id(),
            login: config.login,
            servidor: config.servidor,
            simbolo: config.simbolo,
            magic: config.magic,
            puente,
            libro,
            cuenta,
            motivo,
            terminado,
        })
    }

    /// Whether the bridge process ended (the desk starts a new one).
    pub fn terminado(&self) -> bool {
        self.terminado.load(Ordering::SeqCst)
    }

    /// The link's last book; `None` while the bridge reads nothing.
    pub fn libro(&self) -> Option<LibroMt5> {
        leer(&self.libro)
    }

    /// The last `cuenta` line.
    pub fn cuenta(&self) -> Option<CuentaMt5> {
        leer(&self.cuenta)
    }

    /// Why the bridge is not attached, when it is not.
    pub fn motivo(&self) -> Option<String> {
        leer(&self.motivo)
    }

    /// The bridge, for one-shot commands outside the translator (manual close).
    pub fn puente(&self) -> &Arc<PuenteMt5> {
        &self.puente
    }

    /// Stops the bridge for good.
    pub async fn parar(&self) {
        self.puente.parar().await;
    }
}

/// The links, which systems trade on them, and the store.
pub struct Enlaces {
    enlaces: BTreeMap<String, Arc<EnlaceMt5>>,
    sistemas: BTreeMap<String, String>,
    almacen: Arc<dyn AlmacenMt5>,
}

impl Enlaces {
    pub fn nuevo(almacen: Arc<dyn AlmacenMt5>) -> Self {
        Self {
            enlaces: BTreeMap::new(),
            sistemas: BTreeMap::new(),
            almacen,
        }
    }

    /// Adds (or replaces) a running link.
    pub fn añadir(&mut self, enlace: EnlaceMt5) {
        self.enlaces.insert(enlace.id.clone(), Arc::new(enlace));
    }

    /// Points a system at a link.
    pub fn asignar(&mut self, sistema: impl Into<String>, enlace: impl Into<String>) {
        self.sistemas.insert(sistema.into(), enlace.into());
    }

    /// A link by id.
    pub fn enlace(&self, id: &str) -> Option<Arc<EnlaceMt5>> {
        self.enlaces.get(id).cloned()
    }

    /// Every running link.
    pub fn lista(&self) -> Vec<Arc<EnlaceMt5>> {
        self.enlaces.values().cloned().collect()
    }

    /// The link a system trades on, if it has one.
    pub fn enlace_de_sistema(&self, sistema: &str) -> Option<Arc<EnlaceMt5>> {
        let id = self.sistemas.get(sistema)?;
        self.enlaces.get(id).cloned()
    }

    /// Stops every bridge (engine shutdown).
    pub async fn parar(&self) {
        for enlace in self.enlaces.values() {
            enlace.parar().await;
        }
    }
}

#[async_trait]
impl EntornoMt5 for Enlaces {
    fn enlace_de(&self, sistema: &str) -> std::result::Result<String, String> {
        match self.sistemas.get(sistema) {
            Some(id) if self.enlaces.contains_key(id) => Ok(id.clone()),
            _ => Err(format!("el sistema {sistema} no opera en MetaTrader 5")),
        }
    }

    fn libro(&self, enlace: &str) -> Option<LibroMt5> {
        self.enlaces.get(enlace)?.libro()
    }

    async fn comando(&self, enlace: &str, verbo: &str, arg: &str) -> ResultadoMt5 {
        match self.enlaces.get(enlace) {
            Some(enlace) => enlace.puente.comando(verbo, arg).await,
            None => ResultadoMt5::fallo(-1, format!("el enlace {enlace} no está abierto")),
        }
    }

    fn magic(&self, sistema: &str) -> i64 {
        self.enlace_de_sistema(sistema)
            .and_then(|enlace| enlace.magic)
            .unwrap_or_else(|| magic_por_defecto(sistema))
    }

    fn simbolo(&self, sistema: &str) -> Option<String> {
        self.enlace_de_sistema(sistema)
            .and_then(|enlace| enlace.simbolo.clone())
    }

    fn log(&self, nivel: Nivel, sistema: &str, mensaje: &str) {
        match nivel {
            Nivel::Info => tracing::info!(target: "mt5", "sistema {sistema}: {mensaje}"),
            Nivel::Warning => tracing::warn!(target: "mt5", "sistema {sistema}: {mensaje}"),
            Nivel::Error => tracing::error!(target: "mt5", "sistema {sistema}: {mensaje}"),
        }
        self.almacen.log(nivel, sistema, mensaje);
    }

    fn guardar_orden(&self, orden: &OrdenVirtual) {
        self.almacen.orden(orden);
    }

    fn guardar_ejecucion(&self, ejecucion: &Ejecucion) {
        self.almacen.ejecucion(ejecucion);
    }

    fn guardar_instrumentos(&self, lista: &[Instrumento]) {
        self.almacen.instrumentos(lista);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recuerdo {
        ordenes: Mutex<Vec<i64>>,
        ejecuciones: Mutex<Vec<String>>,
        instrumentos: Mutex<Vec<i64>>,
        logs: Mutex<Vec<String>>,
    }

    impl AlmacenMt5 for Recuerdo {
        fn orden(&self, orden: &OrdenVirtual) {
            self.ordenes.lock().unwrap().push(orden.order_id);
        }

        fn ejecucion(&self, ejecucion: &Ejecucion) {
            self.ejecuciones
                .lock()
                .unwrap()
                .push(ejecucion.exec_id.clone());
        }

        fn instrumentos(&self, lista: &[Instrumento]) {
            self.instrumentos
                .lock()
                .unwrap()
                .extend(lista.iter().map(|i| i.con_id));
        }

        fn log(&self, _nivel: Nivel, sistema: &str, mensaje: &str) {
            self.logs
                .lock()
                .unwrap()
                .push(format!("{sistema}: {mensaje}"));
        }
    }

    #[test]
    fn a_link_id_reads_like_the_dashboard_prints_it() {
        let config = ConfiguracionMt5 {
            login: 5_123_456,
            servidor: "AureaDemo".to_string(),
            simbolo: None,
            magic: None,
        };
        assert_eq!(config.id(), "mt5-5123456@AureaDemo");
    }

    #[test]
    fn a_system_without_a_link_says_so_and_keeps_its_default_magic() {
        let almacen = Arc::new(Recuerdo::default());
        let enlaces = Enlaces::nuevo(almacen);
        assert_eq!(
            enlaces.enlace_de("07").unwrap_err(),
            "el sistema 07 no opera en MetaTrader 5"
        );
        assert_eq!(enlaces.magic("07"), 7_700_007);
        assert_eq!(enlaces.simbolo("07"), None);
        assert!(enlaces.libro("mt5-1@X").is_none());
    }

    #[tokio::test]
    async fn a_command_without_its_link_fails_instead_of_hanging() {
        let enlaces = Enlaces::nuevo(Arc::new(SinAlmacen));
        let resultado = enlaces.comando("mt5-1@X", "SYMBOL", "EURUSD").await;
        assert!(!resultado.ok);
        assert_eq!(resultado.retcode, -1);
        assert!(resultado.message.contains("no está abierto"));
    }

    #[test]
    fn the_store_gets_every_order_fill_instrument_and_log() {
        let almacen = Arc::new(Recuerdo::default());
        let enlaces = Enlaces::nuevo(almacen.clone());
        let mut orden = OrdenVirtual {
            order_id: 1_000,
            perm_id: 1_000,
            system: "7".to_string(),
            client_id: 1,
            con_id: 1,
            symbol: "US100".to_string(),
            action: "BUY".to_string(),
            order_type: "MKT".to_string(),
            qty: 1.0,
            aux_price: 0.0,
            lmt_price: 0.0,
            tif: "DAY".to_string(),
            oca_group: String::new(),
            role: "market".to_string(),
            status: "Filled".to_string(),
            filled: 1.0,
            avg_fill_price: 100.0,
            ticket: 0,
            deals: vec![11],
            message: String::new(),
            account: "mt5-1@X".to_string(),
            created: 1,
            updated: 1,
            next_try: 0,
        };
        enlaces.guardar_orden(&orden);
        orden.order_id = 1_001;
        enlaces.guardar_orden(&orden);
        enlaces.guardar_ejecucion(&Ejecucion {
            exec_id: "11.1000".to_string(),
            order_id: 1_000,
            perm_id: 1_000,
            system: "7".to_string(),
            client_id: 1,
            con_id: 1,
            symbol: "US100".to_string(),
            side: "BOT".to_string(),
            shares: 1.0,
            price: 100.0,
            time: 1,
            account: "DU1".to_string(),
        });
        enlaces.guardar_instrumentos(&[super::super::instruments::Instrumento {
            con_id: 1,
            root: "MNQ".to_string(),
            sec_type: "FUT".to_string(),
            exchange: String::new(),
            currency: "USD".to_string(),
            multiplier: 2.0,
            forex: false,
            account: "mt5-1@X".to_string(),
            mt5: "US100".to_string(),
            digits: 2,
            contract_size: 1.0,
            tick_size: 0.01,
            tick_value: 0.01,
            volume_min: 0.01,
            volume_step: 0.01,
            volume_max: 100.0,
        }]);
        enlaces.log(Nivel::Info, "7", "hola");

        assert_eq!(*almacen.ordenes.lock().unwrap(), vec![1_000, 1_001]);
        assert_eq!(*almacen.ejecuciones.lock().unwrap(), vec!["11.1000"]);
        assert_eq!(*almacen.instrumentos.lock().unwrap(), vec![1]);
        assert_eq!(*almacen.logs.lock().unwrap(), vec!["7: hola"]);
    }
}
