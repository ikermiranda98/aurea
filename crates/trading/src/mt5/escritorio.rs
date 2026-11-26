//! Real MetaTrader 5 on the desk: finds a Python that can drive the terminal,
//! detects the account the running terminal is logged into, keeps one
//! [`EnlaceMt5`] bridge per registered account (remembered across restarts)
//! and turns what the bridges publish into the desk's links, accounts and
//! positions.
//!
//! The bridge (`puente.py`, embedded) needs MetaQuotes' `MetaTrader5` Python
//! package, published only for CPython 3.10–3.14 on Windows; a newer Python
//! can't install it, so the search prefers the launcher's 3.14…3.10.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zeron_proto::trading::{
    Account, Broker, Link, Mt5Detected, Position, PositionSide, ProgramState,
};

use super::enlace::{ConfiguracionMt5, EnlaceMt5};
use super::libro::{CuentaMt5, PosicionMt5};

/// The bridge, written next to the desk's data on first use.
const PUENTE_PY: &str = include_str!("puente.py");
/// How long a detection may take (the probe reads the account twice, 5 s apart).
const ESPERA_SONDA: Duration = Duration::from_secs(25);
/// The Python minors the `MetaTrader5` package exists for, newest first.
const PYTHONS: &[&str] = &["3.14", "3.13", "3.12", "3.11", "3.10"];

/// One registered account, as remembered on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CuentaRegistrada {
    pub login: i64,
    pub server: String,
}

/// What the desk shows of MetaTrader 5 right now.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VistaMt5 {
    pub links: Vec<Link>,
    pub accounts: Vec<Account>,
    pub positions: Vec<Position>,
    /// `Some(Running)` while a bridge is attached to a terminal.
    pub programa: Option<ProgramState>,
    /// Why a registered account isn't reading, per link id.
    pub motivos: Vec<(String, String)>,
}

/// The desk's MetaTrader 5 side.
pub struct Mt5Escritorio {
    dir: PathBuf,
    enlaces: Mutex<BTreeMap<String, Arc<EnlaceMt5>>>,
    python: Mutex<Option<PathBuf>>,
    /// The bots' API (`/bot/*`), once started.
    bots: std::sync::OnceLock<Arc<super::bots::BotsMt5>>,
    /// What the translator said about each bot, newest last (the desk log).
    notas: Mutex<Vec<NotaBot>>,
    /// Each account's positions at the last look, to log opens and closes.
    posiciones: Mutex<BTreeMap<String, BTreeMap<i64, PosicionMt5>>>,
}

/// One translator line for the desk's log.
#[derive(Debug, Clone, PartialEq)]
pub struct NotaBot {
    pub ts: i64,
    pub nivel: super::ordenes::Nivel,
    pub sistema: String,
    pub mensaje: String,
}

/// Who a position belongs to in the log: a system (by its default magic),
/// the person (magic 0) or the desk.
fn autor_posicion(magic: i64) -> String {
    match magic {
        0 => "manual".to_string(),
        m if (7_700_001..7_701_000).contains(&m) => format!("{:02}", m - 7_700_000),
        _ => "mesa".to_string(),
    }
}

fn lado_posicion(side: &str) -> &'static str {
    if side == "short" { "venta" } else { "compra" }
}

