//! The WARP ring: one rotation program, the desktop bat's own.
//!
//! Take the shared lock, make sure WARP's daemon is up, flip the UTC day file
//! when the quota renewed, dedupe the WARP egress against the day's memory until
//! an unseen `/64` lands, then pin `opencode.ai` over it when the app may write
//! the hosts file. It shares every state file the bat and its monitor use — same
//! lock, same memory, same events, same day file — so the button and the bat
//! never rotate at once and never count one `/64` twice.
//!
//! What it deliberately does not port: the app kill+relaunch, the window hide,
//! the winget install and the matinal task. A failed rotation is an honest
//! `ok:false` on the button, never a closed app.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::bucket::cubo;
use crate::history::{GhostRun, a_jsonl, leer_historial, seed};
use crate::lock::librar;
use crate::memory::{ajustar_memoria, dia_girado, podar_eventos, recordado};
use crate::mundo::Mundo;
use crate::pin::{pin_vivo, sin_pin};
use crate::rutas::{APP_IMAGENES, PIN_DEFECTO, PIN_LITERALES, Rutas};
use crate::shell::trazo;

pub const MAX_INTENTOS: u32 = 10;
pub const INTENTOS_TRAZA: u32 = 20;
pub const SONDEO_DESCONEXION: u32 = 4;
pub const SONDEO_DAEMON: u32 = 6;

pub const ESPERA_WARP: Duration = Duration::from_secs(30);
pub const ESPERA_SERVICIO: Duration = Duration::from_secs(60);
pub const ESPERA_POWERSHELL: Duration = Duration::from_secs(30);
pub const ESPERA_RAPIDA: Duration = Duration::from_secs(15);
/// The AAAA ask keeps the TS resolver's own cap.
pub const ESPERA_DNS: Duration = Duration::from_secs(4);

/// The tunnel's own answer, as the status page shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EstadoWarp {
    Connected,
    Disconnected,
    /// `warp-cli` is installed but the service is down or says nothing.
    Unavailable,
    /// No `warp-cli` on this machine.
    Missing,
}

impl EstadoWarp {
    pub fn como_texto(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::Unavailable => "unavailable",
            Self::Missing => "missing",
        }
    }
}

/// What the engine and WARP answer right now: the shared files, the service and
/// one client probe. Nothing is written and nothing rotates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Estado {
    pub warp: EstadoWarp,
    pub rotating: bool,
    pub attempts: u32,
    pub attempt: Option<u32>,
    pub rotations: usize,
    pub current: Option<String>,
    pub pin: bool,
}

/// One rotate call's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rotacion {
    pub ok: bool,
    /// The probe's exact exit address, only when the rotation landed.
    pub address: Option<String>,
}

impl Rotacion {
    pub fn fallo() -> Self {
        Self {
            ok: false,
            address: None,
        }
    }
}

/// The hot egress the gateway rides (lane C2). Its `apuntar` answers the exit
/// that proved a fresh IPv6 path; C1 only needs to ask and, when the graph
/// mounts none, fall back to the trace verdict.
#[async_trait]
pub trait Egress: Send + Sync {
    /// Points the gateway at the first candidate that proves itself.
    async fn apuntar(&self, direcciones: &[String]) -> Option<String>;
    /// Hands the machine back its own address.
    async fn soltar(&self);
    /// The exit the *app's* traffic leaves by, when this transport knows better
    /// than the machine's own `curl`. `None` keeps the pre-C2 trace: in WARP's
    /// proxy mode the machine's curl leaves by the router and reports
    /// `warp=off`, so the ring must ask the transport it mounted.
    async fn traza(&self) -> Option<Traza> {
        None
    }
}

/// One shield trace's exit facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Traza {
    pub ip: String,
    pub warp: Option<String>,
}

/// The ring, over one set of shared files.
pub struct Ghost {
    pub rutas: Rutas,
    mundo: Arc<dyn Mundo>,
    egress: Option<Arc<dyn Egress>>,
    /// The ghost flag: a disabled ring does nothing and says so.
    activo: AtomicBool,
    runs: AtomicU32,
    attempt: AtomicU32,
}

