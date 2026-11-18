//! Bot lifecycle primitives for Aurea (lane A2).
//!
//! Reusable building blocks ported from Openbot's `packages/core/src/trading.ts`
//! and `trading/servidor-bots.ts`: the per-system [`Runtime`], Windows process
//! helpers, the in-flight start guard, the MT5 bot HTTP server scaffolding and
//! the stop decision flow. Full engine wiring (spawn/adopt/sweep/ingest) lives
//! elsewhere; this module keeps the pure, testable parts.

use std::collections::{HashMap, HashSet};
use std::path::Path;use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// Where a running bot came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSource {
    Panel,
    External,
}

/// Per-system runtime state (port of `Runtime` in `trading.ts`).
#[derive(Debug, Clone)]
pub struct Runtime {
    /// `"panel"` / `"external"` / `None` (never started or stopped).
    pub source: Option<RuntimeSource>,
    pub pid: Option<u32>,
    pub started_at: Option<i64>,
    /// Placeholder for the spawned child process handle.
    pub child: Option<ChildHandle>,
    pub planeado: bool,
    /// Whether the "could not verify" warning has already been emitted.
    pub sondeo_dudado: bool,
    pub registro_offset: u64,
    pub consola_offset: u64,
    pub ultima_actividad: Option<i64>,
}

/// Placeholder representation of a spawned child process handle.
#[derive(Debug, Clone, Default)]
pub struct ChildHandle {
    pub pid: Option<u32>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            source: None,
            pid: None,
            started_at: None,
            child: None,
            planeado: false,
            sondeo_dudado: false,
            registro_offset: 0,
            consola_offset: 0,
            ultima_actividad: None,
        }
    }
}

/// `runtimeNuevo()` port.
pub fn runtime_nuevo() -> Runtime {
    Runtime::default()
}

impl Runtime {
    pub fn nuevo() -> Self {
        runtime_nuevo()
    }

    pub fn is_running(&self) -> bool {
        self.started_at.is_some()
    }
}

// ---------------------------------------------------------------------------
// Windows process helpers
// ---------------------------------------------------------------------------