/// How many translator lines the desk keeps.
const NOTAS_MAX: usize = 200;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn sin_ventana(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// `exe -c code` finished well, with its trimmed stdout.
fn ejecutar(exe: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new(exe);
    command.args(args).stdin(std::process::Stdio::null());
    sin_ventana(&mut command);
    let salida = command.output().ok()?;
    salida
        .status
        .success()
        .then(|| String::from_utf8_lossy(&salida.stdout).trim().to_string())
}

impl Mt5Escritorio {
    /// The desk's MT5 side, keeping its files under `trading_dir/mt5`.
    pub fn new(trading_dir: &Path) -> Arc<Self> {
        Arc::new(Self {
            dir: trading_dir.join("mt5"),
            enlaces: Mutex::new(BTreeMap::new()),
            python: Mutex::new(None),
            bots: std::sync::OnceLock::new(),
            notas: Mutex::new(Vec::new()),
            posiciones: Mutex::new(BTreeMap::new()),
        })
    }

    fn registro(&self) -> PathBuf {
        self.dir.join("cuentas.json")
    }

    /// The accounts registered on this desk.
    pub fn registradas(&self) -> Vec<CuentaRegistrada> {
        std::fs::read_to_string(self.registro())
            .ok()
            .and_then(|texto| serde_json::from_str(&texto).ok())
            .unwrap_or_default()
    }

    fn guardar(&self, cuentas: &[CuentaRegistrada]) {
        let _ = std::fs::create_dir_all(&self.dir);
        if let Ok(texto) = serde_json::to_string_pretty(cuentas) {
            let _ = std::fs::write(self.registro(), texto);
        }
    }

    fn script(&self) -> Result<PathBuf, String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let ruta = self.dir.join("puente.py");
        if std::fs::read_to_string(&ruta).ok().as_deref() != Some(PUENTE_PY) {
            std::fs::write(&ruta, PUENTE_PY).map_err(|e| e.to_string())?;
        }
        Ok(ruta)
    }

    /// A Python that has the `MetaTrader5` package, or what to install.
    /// `AUREA_MT5_PYTHON` names one explicitly.
    pub fn python(&self) -> Result<PathBuf, String> {
        if let Some(found) = lock(&self.python).clone() {
            return Ok(found);
        }
        let mut candidatos: Vec<PathBuf> = Vec::new();
        if let Some(explicito) = std::env::var_os("AUREA_MT5_PYTHON") {
            candidatos.push(PathBuf::from(explicito));
        }
        // The installer's own Python (with MetaTrader5), beside zeron.exe.
        if let Some(propio) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("mt5-python").join("python.exe")))
            .filter(|exe| exe.is_file())
        {
            candidatos.push(propio);
        }
        for version in PYTHONS {
            let flag = format!("-{version}");
            if let Some(exe) = ejecutar(
                Path::new("py"),
                &[&flag, "-c", "import sys; print(sys.executable)"],
            ) {
                candidatos.push(PathBuf::from(exe));
            }
        }
        for nombre in ["python", "python3"] {
            candidatos.push(PathBuf::from(nombre));
        }
        let mut compatible: Option<PathBuf> = None;
        let mut otra: Option<String> = None;
        for exe in candidatos {
            let Some(version) = ejecutar(
                &exe,
                &["-c", "import sys; print('%d.%d' % sys.version_info[:2])"],
            ) else {
                continue;
            };
            if !PYTHONS.contains(&version.as_str()) {
                otra.get_or_insert(version);
                continue;
            }
            if ejecutar(&exe, &["-c", "import MetaTrader5"]).is_some() {
                *lock(&self.python) = Some(exe.clone());
                return Ok(exe);
            }
            compatible.get_or_insert(exe);
        }
        Err(match (compatible, otra) {
            (Some(exe), _) => format!(
                "Falta el paquete de Python MetaTrader5. Instálalo con: \"{}\" -m pip install MetaTrader5",
                exe.display()
            ),
            (None, Some(version)) => format!(
                "MetaTrader 5 necesita Python 3.10 a 3.14 (el paquete MetaTrader5 aún no existe para tu Python {version}). Instala Python 3.13 desde python.org y después: py -3.13 -m pip install MetaTrader5"
            ),
            (None, None) => "MetaTrader 5 necesita Python 3.13 con el paquete MetaTrader5: instala Python 3.13 desde python.org y después: py -3.13 -m pip install MetaTrader5".into(),
        })
    }

    /// The account the running terminal is logged into.
    pub async fn detectar(self: &Arc<Self>) -> Result<Vec<Mt5Detected>, String> {
        let this = Arc::clone(self);
        tokio::task::spawn_blocking(move || this.detectar_bloqueante())
            .await
            .map_err(|_| "la detección de MetaTrader 5 se interrumpió".to_string())?
    }

    fn detectar_bloqueante(&self) -> Result<Vec<Mt5Detected>, String> {
        let python = self.python()?;
        let script = self.script()?;
        let desfase = self.dir.join("desfase-sonda.json");
        let mut command = Command::new(&python);
        command
            .arg(&script)
            .arg("--probe")
            .arg("--desfase")
            .arg(&desfase)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        sin_ventana(&mut command);
        let mut hijo = command.spawn().map_err(|e| format!("no se pudo lanzar Python: {e}"))?;
        let inicio = std::time::Instant::now();
        loop {
            match hijo.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if inicio.elapsed() < ESPERA_SONDA => {
                    std::thread::sleep(Duration::from_millis(200))
                }
                _ => {
                    let _ = hijo.kill();
                    break;
                }
            }
        }
        let salida = hijo
            .wait_with_output()
            .map_err(|e| format!("no se pudo leer la sonda: {e}"))?;
        let texto = String::from_utf8_lossy(&salida.stdout);
        let mut motivo: Option<String> = None;
        let mut cuentas: Vec<Mt5Detected> = Vec::new();
        for linea in texto.lines() {
            let Ok(valor) = serde_json::from_str::<serde_json::Value>(linea) else {
                continue;
            };
            match valor.get("type").and_then(|v| v.as_str()) {
                Some("cuenta") => {
                    if let Ok(cuenta) = serde_json::from_value::<CuentaMt5>(valor) {
                        if !cuentas.iter().any(|c| c.login == cuenta.login && c.server == cuenta.server) {
                            cuentas.push(Mt5Detected {
                                login: cuenta.login,
                                server: cuenta.server,
                                name: (!cuenta.name.is_empty()).then_some(cuenta.name),
                                active: true,
                                live: true,
                            });
                        }
                    }
                }
                Some("estado") => {
                    motivo = valor
                        .get("motivo")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                }
                _ => {}
            }
        }
        if cuentas.is_empty() {
            return Err(motivo.unwrap_or_else(|| {
                "MetaTrader 5 no respondió: ábrelo, inicia sesión en tu cuenta y vuelve a intentarlo".into()
            }));
        }
        Ok(cuentas)
    }

    /// Registers an account and starts its bridge (needs a tokio runtime).
    pub fn abrir(&self, login: i64, server: &str) -> Result<Link, String> {
        let server = server.trim();
        if login <= 0 || server.is_empty() {
            return Err("La conexión MT5 necesita el login y el servidor de la cuenta".into());
        }
        let config = ConfiguracionMt5 {
            login,
            servidor: server.to_string(),
            simbolo: None,
            magic: None,
        };
        let id = config.id();
        if !lock(&self.enlaces).contains_key(&id) {
            let python = self.python()?;
            let script = self.script()?;
            let desfase = self.dir.join(format!("desfase-{login}.json"));
            let enlace = EnlaceMt5::arrancar(&python, &script, &desfase, config, &[])
                .map_err(|e| format!("no se pudo arrancar el puente de MetaTrader 5: {e}"))?;
            lock(&self.enlaces).insert(id.clone(), Arc::new(enlace));
        }
        let mut cuentas = self.registradas();
        if !cuentas.iter().any(|c| c.login == login && c.server == server) {
            cuentas.push(CuentaRegistrada { login, server: server.to_string() });
            self.guardar(&cuentas);
        }
        Ok(Link {
            id,
            host: None,
            port: None,
            label: Some(format!("MT5 {login}")),
            broker: Some(Broker::Mt5),
            mt5_login: Some(login),
            mt5_server: Some(server.to_string()),
            connected: false,
        })
    }

    /// Stops an account's bridge and forgets it. `false` when it wasn't an MT5 link.
    pub async fn quitar(&self, id: &str) -> bool {
        let enlace = lock(&self.enlaces).remove(id);
        let cuentas: Vec<CuentaRegistrada> = self
            .registradas()
            .into_iter()
            .filter(|c| format!("mt5-{}@{}", c.login, c.server) != id)
            .collect();
        let existia = enlace.is_some() || cuentas.len() != self.registradas().len();
        self.guardar(&cuentas);
        if let Some(enlace) = enlace {
            enlace.parar().await;
        }
        existia
    }

    /// Starts the bridges of every registered account (engine boot).
    pub fn restaurar(&self) -> Vec<String> {
        let mut fallos = Vec::new();
        for cuenta in self.registradas() {
            if let Err(error) = self.abrir(cuenta.login, &cuenta.server) {
                fallos.push(format!("MT5 {}@{}: {error}", cuenta.login, cuenta.server));
            }
        }
        fallos
    }

    /// Where the bots' API keeps its files.
    pub fn dir_bots(&self) -> PathBuf {
        self.dir.join("bots")
    }

    /// Hands over the started bots' API.
    pub fn set_bots(&self, bots: Arc<super::bots::BotsMt5>) {
        let _ = self.bots.set(bots);
    }

    /// The MT5 account the bots trade on: the first registered one whose
    /// bridge reads its account.
    pub fn enlace_para_bots(&self) -> Option<String> {
        let enlaces = lock(&self.enlaces);
        self.registradas()
            .into_iter()
            .map(|c| format!("mt5-{}@{}", c.login, c.server))
            .find(|id| {
                enlaces
                    .get(id)
                    .is_some_and(|e| e.cuenta().is_some() && e.motivo().is_none())
            })
    }

    /// The environment a bot starts with to trade on MT5: `None` while no
    /// MT5 account is registered (the bot goes to TWS as always), an error
    /// when one is registered but the bot can't reach it.
    pub fn entorno_bot(&self, sistema: &str, carpeta: &Path) -> Option<Result<Vec<(String, String)>, String>> {
        if self.registradas().is_empty() {
            return None;
        }
        let Some(bots) = self.bots.get() else {
            return Some(Err("la API de los bots de MetaTrader 5 aún no está lista; inténtalo en unos segundos".into()));
        };
        let Some(enlace) = self.enlace_para_bots() else {
            return Some(Err(
                "la cuenta de MetaTrader 5 no está conectada: abre MetaTrader 5 con tu sesión iniciada".into(),
            ));
        };
        let mut entorno = bots.entorno(sistema, Some(&carpeta.to_string_lossy()));
        entorno.push(("OPENBOT_MT5_LINK".into(), enlace));
        Some(Ok(entorno))
    }

    /// Notes one translator line for the desk's log.
    pub fn anotar(&self, nivel: super::ordenes::Nivel, sistema: &str, mensaje: &str) {
        let mut notas = lock(&self.notas);
        notas.push(NotaBot {
            ts: chrono::Utc::now().timestamp_millis(),
            nivel,
            sistema: sistema.to_string(),
            mensaje: mensaje.to_string(),
        });
        let sobran = notas.len().saturating_sub(NOTAS_MAX);
        notas.drain(..sobran);
    }

    /// Logs each position that appeared or went since the last look, one
    /// placed by hand included. The first look at an account only remembers
    /// what it holds.
    fn vigilar_posiciones(&self, enlaces: &BTreeMap<String, Arc<EnlaceMt5>>) {
        let mut vistas = lock(&self.posiciones);
        for (id, enlace) in enlaces {
            let Some(libro) = enlace.libro() else { continue };
            let ahora: BTreeMap<i64, PosicionMt5> =
                libro.positions.iter().map(|p| (p.ticket, p.clone())).collect();
            if let Some(antes) = vistas.get(id) {
                for p in ahora.values().filter(|p| !antes.contains_key(&p.ticket)) {
                    self.anotar(
                        super::ordenes::Nivel::Info,
                        &autor_posicion(p.magic),
                        &format!(
                            "Posición abierta en MT5 {}: {} {} {} a {}",
                            libro.login,
                            lado_posicion(&p.side),
                            p.volume,
                            p.symbol,
                            p.entry
                        ),
                    );
                }
                for p in antes.values().filter(|p| !ahora.contains_key(&p.ticket)) {
                    let resultado = libro
                        .deals
                        .as_ref()
                        .map(|deals| {
                            deals
                                .iter()
                                .filter(|d| d.position == p.ticket && d.entry != "in")
                                .map(|d| d.pnl)
                                .sum::<f64>()
                        })
                        .map(|pnl| format!(", resultado {pnl:+.2}"))
                        .unwrap_or_default();
                    self.anotar(
                        super::ordenes::Nivel::Info,
                        &autor_posicion(p.magic),
                        &format!(
                            "Posición cerrada en MT5 {}: {} {} {}{resultado}",
                            libro.login,
                            lado_posicion(&p.side),
                            p.volume,
                            p.symbol
                        ),
                    );
                }
            }
            vistas.insert(id.clone(), ahora);
        }
    }

    /// The translator's lines, oldest first.
    pub fn notas(&self) -> Vec<NotaBot> {
        lock(&self.notas).clone()
    }

    pub fn hay_enlaces(&self) -> bool {
        !lock(&self.enlaces).is_empty() || !self.registradas().is_empty()
    }

    /// The bridge of a link, for commands (manual close).
    pub fn enlace(&self, id: &str) -> Option<Arc<EnlaceMt5>> {
        lock(&self.enlaces).get(id).cloned()
    }

    /// Which link holds an open position on `symbol`, if any.
    pub fn enlace_con_posicion(&self, symbol: &str) -> Option<Arc<EnlaceMt5>> {
        lock(&self.enlaces)
            .values()
            .find(|enlace| {
                enlace
                    .cuenta()
                    .is_some_and(|c| c.positions.iter().any(|p| p.symbol == symbol))
            })
            .cloned()
    }

    /// Closes every MT5 position on `symbol` (the account holding it).
    /// `None` when no MT5 account holds that symbol.
    pub async fn cerrar_posicion(&self, symbol: &str) -> Option<super::manual::RespuestaMt5> {
        let enlace = self.enlace_con_posicion(symbol)?;
        let python = match self.python() {
            Ok(python) => python,
            Err(error) => return Some(super::manual::RespuestaMt5::mal(error)),
        };
        let peticion =
            super::manual::PeticionCierre::cerrar(enlace.login, enlace.servidor.clone(), symbol);
        Some(super::manual::cerrar_manual(&python, &peticion).await)
    }

    /// Closes every open MT5 position on every account; one line per symbol.
    pub async fn cerrar_todo(&self) -> (bool, Vec<String>) {
        let simbolos: Vec<String> = {
            let enlaces = lock(&self.enlaces).clone();
            let mut lista: Vec<String> = enlaces
                .values()
                .filter_map(|e| e.cuenta())
                .flat_map(|c| c.positions.into_iter().map(|p| p.symbol))
                .collect();
            lista.sort();
            lista.dedup();
            lista
        };
        let mut todo_bien = true;
        let mut lineas = Vec::new();
        for simbolo in simbolos {
            if let Some(respuesta) = self.cerrar_posicion(&simbolo).await {
                todo_bien &= respuesta.ok;
                lineas.push(format!("{simbolo}: {}", respuesta.message));
            }
        }
        (todo_bien, lineas)
    }

    /// What the desk shows: every registered account, read or not.
    pub fn vista(&self) -> VistaMt5 {
        let mut vista = VistaMt5::default();
        let enlaces = lock(&self.enlaces).clone();
        self.vigilar_posiciones(&enlaces);
        for cuenta in self.registradas() {
            let id = format!("mt5-{}@{}", cuenta.login, cuenta.server);
            let enlace = enlaces.get(&id);
            let leida = enlace.and_then(|e| e.cuenta());
            let motivo = match enlace {
                None => Some("el puente de MetaTrader 5 no está en marcha".to_string()),
                Some(e) => e.motivo(),
            };
            let conectado = leida.is_some() && motivo.is_none();
            // Unread, the label says why (the desk shows it on the link).
            let label = match (&motivo, conectado) {
                (Some(motivo), false) => format!("MT5 {} · {motivo}", cuenta.login),
                _ => format!("MT5 {}", cuenta.login),
            };
            vista.links.push(Link {
                id: id.clone(),
                host: None,
                port: None,
                label: Some(label),
                broker: Some(Broker::Mt5),
                mt5_login: Some(cuenta.login),
                mt5_server: Some(cuenta.server.clone()),
                connected: conectado,
            });
            if conectado {
                vista.programa = Some(ProgramState::Running);
            }
            if let Some(motivo) = motivo {
                vista.motivos.push((id.clone(), motivo));
            }
            let Some(leida) = leida else { continue };
            vista.accounts.push(Account {
                id: id.clone(),
                kind: format!("MT5 · {} · {}", leida.login, leida.server),
                cash: leida.balance,
                equity: leida.equity,
                pnl_day: leida.pnl_day,
                pnl_realized: leida.pnl_realized,
                available_funds: leida.margin_free,
                demo: Some(leida.trade_mode != 2),
                buying_power: None,
                margin_used: leida.margin_used,
                link: Some(id.clone()),
                ..cuenta_vacia(&id)
            });
            for posicion in &leida.positions {
                let size = posicion.size.unwrap_or(0.0);
                let pnl_pct = match (posicion.entry, posicion.current) {
                    (Some(entry), Some(current)) if entry != 0.0 => {
                        let cambio = (current - entry) / entry * 100.0;
                        Some(if posicion.side == "short" { -cambio } else { cambio })
                    }
                    _ => None,
                };
                vista.positions.push(Position {
                    symbol: posicion.symbol.clone(),
                    side: if posicion.side == "short" {
                        PositionSide::Short
                    } else {
                        PositionSide::Long
                    },
                    size,
                    entry: posicion.entry,
                    current: posicion.current,
                    pnl: posicion.pnl,
                    pnl_pct,
                });
            }
        }
        vista
    }
}