impl Ghost {
    pub fn nuevo(rutas: Rutas, mundo: Arc<dyn Mundo>) -> Self {
        Self {
            rutas,
            mundo,
            egress: None,
            activo: AtomicBool::new(true),
            runs: AtomicU32::new(0),
            attempt: AtomicU32::new(0),
        }
    }

    pub fn con_egress(mut self, egress: Arc<dyn Egress>) -> Self {
        self.egress = Some(egress);
        self
    }

    pub fn activar(&self, activo: bool) {
        self.activo.store(activo, Ordering::SeqCst);
    }

    /// The whole rotation program, once. A concurrent run retires through the
    /// shared lock.
    pub async fn rotar(&self, origen: Option<&str>) -> Rotacion {
        if !self.activo.load(Ordering::SeqCst) {
            self.evt("rotate: ghost deshabilitado por flag").await;
            return Rotacion::fallo();
        }
        let empezado = self.mundo.ahora_ms();
        let memoria = self.lineas(&self.rutas.memoria);
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.attempt.store(0, Ordering::SeqCst);
        let salida = self.programar().await;
        let intentos = self.attempt.load(Ordering::SeqCst);
        self.registrar(
            &salida,
            origen,
            memoria.last().map(String::as_str),
            empezado,
            intentos,
        )
        .await;
        self.runs.fetch_sub(1, Ordering::SeqCst);
        salida
    }

    /// The episode's close: the pin comes off the hosts and the WARP tunnel
    /// hangs up, so the app rides its own address again.
    pub async fn soltar(&self) -> Rotacion {
        if !self.mundo_es_windows() {
            return Rotacion::fallo();
        }
        let Some(_cerrojo) = self.tomar_cerrojo() else {
            self.evt("release: descartado - ya hay un run en marcha").await;
            return Rotacion::fallo();
        };
        if !self.activo.load(Ordering::SeqCst) {
            self.evt("release: ghost deshabilitado por flag").await;
            return Rotacion::fallo();
        }
        // An unreadable hosts file cannot prove or disprove a pin: with a pin
        // possibly standing, disconnecting would leave it pointing at a WARP
        // address nothing serves.
        if self.leer_hosts().is_none() {
            self.evt("release: la lectura de hosts fallo - el pin no se puede verificar - WARP se deja arriba")
                .await;
            return Rotacion::fallo();
        }
        if self.pin_vivo() {
            if !self.host_permite() {
                self.evt("release: pin vivo y hosts deniega la escritura - WARP se deja arriba")
                    .await;
                return Rotacion::fallo();
            }
            if !self.despintar().await {
                self.evt("release: pin: no pudo quitarse la linea - WARP se deja arriba")
                    .await;
                return Rotacion::fallo();
            }
        }
        let cli = self.locate_warp_cli().await;
        if cli.is_empty() {
            self.soltar_egress().await;
            self.evt("release: sin warp-cli - el pin fuera y la salida ya es la propia del equipo")
                .await;
            return Rotacion {
                ok: true,
                address: None,
            };
        }
        self.soltar_egress().await;
        self.warp(&cli, &["disconnect"]).await;
        if !self.esperar_desconexion(&cli).await {
            self.evt("release: WARP sigue conectado tras el disconnect - reintenten las llamadas")
                .await;
            return Rotacion::fallo();
        }
        self.evt("release: WARP desconectado - la salida vuelve a la propia del equipo (servicio arriba)")
            .await;
        Rotacion {
            ok: true,
            address: None,
        }
    }