/// Runs `programa` with `args`, capturing stdout, killing on timeout.
/// Never blocks the caller's thread: everything goes through tokio.
pub async fn ejecutalo<S: AsRef<str>>(
    programa: &str,
    args: &[S],
    timeout: Duration,
) -> Result<String, String> {
    let mut child = Command::new(programa)
        .args(args.iter().map(|s| s.as_ref()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;


    let mut stdout = child.stdout.take().ok_or_else(|| "no stdout pipe".to_string())?;
    let mut buffer = String::new();
    let read_fut = stdout.read_to_string(&mut buffer);

    match tokio::time::timeout(timeout, read_fut).await {
        Ok(Ok(_)) => {
            let _ = child.wait().await;
            Ok(buffer)
        }
        Ok(Err(e)) => {
            let _ = child.kill().await;
            Err(format!("read failed: {e}"))
        }
        Err(_) => {
            let _ = child.kill().await;
            Err("timeout".to_string())
        }
    }
}

/// Hidden-window variant: sets `CREATE_NO_WINDOW` before spawning. Prefer this
/// for `pythonw`/`tasklist`/`powershell` so no console flashes.
pub async fn ejecutalo_oculto<S: AsRef<str>>(
    programa: &str,
    args: &[S],
    timeout: Duration,
) -> Result<String, String> {
    let mut cmd = Command::new(programa);
    cmd.args(args.iter().map(|s| s.as_ref()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        #[allow(unused_imports)]
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let mut stdout = child.stdout.take().ok_or_else(|| "no stdout pipe".to_string())?;
    let mut buffer = String::new();
    let read_fut = stdout.read_to_string(&mut buffer);
    match tokio::time::timeout(timeout, read_fut).await {
        Ok(Ok(_)) => {
            let _ = child.wait().await;
            Ok(buffer)
        }
        Ok(Err(e)) => {
            let _ = child.kill().await;
            Err(format!("read failed: {e}"))
        }
        Err(_) => {
            let _ = child.kill().await;
            Err("timeout".to_string())
        }
    }
}

/// `pidVivo`: three-way. `Some(true)` alive, `Some(false)` not alive,
/// `None` could not check / empty output.
pub async fn pid_vivo(pid: u32) -> Option<bool> {
    let args: [&str; 5] = ["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"];
    let out = ejecutalo_oculto("tasklist", &args, Duration::from_secs(5)).await.ok()?;
    if out.trim().is_empty() {
        return None;
    }
    Some(out.contains(&format!("\"{pid}\"")))
}

/// `rutaPlana`: lowercase, `/` -> `\`, collapse `\\+`.
pub fn ruta_plana(ruta: &str) -> String {
    let lowered = ruta.to_lowercase().replace('/', "\\");
    let mut out = String::with_capacity(lowered.len());
    let mut prev_bs = false;
    for ch in lowered.chars() {
        if ch == '\\' {
            if prev_bs {
                continue;
            }
            prev_bs = true;
        } else {
            prev_bs = false;
        }
        out.push(ch);
    }
    out
}

/// Pure decision logic for `pidEsBot`, testable without spawning processes.
pub fn pid_es_bot_decision(
    pid: u32,
    cmdline: &str,
    panel_pid: Option<&str>,
    dir: Option<&Path>,
) -> Option<bool> {
    let linea = cmdline.trim();
    if linea.is_empty() {
        return None;
    }
    if !linea.contains("bot.py") {
        return Some(false);
    }
    if let Some(dir) = dir {
        if let Some(panel) = panel_pid {
            let panel = panel.trim();
            if !panel.is_empty() {
                if panel == pid.to_string() {
                    return Some(true);
                }
                return Some(false);
            }
        }
        let Some(path) = extraer_ruta_bot_py(linea) else {
            return Some(false);
        };
        let dir_plano = ruta_plana(&dir.to_string_lossy());
        let path_plano = ruta_plana(&path);
        return Some(path_plano.starts_with(&format!("{dir_plano}\\")));
    }
    Some(linea.contains("pythonw"))
}

// Aliases for the pure decision (different naming conventions).
pub fn pid_es_bot_puro(
    pid: u32,
    cmdline: &str,
    panel_pid: Option<&str>,
    dir: Option<&Path>,
) -> Option<bool> {
    pid_es_bot_decision(pid, cmdline, panel_pid, dir)
}
pub fn decidir_pid_es_bot(
    pid: u32,
    cmdline: &str,
    panel_pid: Option<&str>,
    dir: Option<&Path>,
) -> Option<bool> {
    pid_es_bot_decision(pid, cmdline, panel_pid, dir)
}
pub fn evaluar_pid_es_bot(
    pid: u32,
    cmdline: &str,
    panel_pid: Option<&str>,
    dir: Option<&Path>,
) -> Option<bool> {
    pid_es_bot_decision(pid, cmdline, panel_pid, dir)
}
pub fn pid_es_bot_logica(
    pid: u32,
    cmdline: &str,
    panel_pid: Option<&str>,
    dir: Option<&Path>,
) -> Option<bool> {
    pid_es_bot_decision(pid, cmdline, panel_pid, dir)
}

fn extraer_ruta_bot_py(linea: &str) -> Option<String> {
    // Quoted form: "([^"]*bot\.py)"
    let mut search = 0usize;
    while search < linea.len() {
        let Some(rel) = linea[search..].find('"') else {
            break;
        };
        let abs_start = search + rel;
        let Some(rel_end) = linea[abs_start + 1..].find('"') else {
            break;
        };
        let abs_end = abs_start + 1 + rel_end;
        let content = &linea[abs_start + 1..abs_end];
        if content.ends_with("bot.py") {
            return Some(content.to_string());
        }
        search = abs_end + 1;
    }
    // Unquoted form: ([^\s"]*bot\.py)
    if let Some(pos) = linea.find("bot.py") {
        let end = pos + "bot.py".len();
        let before = &linea[..pos];
        let start = before
            .rfind(|c: char| c.is_whitespace() || c == '"')
            .map(|i| i + 1)
            .unwrap_or(0);
        let candidate = &linea[start..end];
        if !candidate.is_empty()
            && !candidate.contains('"')
            && !candidate.chars().any(char::is_whitespace)
        {
            return Some(candidate.to_string());
        }
    }
    None
}

/// `pidEsBot`: runs PowerShell and applies the pure decision.
pub async fn pid_es_bot(pid: u32, dir: Option<&Path>) -> Option<bool> {
    let filter = format!("ProcessId={pid}");
    let args = [
        "-NoProfile",
        "-Command",
        &format!(
            "Get-CimInstance Win32_Process -Filter \"{}\" | Select-Object -ExpandProperty CommandLine",
            filter
        ),
    ];
    let out = ejecutalo_oculto("powershell", &args, Duration::from_secs(8))
        .await
        .ok()?;
    let panel_contents = match dir {
        Some(d) => tokio::fs::read_to_string(d.join("panel.pid")).await.ok(),
        None => None,
    };
    pid_es_bot_decision(pid, &out, panel_contents.as_deref(), dir)
}

// ---------------------------------------------------------------------------
// arranquesEnVuelo guard
// ---------------------------------------------------------------------------

pub struct ArranquesEnVuelo {
    inner: Mutex<HashSet<String>>,
}

impl ArranquesEnVuelo {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashSet::new()),
        }
    }

    /// Returns `true` if the start was registered (was not in flight).
    pub fn intentar(&self, id: &str) -> bool {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if guard.contains(id) {
            false
        } else {
            guard.insert(id.to_string());
            true
        }
    }

    pub fn liberar(&self, id: &str) {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard.remove(id);
    }

    pub fn en_vuelo(&self, id: &str) -> bool {
        let guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        guard.contains(id)
    }
}

impl Default for ArranquesEnVuelo {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Offset helpers (thin wrappers over pool)
// ---------------------------------------------------------------------------

pub use crate::pool::{file_tail_size, today_offset};

pub fn offset_de_hoy(path: &Path, today_prefix: &str) -> u64 {
    today_offset(path, today_prefix)
}

pub fn tamano_cola(path: &Path) -> u64 {
    file_tail_size(path)
}

// ---------------------------------------------------------------------------
// servidor-bots scaffolding
// ---------------------------------------------------------------------------

/// Generates a 48-hex-char token (24 bytes), like `randomBytes(24).toString("hex")`.
pub fn generar_token() -> String {
    generar_token_bot()
}

fn generar_token_bot() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ std::process::id() as u64
        ^ counter;
    // Simple xorshift to produce 24 bytes.
    let mut state = seed | 1;
    let mut bytes = [0u8; 24];
    for byte in bytes.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = (state & 0xff) as u8;
    }
    let mut out = String::with_capacity(48);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

pub fn token_valido(token: &str) -> bool {
    token.len() >= 16
}

/// In-memory bot server token store.
pub struct ServidorBots {
    tokens: Mutex<HashMap<String, String>>, // token -> system
    sistemas: Mutex<HashMap<String, String>>, // system -> token
}

impl ServidorBots {
    pub fn nuevo() -> Self {
        Self {
            tokens: Mutex::new(HashMap::new()),
            sistemas: Mutex::new(HashMap::new()),
        }
    }

    pub fn token_de(&self, sistema: &str) -> String {
        {
            let sistemas = self.sistemas.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(t) = sistemas.get(sistema) {
                return t.clone();
            }
        }
        let t = generar_token();
        {
            let mut sistemas = self.sistemas.lock().unwrap_or_else(|p| p.into_inner());
            sistemas.insert(sistema.to_string(), t.clone());
        }
        {
            let mut tokens = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
            tokens.insert(t.clone(), sistema.to_string());
        }
        t
    }

    pub fn registrar_token(&self, sistema: &str, token: &str) {
        {
            let mut sistemas = self.sistemas.lock().unwrap_or_else(|p| p.into_inner());
            sistemas.insert(sistema.to_string(), token.to_string());
        }
        {
            let mut tokens = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
            tokens.insert(token.to_string(), sistema.to_string());
        }
    }

    pub fn sistema_de_token(&self, token: &str) -> Option<String> {
        if token.len() < 16 {
            return None;
        }
        let tokens = self.tokens.lock().unwrap_or_else(|p| p.into_inner());
        tokens.get(token).cloned()
    }

    pub fn validar_token(&self, token: &str) -> bool {
        self.sistema_de_token(token).is_some()
    }
}

impl Default for ServidorBots {
    fn default() -> Self {
        Self::nuevo()
    }
}

/// Free function: resolve system by token from a map (linear scan).
pub fn sistema_de_token_mapa(mapa: &HashMap<String, String>, token: &str) -> Option<String> {
    if token.len() < 16 {
        return None;
    }
    for (t, s) in mapa {
        if t == token {
            return Some(s.clone());
        }
    }
    None
}

pub fn resolver_sistema_por_token(mapa: &HashMap<String, String>, token: &str) -> Option<String> {
    sistema_de_token_mapa(mapa, token)
}

pub fn sistema_de_token(tokens: &HashMap<String, String>, token: &str) -> Option<String> {
    sistema_de_token_mapa(tokens, token)
}

/// Bot server routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RutaBot {
    Hello,
    Qualify,
    Bars,
    Order,
    Cancel,
    Orders,
    Fills,
    Positions,
    Account,
}