/// An account with only its id: the base the MT5 fields fill.
fn cuenta_vacia(id: &str) -> Account {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "type": "MT5",
        "cash": null,
        "equity": null,
        "pnlDay": null,
        "pnlRealized": null,
    }))
    .expect("a minimal account parses")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_accounts_survive_and_show_unread() {
        let dir = tempfile::tempdir().unwrap();
        let escritorio = Mt5Escritorio::new(dir.path());
        escritorio.guardar(&[CuentaRegistrada { login: 123, server: "Broker-Demo".into() }]);
        let otro = Mt5Escritorio::new(dir.path());
        assert_eq!(otro.registradas().len(), 1);
        let vista = otro.vista();
        assert_eq!(vista.links.len(), 1);
        assert_eq!(vista.links[0].id, "mt5-123@Broker-Demo");
        assert!(!vista.links[0].connected);
        assert!(vista.accounts.is_empty());
        assert_eq!(vista.motivos.len(), 1);
    }

    #[tokio::test]
    async fn removing_forgets_the_account() {
        let dir = tempfile::tempdir().unwrap();
        let escritorio = Mt5Escritorio::new(dir.path());
        escritorio.guardar(&[CuentaRegistrada { login: 7, server: "S".into() }]);
        assert!(escritorio.quitar("mt5-7@S").await);
        assert!(escritorio.registradas().is_empty());
        assert!(!escritorio.quitar("127.0.0.1:7497").await);
    }

    #[test]
    fn a_link_needs_login_and_server() {
        let dir = tempfile::tempdir().unwrap();
        let escritorio = Mt5Escritorio::new(dir.path());
        assert!(escritorio.abrir(0, "S").is_err());
        assert!(escritorio.abrir(5, "  ").is_err());
    }
}