    /// The status read: shared files plus one client probe, read-only.
    pub async fn estado(&self) -> Estado {
        let hoy = self.mundo.hoy_utc();
        let dia = self
            .leer_texto(&self.rutas.dia)
            .map(|texto| primera_linea(&texto))
            .unwrap_or_default();
        let memoria = if dia == hoy {
            self.lineas(&self.rutas.memoria)
        } else {
            Vec::new()
        };
        let mut warp = if self.mundo_es_windows() && self.rutas.warp_cli.exists() {
            EstadoWarp::Unavailable
        } else {
            EstadoWarp::Missing
        };
        if warp == EstadoWarp::Unavailable {
            let consulta = self
                .mundo
                .ejecutar(
                    "sc",
                    &args(&["query", "CloudflareWARP"]),
                    Some(ESPERA_SERVICIO),
                )
                .await;
            if consulta.stdout.contains("RUNNING") || consulta.stdout.contains("START_PENDING") {
                let cli = self.rutas.warp_cli.to_string_lossy().to_string();
                let salida = self.warp_status(&cli).await;
                if salida.contains("Status update: Connected") {
                    warp = EstadoWarp::Connected;
                } else if salida.contains("Disconnected") {
                    warp = EstadoWarp::Disconnected;
                }
            }
        }
        let runs = self.runs.load(Ordering::SeqCst);
        let attempt = self.attempt.load(Ordering::SeqCst);
        Estado {
            warp,
            rotating: runs > 0,
            attempts: MAX_INTENTOS,
            attempt: (runs > 0 && attempt > 0).then_some(attempt),
            rotations: memoria.len(),
            current: memoria.last().cloned(),
            pin: self.pin_vivo(),
        }
    }

    /// This engine's finished runs, oldest first.
    pub async fn historial(&self) -> Vec<GhostRun> {
        if !self.rutas.historial.exists() {
            let eventos = self.leer_texto(&self.rutas.eventos).unwrap_or_default();
            let hoy = chrono::Local::now().date_naive();
            let runs = seed(&eventos, hoy);
            let _ = std::fs::write(&self.rutas.historial, a_jsonl(&runs));
        }
        self.leer_texto(&self.rutas.historial)
            .map(|texto| leer_historial(&texto))
            .unwrap_or_default()
    }

    // ---- the ladder -------------------------------------------------------