pub fn emparejar_ruta(metodo: &str, path: &str) -> Option<RutaBot> {
    match (metodo.to_uppercase().as_str(), path) {
        ("GET", "/bot/hello") => Some(RutaBot::Hello),
        ("POST", "/bot/qualify") => Some(RutaBot::Qualify),
        ("POST", "/bot/bars") => Some(RutaBot::Bars),
        ("POST", "/bot/order") => Some(RutaBot::Order),
        ("POST", "/bot/cancel") => Some(RutaBot::Cancel),
        ("GET", "/bot/orders") => Some(RutaBot::Orders),
        ("GET", "/bot/fills") => Some(RutaBot::Fills),
        ("GET", "/bot/positions") => Some(RutaBot::Positions),
        ("GET", "/bot/account") => Some(RutaBot::Account),
        _ => None,
    }
}

/// Auth extraction trait over common header containers.
pub trait ExtraerTokenBot {
    fn extraer_token_bot(&self) -> Option<String>;
}

impl ExtraerTokenBot for HashMap<String, String> {
    fn extraer_token_bot(&self) -> Option<String> {
        self.iter().find_map(|(k, v)| {
            if k.eq_ignore_ascii_case("x-openbot-bot") {
                Some(v.clone())
            } else {
                None
            }
        })
    }
}

impl ExtraerTokenBot for std::collections::BTreeMap<String, String> {
    fn extraer_token_bot(&self) -> Option<String> {
        self.iter().find_map(|(k, v)| {
            if k.eq_ignore_ascii_case("x-openbot-bot") {
                Some(v.clone())
            } else {
                None
            }
        })
    }
}

impl ExtraerTokenBot for Vec<(String, String)> {
    fn extraer_token_bot(&self) -> Option<String> {
        self.iter().find_map(|(k, v)| {
            if k.eq_ignore_ascii_case("x-openbot-bot") {
                Some(v.clone())
            } else {
                None
            }
        })
    }
}

impl ExtraerTokenBot for [(String, String)] {
    fn extraer_token_bot(&self) -> Option<String> {
        self.iter().find_map(|(k, v)| {
            if k.eq_ignore_ascii_case("x-openbot-bot") {
                Some(v.clone())
            } else {
                None
            }
        })
    }
}

pub fn extraer_token<T: ExtraerTokenBot>(headers: &T) -> Option<String> {
    headers.extraer_token_bot()
}

/// Dispatch result for a bot HTTP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RespuestaBot {
    pub status: u16,
    pub route: Option<RutaBot>,
    pub system: Option<String>,
    pub error: Option<String>,
}

pub fn despachar_bot<F>(metodo: &str, path: &str, token: Option<&str>, resolver: F) -> RespuestaBot
where
    F: Fn(&str) -> Option<String>,
{
    let sistema = match token {
        Some(t) if t.len() >= 16 => resolver(t),
        _ => None,
    };
    if sistema.is_none() {
        return RespuestaBot {
            status: 401,
            route: None,
            system: None,
            error: Some("token de bot inválido".to_string()),
        };
    }
    match emparejar_ruta(metodo, path) {
        Some(r) => RespuestaBot {
            status: 200,
            route: Some(r),
            system: sistema,
            error: None,
        },
        None => RespuestaBot {
            status: 404,
            route: None,
            system: sistema,
            error: Some("ruta no encontrada".to_string()),
        },
    }
}