    async fn programar(&self) -> Rotacion {
        if !self.mundo_es_windows() {
            return Rotacion::fallo();
        }
        let Some(_cerrojo) = self.tomar_cerrojo() else {
            self.evt("cerrojo: rotate descartado - ya hay un run en marcha")
                .await;
            return Rotacion::fallo();
        };

        // The bat would install WARP here; the engine refuses, honestly.
        let cli = self.locate_warp_cli().await;
        if cli.is_empty() {
            self.evt("rotate: warp-cli no encontrado - imposible rotar")
                .await;
            return Rotacion::fallo();
        }

        if !self.preflight(&cli).await {
            self.evt("abortar: daemon WARP inalcanzable tras el net start")
                .await;
            return Rotacion::fallo();
        }

        self.girar_dia().await;
        self.ajustar_memoria_del_dia().await;
        self.warp(&cli, &["--accept-tos", "mode", "warp"]).await;

        let app_viva = self.contar_app().await;
        let monitores = self.contar_monitores().await;
        if app_viva && monitores > 0 {
            self.evt("auto-hot: sesion viva detectada - rotacion en caliente de su /64")
                .await;
        }

        let puede_hosts = self.host_permite();
        let mut ganada: Option<(String, String)> = None;
        let mut sin_ip = 0u32;
        let mut repetidos = 0u32;

        for intento in 1..=MAX_INTENTOS {
            if ganada.is_some() {
                break;
            }
            self.attempt.store(intento, Ordering::SeqCst);
            self.warp(&cli, &["disconnect"]).await;
            self.esperar_desconexion(&cli).await;
            self.warp(&cli, &["connect"]).await;

            let cuerpo = self.sondear().await;
            let ip = cuerpo.as_ref().map(|traza| traza.ip.clone());
            let warp_flag = cuerpo.as_ref().and_then(|traza| traza.warp.clone());
            let bucket = ip.as_deref().and_then(cubo);
            let vista = match &bucket {
                Some(bucket) => self.recordado(bucket).await,
                None => false,
            };

            if warp_flag.as_deref() == Some("on") && ip.is_some() && bucket.is_some() && !vista {
                let landing = ip.clone().unwrap_or_default();
                let cubo_ganado = bucket.clone().unwrap_or_default();
                ganada = Some((landing, cubo_ganado));
                break;
            }
            if ip.is_some() {
                repetidos += 1;
            } else {
                sin_ip += 1;
            }
            if let Some(recordado) = bucket.as_ref().filter(|_| !vista) {
                self.anadir(&self.rutas.memoria, recordado);
            }
            let etiqueta = match (&bucket, vista) {
                (None, _) => format!("intento {intento}: sin IP de WARP"),
                (Some(bucket), true) => format!("intento {intento}: /64 {bucket} repetido"),
                (Some(bucket), false) => format!(
                    "intento {intento}: /64 {bucket} fuera de WARP (warp={})",
                    warp_flag.as_deref().unwrap_or("?")
                ),
            };
            self.evt(&etiqueta).await;
            if ip.is_none() {
                self.evt("rescate: re-registro de WARP").await;
                self.warp(&cli, &["registration", "delete"]).await;
                self.mundo.dormir(Duration::from_secs(3)).await;
                self.warp(&cli, &["--accept-tos", "registration", "new"])
                    .await;
                self.mundo.dormir(Duration::from_secs(5)).await;
            }
        }

        let Some((ip, bucket)) = ganada else {
            self.warp(&cli, &["disconnect"]).await;
            let final_ = if sin_ip >= repetidos {
                format!("fin sin conectar: {sin_ip} intentos sin IP")
            } else {
                format!("pool agotado: {repetidos} bloques repetidos")
            };
            self.evt(&final_).await;
            return Rotacion::fallo();
        };

        // A fresh /64 only serves the app when its next gateway request leaves
        // by it. An embedder's graph may mount no egress at all: the old trace
        // verdict is all that stands there.
        self.anadir(&self.rutas.memoria, &bucket);
        let resuelta = self.dirigir_aaaa().await;

        let Some(egress) = self.egress.clone() else {
            self.evt("egress: el grafo no monto el transporte - veredicto solo por traza")
                .await;
            return if self.tracer_ok(8).await {
                Rotacion {
                    ok: true,
                    address: Some(ip),
                }
            } else {
                Rotacion::fallo()
            };
        };

        let mut candidatos: Vec<String> = resuelta.into_iter().collect();
        candidatos.extend(PIN_LITERALES.iter().map(|pin| pin.to_string()));
        let Some(exit) = egress.apuntar(&candidatos).await else {
            self.evt("egress: ningun IPv6 de opencode.ai respondio - la rotacion no sirve a la app")
                .await;
            if puede_hosts {
                self.despintar().await;
            }
            return Rotacion::fallo();
        };

        // The pin stays for the machines that may write it — the bat's own
        // hygiene, and the path the desktop app's resolver still reads.
        if puede_hosts {
            self.asegurar_pin().await;
        }

        // The monitor is the session's hygiene, not the pin's: on a live app it
        // rides along whether or not this process could pin.
        if app_viva {
            self.epoch_nueva();
            self.lanzar_monitor(&cli).await;
        }

        self.evt(&format!(
            "rotate: /64 nuevo {bucket} en caliente - OpenCode intacto (salida {exit})"
        ))
        .await;
        Rotacion {
            ok: true,
            address: Some(ip),
        }
    }

    // ---- ceremonies -------------------------------------------------------

    async fn preflight(&self, cli: &str) -> bool {
        self.mundo
            .ejecutar(
                "net",
                &args(&["start", "CloudflareWARP"]),
                Some(ESPERA_SERVICIO),
            )
            .await;
        self.mundo
            .ejecutar(
                "net",
                &args(&["start", "Cloudflare WARP"]),
                Some(ESPERA_SERVICIO),
            )
            .await;
        self.mundo.dormir(Duration::from_secs(3)).await;
        self.podar_eventos().await;
        for _ in 0..SONDEO_DAEMON {
            let salida = self.warp_status(cli).await;
            if salida.contains("Status update") {
                return true;
            }
            self.mundo.dormir(Duration::from_secs(1)).await;
        }
        false
    }

    async fn podar_eventos(&self) {
        let lineas = self.lineas(&self.rutas.eventos);
        if let Some(recortadas) = podar_eventos(&lineas) {
            let texto = recortadas.join("\n");
            let _ = std::fs::write(&self.rutas.eventos, format!("{texto}\n"));
        }
    }

    async fn girar_dia(&self) {
        let hoy = self.mundo.hoy_utc();
        let anterior = self
            .leer_texto(&self.rutas.dia)
            .map(|texto| primera_linea(&texto))
            .unwrap_or_default();
        let _ = std::fs::write(&self.rutas.dia, format!("{hoy}\n"));
        if dia_girado(&anterior, &hoy) {
            let _ = std::fs::remove_file(&self.rutas.memoria);
            self.evt("dia UTC nuevo: memoria de /64 borrada - la cuota se ha renovado")
                .await;
        }
    }

    async fn ajustar_memoria_del_dia(&self) {
        let lineas = self.lineas(&self.rutas.memoria);
        if let Some(recortadas) = ajustar_memoria(&lineas) {
            let texto = recortadas.join("\n");
            let _ = std::fs::write(&self.rutas.memoria, format!("{texto}\n"));
            self.evt("memoria de /64 ajustada: se queda con los ultimos 50")
                .await;
        }
    }

    async fn recordado(&self, bucket: &str) -> bool {
        recordado(&self.lineas(&self.rutas.memoria), bucket)
    }

    async fn locate_warp_cli(&self) -> String {
        if self.rutas.warp_cli.exists() {
            return self.rutas.warp_cli.to_string_lossy().to_string();
        }
        let salida = self
            .mundo
            .ejecutar(
                "powershell",
                &args(&[
                    "-NoProfile",
                    "-Command",
                    "Get-ChildItem -Path ($env:ProgramFiles + '\\Cloudflare') -Filter warp-cli.exe -Recurse -ErrorAction SilentlyContinue | Select-Object -First 1 -ExpandProperty FullName",
                ]),
                Some(ESPERA_POWERSHELL),
            )
            .await;
        salida.lineas().first().map(|linea| linea.to_string()).unwrap_or_default()
    }

    async fn warp_status(&self, cli: &str) -> String {
        self.mundo
            .ejecutar(cli, &args(&["status"]), Some(ESPERA_WARP))
            .await
            .stdout
    }

    async fn warp(&self, cli: &str, verbos: &[&str]) {
        self.mundo
            .ejecutar(cli, &args(verbos), Some(ESPERA_WARP))
            .await;
    }

    async fn esperar_desconexion(&self, cli: &str) -> bool {
        for _ in 0..SONDEO_DESCONEXION {
            let salida = self.warp_status(cli).await;
            if salida.contains("Disconnected") {
                return true;
            }
            self.mundo.dormir(Duration::from_secs(1)).await;
        }
        false
    }

    /// The shield's own trace: one curl fired until an `ip=` shows — exactly the
    /// bat's 20 shots with the 1 s breath between them. A mounted transport
    /// answers first: in proxy mode the machine's curl is not the app's path.
    async fn sondear(&self) -> Option<Traza> {
        if let Some(egress) = &self.egress {
            if let Some(traza) = egress.traza().await {
                return Some(traza);
            }
        }
        for _ in 0..INTENTOS_TRAZA {
            let salida = self
                .mundo
                .ejecutar(
                    "curl",
                    &args(&[
                        "-6",
                        "-s",
                        "--max-time",
                        "8",
                        "https://cloudflare.com/cdn-cgi/trace",
                    ]),
                    None,
                )
                .await;
            if let Some(ip) = trazo("ip", &salida.stdout) {
                return Some(Traza {
                    ip,
                    warp: trazo("warp", &salida.stdout),
                });
            }
            self.mundo.dormir(Duration::from_secs(1)).await;
        }
        None
    }