impl ServidorBots {
    pub fn despachar(&self, metodo: &str, path: &str, token: Option<&str>) -> RespuestaBot {
        despachar_bot(metodo, path, token, |t| self.sistema_de_token(t))
    }
}

// ---------------------------------------------------------------------------
// Spawn/stop planning helpers
// ---------------------------------------------------------------------------

pub const STOP_ORIGIN_USER: &str = "user";
pub const STOP_ORIGIN_TASK: &str = "task";
pub const STOP_ORIGIN_PANIC: &str = "panic";

pub fn mensaje_origen_stop(origen: &str) -> Option<&'static str> {
    match origen {
        "user" => Some("trading.log.bot.stopped_user"),
        "task" => Some("trading.log.bot.stopped_task"),
        "panic" => Some("trading.log.bot.stopped_panic"),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmacionPid {
    NoVerificado,
    SigueVivo,
    Detenido,
}

impl From<Option<bool>> for ConfirmacionPid {
    fn from(value: Option<bool>) -> Self {
        match value {
            None => ConfirmacionPid::NoVerificado,
            Some(true) => ConfirmacionPid::SigueVivo,
            Some(false) => ConfirmacionPid::Detenido,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccionParadaPid {
    NoMatarNoVerificado,
    YaMuerto,
    Matar,
    NoMatarReciclado,
    NoMatarNoVerificadoBot,
}

pub fn accion_parada_pid(pid_vivo: Option<bool>, pid_es_bot: Option<bool>) -> AccionParadaPid {
    match pid_vivo {
        None => AccionParadaPid::NoMatarNoVerificado,
        Some(false) => AccionParadaPid::YaMuerto,
        Some(true) => match pid_es_bot {
            Some(true) => AccionParadaPid::Matar,
            Some(false) => AccionParadaPid::NoMatarReciclado,
            None => AccionParadaPid::NoMatarNoVerificadoBot,
        },
    }
}

/// Whether a stop is confirmed after the process kill (true only if dead).
pub fn parada_confirmada(pid_vivo_despues: Option<bool>) -> bool {
    matches!(pid_vivo_despues, Some(false))
}

/// Whether a child-process stop is confirmed.
pub fn parada_hijo_confirmada(pid_vivo_despues: Option<bool>) -> bool {
    parada_confirmada(pid_vivo_despues)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn ruta_plana_basica() {
        assert_eq!(ruta_plana("C:/Foo//Bar\\baz"), "c:\\foo\\bar\\baz");
        assert_eq!(ruta_plana("A//B///C"), "a\\b\\c");
        assert_eq!(ruta_plana(""), "");
        assert_eq!(ruta_plana("no/slashes"), "no\\slashes");
    }

    #[test]
    fn runtime_nuevo_inicia_vacio() {
        let r = runtime_nuevo();
        assert!(r.source.is_none());
        assert!(r.pid.is_none());
        assert!(r.started_at.is_none());
        assert!(r.child.is_none());
        assert!(!r.planeado);
        assert!(!r.sondeo_dudado);
        assert_eq!(r.registro_offset, 0);
        assert_eq!(r.consola_offset, 0);
        assert!(r.ultima_actividad.is_none());
    }

    #[test]
    fn pid_es_bot_none_si_vacio() {
        assert_eq!(pid_es_bot_decision(10, "", None, None), None);
        assert_eq!(pid_es_bot_decision(10, "   ", None, None), None);
    }

    #[test]
    fn pid_es_bot_false_sin_bot_py() {
        assert_eq!(
            pid_es_bot_decision(10, "pythonw foo.py", None, None),
            Some(false)
        );
    }

    #[test]
    fn pid_es_bot_sin_dir_requiere_pythonw() {
        let cmd = "pythonw C:\\bots\\bot.py";
        assert_eq!(pid_es_bot_decision(10, cmd, None, None), Some(true));
        let cmd2 = "python C:\\bots\\bot.py";
        assert_eq!(pid_es_bot_decision(10, cmd2, None, None), Some(false));
    }

    #[test]
    fn pid_es_bot_dir_panel_pid_match() {
        let dir = PathBuf::from("C:\\sistemas\\07");
        let cmd = "pythonw \"C:\\sistemas\\07\\bot.py\"";
        assert_eq!(
            pid_es_bot_decision(42, cmd, Some("42"), Some(&dir)),
            Some(true)
        );
    }

    #[test]
    fn pid_es_bot_dir_panel_pid_mismatch() {
        let dir = PathBuf::from("C:\\sistemas\\07");
        let cmd = "pythonw \"C:\\sistemas\\07\\bot.py\"";
        assert_eq!(
            pid_es_bot_decision(42, cmd, Some("99"), Some(&dir)),
            Some(false)
        );
    }

    #[test]
    fn pid_es_bot_dir_sin_panel_verifica_prefijo() {
        let dir = PathBuf::from("C:\\sistemas\\07");
        let cmd = "pythonw \"C:\\sistemas\\07\\bot.py\"";
        assert_eq!(
            pid_es_bot_decision(42, cmd, Some(""), Some(&dir)),
            Some(true)
        );
        let cmd2 = "pythonw \"C:\\otros\\bot.py\"";
        assert_eq!(
            pid_es_bot_decision(42, cmd2, Some(""), Some(&dir)),
            Some(false)
        );
    }

    #[test]
    fn pid_es_bot_dir_panel_pid_vacio_procede() {
        let dir = PathBuf::from("C:\\sistemas\\07");
        let cmd = "pythonw \"C:\\sistemas\\07\\bot.py\"";
        // panel.pid absent => None
        assert_eq!(
            pid_es_bot_decision(42, cmd, None, Some(&dir)),
            Some(true)
        );
    }

    #[test]
    fn pid_es_bot_unquoted_path() {
        let dir = PathBuf::from("C:\\sistemas\\07");
        let cmd = "pythonw C:\\sistemas\\07\\bot.py";
        assert_eq!(
            pid_es_bot_decision(42, cmd, Some(""), Some(&dir)),
            Some(true)
        );
    }

    #[test]
    fn pid_es_bot_case_insensitive_path() {
        let dir = PathBuf::from("C:\\Sistemas\\07");
        // ruta_plana lowercases; ensure match
        let cmd = "pythonw \"C:\\Sistemas\\07\\bot.py\"";
        assert_eq!(
            pid_es_bot_decision(42, cmd, Some(""), Some(&dir)),
            Some(true)
        );
    }

    #[test]
    fn token_validacion_y_resolucion() {
        let mut mapa: HashMap<String, String> = HashMap::new();
        let tok = "a".repeat(24);
        mapa.insert(tok.clone(), "07".to_string());
        assert!(token_valido(&tok));
        assert!(!token_valido("corto"));
        assert_eq!(sistema_de_token_mapa(&mapa, &tok), Some("07".to_string()));
        assert_eq!(sistema_de_token_mapa(&mapa, "corto"), None);
        let generado = generar_token();
        assert!(generado.len() >= 16);
        assert!(generado.len() == 48);
    }

    #[test]
    fn servidor_bots_token_de_y_resolve() {
        let s = ServidorBots::nuevo();
        let t = s.token_de("07");
        assert!(t.len() >= 16);
        assert_eq!(s.sistema_de_token(&t), Some("07".to_string()));
        assert!(s.validar_token(&t));
        assert!(!s.validar_token("nope"));
        // idempotent
        assert_eq!(s.token_de("07"), t);
    }

    #[test]
    fn servidor_bots_registrar_token() {
        let s = ServidorBots::nuevo();
        s.registrar_token("09", "b".repeat(20).as_str());
        assert_eq!(s.sistema_de_token(&"b".repeat(20)), Some("09".to_string()));
    }

    #[test]
    fn extraer_token_cabecera() {
        let mut h: HashMap<String, String> = HashMap::new();
        h.insert("X-OpenBot-Bot".to_string(), "tok123456789012345".to_string());
        assert_eq!(extraer_token(&h), Some("tok123456789012345".to_string()));
        let v = vec![("other".to_string(), "x".to_string())];
        assert!(extraer_token(&v).is_none());
    }

    #[test]
    fn emparejar_rutas() {
        assert_eq!(emparejar_ruta("GET", "/bot/hello"), Some(RutaBot::Hello));
        assert_eq!(emparejar_ruta("POST", "/bot/order"), Some(RutaBot::Order));
        assert!(emparejar_ruta("GET", "/bot/nope").is_none());
        assert!(emparejar_ruta("DELETE", "/bot/hello").is_none());
    }

    #[test]
    fn despachar_auth_y_404() {
        let resolver = |t: &str| {
            if t == "validtoken1234567890" {
                Some("07".to_string())
            } else {
                None
            }
        };
        // invalid token -> 401
        let r = despachar_bot("GET", "/bot/hello", Some("short"), resolver);
        assert_eq!(r.status, 401);
        // unknown route with valid token -> 404
        let r = despachar_bot("GET", "/bot/nope", Some("validtoken1234567890"), resolver);
        assert_eq!(r.status, 404);
        // valid -> 200
        let r = despachar_bot("GET", "/bot/hello", Some("validtoken1234567890"), resolver);
        assert_eq!(r.status, 200);
        assert_eq!(r.route, Some(RutaBot::Hello));
        assert_eq!(r.system, Some("07".to_string()));
        // no token -> 401
        let r = despachar_bot("GET", "/bot/hello", None, resolver);
        assert_eq!(r.status, 401);
    }

    #[test]
    fn despachar_servidor_bots() {
        let s = ServidorBots::nuevo();
        let t = s.token_de("07");
        let r = s.despachar("GET", "/bot/account", Some(&t));
        assert_eq!(r.status, 200);
        let r = s.despachar("GET", "/bot/account", Some("invalidtoken1234567890"));
        assert_eq!(r.status, 401);
    }

    #[test]
    fn arranques_en_vuelo_guard() {
        let g = ArranquesEnVuelo::new();
        assert!(g.intentar("07"));
        assert!(!g.intentar("07"));
        assert!(g.en_vuelo("07"));
        g.liberar("07");
        assert!(!g.en_vuelo("07"));
        assert!(g.intentar("07"));
    }

    #[test]
    fn confirmacion_pid_from_option() {
        assert_eq!(ConfirmacionPid::from(None), ConfirmacionPid::NoVerificado);
        assert_eq!(ConfirmacionPid::from(Some(true)), ConfirmacionPid::SigueVivo);
        assert_eq!(ConfirmacionPid::from(Some(false)), ConfirmacionPid::Detenido);
    }

    #[test]
    fn accion_parada_pid_escenarios() {
        assert_eq!(
            accion_parada_pid(None, None),
            AccionParadaPid::NoMatarNoVerificado
        );
        assert_eq!(accion_parada_pid(Some(false), None), AccionParadaPid::YaMuerto);
        assert_eq!(
            accion_parada_pid(Some(true), Some(true)),
            AccionParadaPid::Matar
        );
        assert_eq!(
            accion_parada_pid(Some(true), Some(false)),
            AccionParadaPid::NoMatarReciclado
        );
        assert_eq!(
            accion_parada_pid(Some(true), None),
            AccionParadaPid::NoMatarNoVerificadoBot
        );
    }

    #[test]
    fn parada_confirmada_solo_si_muerto() {
        assert!(!parada_confirmada(None));
        assert!(!parada_confirmada(Some(true)));
        assert!(parada_confirmada(Some(false)));
    }

    #[test]
    fn mensaje_origen_stop_test() {
        assert!(mensaje_origen_stop("user").is_some());
        assert!(mensaje_origen_stop("task").is_some());
        assert!(mensaje_origen_stop("panic").is_some());
        assert!(mensaje_origen_stop("otro").is_none());
    }

    #[test]
    fn offset_wrappers() {
        // just ensure compile and call pool via wrappers is not duplicated wrongly
        let p = std::env::temp_dir().join("nope_lifecycle.log");
        assert_eq!(offset_de_hoy(&p, "2026-11-17"), 0);
        assert_eq!(tamano_cola(&p), 0);
    }
}