    /// opencode.ai's trace only proves a pin when curl itself exits clean.
    async fn tracer_ok(&self, segundos: u64) -> bool {
        self.mundo
            .ejecutar(
                "curl",
                &args(&[
                    "-6",
                    "-s",
                    "--max-time",
                    &segundos.to_string(),
                    "https://opencode.ai/cdn-cgi/trace",
                ]),
                None,
            )
            .await
            .status
            == 0
    }

    /// opencode.ai's AAAA at 1.1.1.1; a failure falls to the week's literal.
    async fn dirigir_aaaa(&self) -> Option<String> {
        let salida = self
            .mundo
            .ejecutar(
                "powershell",
                &args(&[
                    "-NoProfile",
                    "-Command",
                    "Resolve-DnsName opencode.ai -Type AAAA -Server 1.1.1.1 -ErrorAction SilentlyContinue | Select-Object -ExpandProperty IP6Address",
                ]),
                Some(ESPERA_DNS),
            )
            .await;
        salida.lineas().first().map(|linea| linea.to_string())
    }

    /// The pin flow verbatim from the bat: fresh bak first, then the candidates
    /// in order, each one written to the hosts, flushed and curl-proved before
    /// it may stay.
    async fn asegurar_pin(&self) {
        if self.pin_vivo() {
            return;
        }
        self.despintar().await;
        let Some(hosts_ahora) = self.leer_hosts() else {
            self.evt("pin: la lectura de hosts fallo - el bak y el pin no se escriben")
                .await;
            return;
        };
        let bak = con_sufijo(&self.rutas.hosts, ".bak");
        let _ = std::fs::write(bak, &hosts_ahora);

        let resuelta = self.dirigir_aaaa().await;
        if resuelta.is_none() {
            self.evt("pin: resolucion AAAA fallo - uso el literal").await;
        }
        let primero = resuelta.clone().unwrap_or_else(|| PIN_DEFECTO.to_string());
        let mut candidatos = vec![primero];
        candidatos.extend(self.ultimos_marcadores());
        candidatos.extend(PIN_LITERALES.iter().map(|pin| pin.to_string()));

        for candidato in candidatos {
            // The pin lands on its own line: a hosts file ending without a
            // newline would glue it onto the user's last entry.
            let Some(previo) = self.leer_hosts() else {
                self.evt("pin: la lectura de hosts fallo - el pin no se escribe")
                    .await;
                return;
            };
            let salto = if !previo.is_empty() && !previo.ends_with(b"\n") {
                "\n"
            } else {
                ""
            };
            let linea = format!("{salto}{candidato} opencode.ai # opencode-pin");
            if !self.anadir_bytes(&self.rutas.hosts, linea.as_bytes()) {
                self.evt("pin: la escritura de hosts fallo - el pin no se escribe")
                    .await;
                return;
            }
            self.flushdns().await;
            if self.tracer_ok(6).await {
                self.anadir(&self.rutas.pin_log, &candidato);
                self.evt(&format!(
                    "pin: {candidato} apunta a opencode.ai - verificado con curl"
                ))
                .await;
                return;
            }
            self.evt(&format!(
                "pin: {candidato} no respondio - pruebo el siguiente candidato"
            ))
            .await;
            if !self.despintar().await {
                self.evt("pin: no pudo quitarse la linea anterior - no apilo otra")
                    .await;
                return;
            }
        }
    }

    fn pin_vivo(&self) -> bool {
        self.leer_hosts().map(|hosts| pin_vivo(&hosts)).unwrap_or(false)
    }

    /// Removes every pinned line, flushes DNS and only counts when a re-read
    /// proves the line is gone.
    async fn despintar(&self) -> bool {
        let Some(texto) = self.leer_hosts() else {
            self.evt("hosts: la lectura fallo - no se reescribe el archivo")
                .await;
            return false;
        };
        let (limpio, quitado) = sin_pin(&texto);
        if quitado && !self.escribir_hosts(&limpio) {
            self.evt("hosts: la escritura fallo - el pin sigue como estaba")
                .await;
            return false;
        }
        self.flushdns().await;
        self.evt("hosts: pin quitado y flushdns").await;
        match self.leer_hosts() {
            Some(despues) => !pin_vivo(&despues),
            None => false,
        }
    }

    async fn flushdns(&self) {
        self.mundo
            .ejecutar(
                "ipconfig",
                &args(&["/flushdns"]),
                Some(ESPERA_RAPIDA),
            )
            .await;
    }

    /// The append-open probe: may this process write the hosts at all?
    fn host_permite(&self) -> bool {
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.rutas.hosts)
            .is_ok()
    }

    async fn contar_app(&self) -> bool {
        let salida = self
            .mundo
            .ejecutar(
                "tasklist",
                &args(&["/FO", "CSV", "/NH"]),
                Some(ESPERA_RAPIDA),
            )
            .await;
        let minusculas = salida.stdout.to_lowercase();
        APP_IMAGENES
            .iter()
            .any(|imagen| minusculas.contains(&format!("\"{imagen}\"")))
    }

    async fn contar_monitores(&self) -> u32 {
        let salida = self
            .mundo
            .ejecutar(
                "powershell",
                &args(&[
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    "@(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object { ($_.CommandLine -ne $null) -and ($_.CommandLine -match 'ghost.epoch') -and ($_.CommandLine -match '-WindowStyle Hidden') -and ($_.ProcessId -ne $PID) }).Count",
                ]),
                Some(ESPERA_POWERSHELL),
            )
            .await;
        salida
            .stdout
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|cuenta| *cuenta > 0)
            .unwrap_or(0)
    }

    fn epoch_nueva(&self) {
        let epoch = format!("{}{}\n", self.mundo.digito(), self.mundo.digito());
        let _ = std::fs::write(&self.rutas.epoch, epoch);
    }

    async fn lanzar_monitor(&self, cli: &str) {
        self.mundo
            .lanzar_oculto(
                "powershell",
                &args(&[
                    "-NoProfile",
                    "-WindowStyle",
                    "Hidden",
                    "-Command",
                    &monitor_command(cli),
                ]),
            )
            .await;
    }

    /// The pin log's newest three candidates.
    fn ultimos_marcadores(&self) -> Vec<String> {
        let lineas = self.lineas(&self.rutas.pin_log);
        let desde = lineas.len().saturating_sub(3);
        lineas[desde..].to_vec()
    }

    fn tomar_cerrojo(&self) -> Option<crate::lock::Cerrojo> {
        let ahora = SystemTime::UNIX_EPOCH + Duration::from_millis(self.mundo.ahora_ms() as u64);
        let pid = self.mundo.pid_propio();
        let mundo = Arc::clone(&self.mundo);
        librar(
            self.rutas.cerrojo.clone(),
            pid,
            move |anterior| mundo.pid_vivo(anterior),
            ahora,
        )
    }

    async fn registrar(
        &self,
        salida: &Rotacion,
        origen: Option<&str>,
        previa: Option<&str>,
        empezado: i64,
        intentos: u32,
    ) {
        let duracion = (self.mundo.ahora_ms() - empezado).max(0) as u64;
        let to = salida
            .address
            .as_deref()
            .and_then(cubo);
        let run = GhostRun::registrar(
            empezado,
            origen,
            salida.ok,
            intentos,
            duracion,
            previa,
            to.as_deref(),
        );
        if let Ok(linea) = serde_json::to_string(&run) {
            self.anadir(&self.rutas.historial, &linea);
        }
    }

    async fn evt(&self, mensaje: &str) {
        let sello = chrono::Local::now().format("%d/%m/%Y %H:%M:%S");
        self.anadir(&self.rutas.eventos, &format!("[{sello}] {mensaje}"));
    }

    // ---- files ------------------------------------------------------------

    fn leer_texto(&self, ruta: &Path) -> Option<String> {
        std::fs::read_to_string(ruta).ok()
    }

    fn lineas(&self, ruta: &Path) -> Vec<String> {
        self.leer_texto(ruta)
            .map(|texto| {
                texto
                    .split(['\r', '\n'])
                    .filter(|linea| !linea.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn anadir(&self, ruta: &Path, linea: &str) {
        use std::io::Write;
        if let Ok(mut archivo) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(ruta)
        {
            let _ = writeln!(archivo, "{linea}");
        }
    }

    fn leer_hosts(&self) -> Option<Vec<u8>> {
        std::fs::read(&self.rutas.hosts).ok()
    }

    fn escribir_hosts(&self, texto: &[u8]) -> bool {
        std::fs::write(&self.rutas.hosts, texto).is_ok()
    }

    fn anadir_bytes(&self, ruta: &Path, bytes: &[u8]) -> bool {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(ruta)
            .and_then(|mut archivo| archivo.write_all(bytes))
            .is_ok()
    }

    async fn soltar_egress(&self) {
        if let Some(egress) = &self.egress {
            egress.soltar().await;
        }
    }

    fn mundo_es_windows(&self) -> bool {
        self.mundo.es_windows()
    }
}

fn args(lista: &[&str]) -> Vec<String> {
    lista.iter().map(|valor| valor.to_string()).collect()
}

fn primera_linea(texto: &str) -> String {
    texto
        .split(['\r', '\n'])
        .next()
        .unwrap_or("")
        .to_string()
}

fn con_sufijo(ruta: &Path, sufijo: &str) -> std::path::PathBuf {
    let mut nombre = ruta.as_os_str().to_os_string();
    nombre.push(sufijo);
    std::path::PathBuf::from(nombre)
}

/// The hidden session monitor, the bat's own one-liner: an epoch swap retires
/// it, its app watch drops the pin when the session's app goes, and ten seconds
/// later it hangs WARP up — leaving the service alone.
pub fn monitor_command(cli: &str) -> String {
    format!(
        "$t0=Get-Date; $e=$env:TEMP+'\\ghost.epoch'; $mine=Get-Content $e -First 1 -ErrorAction SilentlyContinue; Start-Sleep 20; $m=0; while ($m -lt 2) {{ if (((Get-Content $e -First 1 -ErrorAction SilentlyContinue)) -ne $mine) {{ Add-Content ($env:USERPROFILE+'\\ghost-eventos.log') ('['+(Get-Date -Format 'dd/MM/yyyy HH:mm:ss')+'] monitor: retirado por epoch nueva - sesion sustituida o stop manual'); exit }}; if (Get-Process -Name 'OpenCode','OpenCode Dev','Openbot','Openbot Dev','zeron','zeron-engine' -ErrorAction SilentlyContinue) {{ $m=0 }} else {{ $m++ }}; Start-Sleep 3 }}; $h=$env:windir+'\\System32\\drivers\\etc\\hosts'; (Get-Content $h) -notmatch 'opencode-pin' | Out-File -Encoding ascii $h; ipconfig /flushdns; Add-Content ($env:USERPROFILE+'\\ghost-eventos.log') (('['+(Get-Date -Format 'dd/MM/yyyy HH:mm:ss')+'] sesion cerrada: pin fuera de hosts y flushdns (duracion ' + [int]((New-TimeSpan $t0 (Get-Date)).TotalMinutes) + ' min)')); Start-Sleep 10; if (-not (Get-Process -Name 'OpenCode','OpenCode Dev','Openbot','Openbot Dev','zeron','zeron-engine' -ErrorAction SilentlyContinue)) {{ & '{cli}' disconnect; Add-Content ($env:USERPROFILE+'\\ghost-eventos.log') (('['+(Get-Date -Format 'dd/MM/yyyy HH:mm:ss')+'] WARP desconectado - el pin fuera de hosts y el servicio queda para la proxima rotacion (duracion ' + [int]((New-TimeSpan $t0 (Get-Date)).TotalMinutes) + ' min)')) }}"
    )
}

#[cfg(test)]
mod tests;
