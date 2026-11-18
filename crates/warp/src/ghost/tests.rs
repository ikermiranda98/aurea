//! The ladder, whole, over a scripted machine.
//!
//! Every test gets its own directory under a tempdir: the hosts file, the
//! memory, the day file, the lock and the epoch are the test's, never the
//! user's. The [`MundoFalso`] answers commands by program and records what the
//! ring asked for.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tempfile::TempDir;

use super::*;
use crate::mundo::Mundo;
use crate::rutas::Rutas;
use crate::shell::Salida;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Llamada {
    programa: String,
    args: Vec<String>,
}

type Responder = Box<dyn Fn(&str, &[String]) -> Salida + Send + Sync>;

struct MundoFalso {
    responder: Responder,
    llamadas: Mutex<Vec<Llamada>>,
    dormidas: Mutex<Vec<Duration>>,
    lanzados: Mutex<Vec<Llamada>>,
    reloj: Mutex<i64>,
    hoy: Mutex<String>,
    vivos: Vec<u32>,
    windows: bool,
}

impl MundoFalso {
    fn llamadas(&self) -> Vec<Llamada> {
        self.llamadas.lock().unwrap().clone()
    }

    fn veces(&self, programa: &str, arg: &str) -> usize {
        self.llamadas()
            .iter()
            .filter(|llamada| {
                Path::new(&llamada.programa).file_name().and_then(|n| n.to_str()) == Some(programa)
                    && llamada.args.iter().any(|a| a == arg)
            })
            .count()
    }

    fn dormidas(&self) -> Vec<Duration> {
        self.dormidas.lock().unwrap().clone()
    }

    fn lanzados(&self) -> Vec<Llamada> {
        self.lanzados.lock().unwrap().clone()
    }
}

#[async_trait]
impl Mundo for MundoFalso {
    async fn ejecutar(&self, programa: &str, args: &[String], _espera: Option<Duration>) -> Salida {
        self.llamadas.lock().unwrap().push(Llamada {
            programa: programa.to_string(),
            args: args.to_vec(),
        });
        (self.responder)(programa, args)
    }

    async fn dormir(&self, espera: Duration) {
        self.dormidas.lock().unwrap().push(espera);
    }

    fn ahora_ms(&self) -> i64 {
        *self.reloj.lock().unwrap()
    }

    fn hoy_utc(&self) -> String {
        self.hoy.lock().unwrap().clone()
    }

    fn pid_vivo(&self, pid: u32) -> bool {
        self.vivos.contains(&pid)
    }

    fn pid_propio(&self) -> u32 {
        std::process::id()
    }

    fn digito(&self) -> u32 {
        7
    }

    fn es_windows(&self) -> bool {
        self.windows
    }

    async fn lanzar_oculto(&self, programa: &str, args: &[String]) {
        self.lanzados.lock().unwrap().push(Llamada {
            programa: programa.to_string(),
            args: args.to_vec(),
        });
    }
}

fn mundo(
    responder: impl Fn(&str, &[String]) -> Salida + Send + Sync + 'static,
) -> Arc<MundoFalso> {
    let responder: Responder = Box::new(responder);
    Arc::new(MundoFalso {
        responder,
        llamadas: Mutex::new(Vec::new()),
        dormidas: Mutex::new(Vec::new()),
        lanzados: Mutex::new(Vec::new()),
        reloj: Mutex::new(1_700_000_000_000),
        hoy: Mutex::new("2026-11-18".to_string()),
        vivos: Vec::new(),
        windows: true,
    })
}

fn tiene(args: &[String], aguja: &str) -> bool {
    args.iter().any(|arg| arg.contains(aguja))
}

fn ok(stdout: &str) -> Salida {
    Salida::bien(stdout)
}

/// The ordinary answers: a live app, no monitors, a warp-cli that is up, DNS
/// resolving, and the trace showing what `traza` says.
fn mundo_base(traza: impl Fn(u32) -> String + Send + Sync + 'static) -> Arc<MundoFalso> {
    let turno = AtomicU32::new(0);
    mundo(move |programa, args| match programa {
        "net" => ok(""),
        "tasklist" => ok("\"OpenCode.exe\",\"4242\"\n"),
        "powershell" if tiene(args, "Get-CimInstance") => ok("0\n"),
        "powershell" if tiene(args, "Resolve-DnsName") => ok("2606:4700:78::90:0:141\n"),
        "powershell" => ok(""),
        "curl" if tiene(args, "cloudflare.com") => Salida::bien(traza(turno.fetch_add(1, Ordering::SeqCst))),
        "curl" => ok(""),
        "ipconfig" => ok(""),
        _ => {
            if args.first().map(String::as_str) == Some("status") {
                ok("Status update: Disconnected\n")
            } else {
                ok("")
            }
        }
    })
}

fn preparar() -> (TempDir, Rutas) {
    let dir = tempfile::tempdir().unwrap();
    let rutas = Rutas::en(dir.path());
    std::fs::write(&rutas.warp_cli, b"").unwrap();
    std::fs::write(&rutas.hosts, b"127.0.0.1 localhost\n").unwrap();
    (dir, rutas)
}

fn leer(ruta: &Path) -> String {
    std::fs::read_to_string(ruta).unwrap_or_default()
}

#[tokio::test]
async fn a_new_utc_day_wipes_the_memory_before_the_ladder() {
    let (_dir, rutas) = preparar();
    std::fs::write(&rutas.dia, "2026-11-17\n").unwrap();
    std::fs::write(&rutas.memoria, "viejo:0:0:0\notro:0:0:0\n").unwrap();
    let mundo = mundo_base(|_| {
        "ip=2a09:bac5:6de7:31e1:0:0:0:1\nwarp=on\n".to_string()
    });
    let ghost = Ghost::nuevo(rutas.clone(), mundo);

    let salida = ghost.rotar(None).await;

    assert!(salida.ok);
    let memoria = leer(&rutas.memoria);
    assert!(!memoria.contains("viejo:0:0:0"), "{memoria}");
    assert!(memoria.contains("2a09:bac5:6de7:31e1"));
    assert!(leer(&rutas.dia).starts_with("2026-11-18"));
    assert!(leer(&rutas.eventos).contains("dia UTC nuevo"));
}

#[tokio::test]
async fn a_bucket_already_in_the_memory_is_retried_until_a_new_one_lands() {
    let (_dir, rutas) = preparar();
    std::fs::write(&rutas.dia, "2026-11-18\n").unwrap();
    std::fs::write(&rutas.memoria, "2a09:bac5:6de7:31e1\n").unwrap();
    let mundo = mundo_base(|turno| {
        if turno == 0 {
            "ip=2a09:bac5:6de7:31e1:0:0:0:2\nwarp=on\n".to_string()
        } else {
            "ip=2606:4700:78:9:0:0:0:3\nwarp=on\n".to_string()
        }
    });
    let ghost = Ghost::nuevo(rutas.clone(), mundo);
    let salida = ghost.rotar(None).await;

    assert!(salida.ok, "second attempt lands");
    let eventos = leer(&rutas.eventos);
    assert!(eventos.contains("intento 1: /64 2a09:bac5:6de7:31e1 repetido"), "{eventos}");
    assert!(salida.address.unwrap().starts_with("2606:4700:78:9"));
    assert!(leer(&rutas.memoria).contains("2606:4700:78:9"));
}

#[tokio::test]
async fn ten_repeated_buckets_exhaust_the_pool() {
    let (_dir, rutas) = preparar();
    std::fs::write(&rutas.dia, "2026-11-18\n").unwrap();
    std::fs::write(&rutas.memoria, "2a09:bac5:6de7:31e1\n").unwrap();
    let mundo = mundo_base(|_| "ip=2a09:bac5:6de7:31e1:0:0:0:1\nwarp=on\n".to_string());
    let ghost = Ghost::nuevo(rutas.clone(), mundo.clone());
    let salida = ghost.rotar(None).await;

    assert!(!salida.ok);
    assert_eq!(ghost.attempt.load(Ordering::SeqCst), MAX_INTENTOS);
    let eventos = leer(&rutas.eventos);
    assert!(eventos.contains("pool agotado: 10 bloques repetidos"), "{eventos}");
    // The losing ladder hangs the tunnel up.
    let desconexiones = mundo.veces("warp-cli.exe", "disconnect");
    assert_eq!(desconexiones, MAX_INTENTOS as usize + 1);
}

#[tokio::test]
async fn a_live_lock_retires_the_run_without_touching_warp() {
    let (_dir, rutas) = preparar();
    std::fs::write(&rutas.cerrojo, "4242").unwrap();
    let mut falso = MundoFalso {
        responder: Box::new(|_, _| ok("")),
        llamadas: Mutex::new(Vec::new()),
        dormidas: Mutex::new(Vec::new()),
        lanzados: Mutex::new(Vec::new()),
        reloj: Mutex::new(1_700_000_000_000),
        hoy: Mutex::new("2026-11-18".to_string()),
        vivos: vec![4242],
        windows: true,
    };
    falso.vivos = vec![4242];
    let mundo = Arc::new(falso);
    let ghost = Ghost::nuevo(rutas.clone(), mundo.clone());

    let salida = ghost.rotar(None).await;

    assert!(!salida.ok);
    assert!(mundo.llamadas().is_empty());
    assert!(leer(&rutas.eventos).contains("ya hay un run en marcha"));
}

#[tokio::test]
async fn without_a_warp_cli_the_ring_refuses_to_start() {
    let (_dir, rutas) = preparar();
    std::fs::remove_file(&rutas.warp_cli).unwrap();
    let mundo = mundo(|programa, _| match programa {
        "powershell" => ok(""),
        _ => ok(""),
    });
    let ghost = Ghost::nuevo(rutas.clone(), mundo);

    let salida = ghost.rotar(None).await;

    assert!(!salida.ok);
    assert!(leer(&rutas.eventos).contains("warp-cli no encontrado"));
}

#[tokio::test]
async fn a_silent_daemon_aborts_before_the_ladder() {
    let (_dir, rutas) = preparar();
    let mundo = mundo(|_, _| ok(""));
    let ghost = Ghost::nuevo(rutas.clone(), mundo);
    let salida = ghost.rotar(None).await;

    assert!(!salida.ok);
    assert!(leer(&rutas.eventos).contains("daemon WARP inalcanzable"));
}

#[tokio::test]
async fn every_attempt_without_an_ip_re_registers_and_the_ladder_gives_up() {
    let (_dir, rutas) = preparar();
    std::fs::write(&rutas.dia, "2026-11-18\n").unwrap();
    let intentos = Arc::new(AtomicU32::new(0));
    let turno = Arc::clone(&intentos);
    let mundo = mundo_base(move |_| {
        turno.fetch_add(1, Ordering::SeqCst);
        String::new()
    });
    let ghost = Ghost::nuevo(rutas.clone(), mundo.clone());
    let salida = ghost.rotar(None).await;

    assert!(!salida.ok, "ten attempts without an IP never land");
    assert_eq!(mundo.veces("warp-cli.exe", "delete"), MAX_INTENTOS as usize);
    assert_eq!(mundo.veces("warp-cli.exe", "new"), MAX_INTENTOS as usize);
    let dormidas = mundo.dormidas();
    assert!(dormidas.contains(&Duration::from_secs(3)));
    assert!(dormidas.contains(&Duration::from_secs(5)));
    let eventos = leer(&rutas.eventos);
    assert!(eventos.contains("rescate: re-registro de WARP"), "{eventos}");
    assert!(eventos.contains("fin sin conectar: 10 intentos sin IP"), "{eventos}");
}

#[tokio::test]
async fn a_landing_without_an_egress_keeps_the_trace_verdict() {
    let (_dir, rutas) = preparar();
    std::fs::write(&rutas.dia, "2026-11-18\n").unwrap();
    let mundo = mundo_base(|_| "ip=2a09:bac5:6de7:31e1:0:0:0:1\nwarp=on\n".to_string());
    let ghost = Ghost::nuevo(rutas.clone(), mundo);
    let salida = ghost.rotar(Some("manual")).await;

    assert!(salida.ok);
    assert_eq!(salida.address.as_deref(), Some("2a09:bac5:6de7:31e1:0:0:0:1"));
    assert!(!leer(&rutas.hosts).contains("opencode-pin"), "no egress, no pin");
    assert_eq!(leer(&rutas.historial).lines().count(), 1);
    let run: GhostRun = serde_json::from_str(leer(&rutas.historial).trim()).unwrap();
    assert!(run.ok);
    assert_eq!(run.origin, "manual");
    assert_eq!(run.to.as_deref(), Some("2a09:bac5:6de7:31e1"));
    assert_eq!(run.attempts, 1);
}

struct EgressFalso {
    apuntado: Mutex<Option<Vec<String>>>,
    soltado: Mutex<u32>,
    salida: Option<String>,
}

#[async_trait]
impl Egress for EgressFalso {
    async fn apuntar(&self, direcciones: &[String]) -> Option<String> {
        *self.apuntado.lock().unwrap() = Some(direcciones.to_vec());
        self.salida.clone()
    }

    async fn soltar(&self) {
        *self.soltado.lock().unwrap() += 1;
    }
}

#[tokio::test]
async fn a_landing_with_an_egress_pins_hosts_and_starts_the_monitor() {
    let (_dir, rutas) = preparar();
    std::fs::write(&rutas.dia, "2026-11-18\n").unwrap();
    let mundo = mundo_base(|_| "ip=2a09:bac5:6de7:31e1:0:0:0:1\nwarp=on\n".to_string());
    let egress = Arc::new(EgressFalso {
        apuntado: Mutex::new(None),
        soltado: Mutex::new(0),
        salida: Some("2606:4700:78::90:0:141".to_string()),
    });
    let ghost =
        Ghost::nuevo(rutas.clone(), mundo.clone()).con_egress(Arc::clone(&egress) as Arc<dyn Egress>);
    let salida = ghost.rotar(None).await;

    assert!(salida.ok, "{}", leer(&rutas.eventos));
    let hosts = leer(&rutas.hosts);
    assert!(hosts.contains("2606:4700:78::90:0:141 opencode.ai # opencode-pin"), "{hosts}");
    assert!(hosts.starts_with("127.0.0.1 localhost\n"));
    assert_eq!(leer(&rutas.pin_log).trim(), "2606:4700:78::90:0:141");
    assert!(Path::new(&rutas.epoch).exists());
    // One flush takes the old pin off, another proves the new one.
    assert_eq!(mundo.veces("ipconfig", "/flushdns"), 2);
    let lanzados = mundo.lanzados();
    assert_eq!(lanzados.len(), 1);
    assert!(lanzados[0].args.iter().any(|arg| arg.contains("ghost.epoch")));
    let apuntado = egress.apuntado.lock().unwrap().clone().unwrap();
    assert_eq!(apuntado[0], "2606:4700:78::90:0:141");
    assert_eq!(apuntado.len(), 4, "resolve plus the three literals");
}

#[tokio::test]
async fn an_egress_that_cannot_point_fails_the_rotation_and_unpins() {
    let (_dir, rutas) = preparar();
    std::fs::write(&rutas.dia, "2026-11-18\n").unwrap();
    std::fs::write(
        &rutas.hosts,
        b"127.0.0.1 localhost\n2606:4700:78::90:0:140 opencode.ai # opencode-pin\n",
    )
    .unwrap();
    let mundo = mundo_base(|_| "ip=2a09:bac5:6de7:31e1:0:0:0:1\nwarp=on\n".to_string());
    let egress = Arc::new(EgressFalso {
        apuntado: Mutex::new(None),
        soltado: Mutex::new(0),
        salida: None,
    });
    let ghost = Ghost::nuevo(rutas.clone(), mundo).con_egress(egress as Arc<dyn Egress>);
    let salida = ghost.rotar(None).await;

    assert!(!salida.ok);
    assert!(!leer(&rutas.hosts).contains("opencode-pin"));
    assert!(leer(&rutas.eventos).contains("la rotacion no sirve a la app"));
}

#[tokio::test]
async fn release_takes_the_pin_out_and_hangs_the_tunnel_up() {
    let (_dir, rutas) = preparar();
    std::fs::write(
        &rutas.hosts,
        b"127.0.0.1 localhost\n2606:4700:78::90:0:141 opencode.ai # opencode-pin\n",
    )
    .unwrap();
    let mundo = mundo(|programa, args| match programa {
        "ipconfig" => ok(""),
        _ => {
            if args.first().map(String::as_str) == Some("status") {
                ok("Status update: Disconnected\n")
            } else {
                ok("")
            }
        }
    });
    let egress = Arc::new(EgressFalso {
        apuntado: Mutex::new(None),
        soltado: Mutex::new(0),
        salida: None,
    });
    let ghost = Ghost::nuevo(rutas.clone(), mundo.clone())
        .con_egress(Arc::clone(&egress) as Arc<dyn Egress>);

    let salida = ghost.soltar().await;

    assert!(salida.ok);
    assert!(!leer(&rutas.hosts).contains("opencode-pin"));
    assert_eq!(*egress.soltado.lock().unwrap(), 1, "the egress goes before the tunnel");
    assert_eq!(mundo.veces("warp-cli.exe", "disconnect"), 1);
    assert!(leer(&rutas.eventos).contains("WARP desconectado"));
}

#[tokio::test]
async fn release_refuses_when_the_hosts_file_cannot_be_read() {
    let (_dir, rutas) = preparar();
    std::fs::remove_file(&rutas.hosts).unwrap();
    std::fs::create_dir_all(&rutas.hosts).unwrap();
    let mundo = mundo(|_, _| ok(""));
    let ghost = Ghost::nuevo(rutas.clone(), mundo.clone());

    let salida = ghost.soltar().await;

    assert!(!salida.ok);
    assert!(mundo.llamadas().is_empty());
    assert!(leer(&rutas.eventos).contains("WARP se deja arriba"));
}

#[tokio::test]
async fn status_reads_the_shared_files_and_never_writes() {
    let (_dir, rutas) = preparar();
    std::fs::write(&rutas.dia, "2026-11-18\n").unwrap();
    std::fs::write(&rutas.memoria, "a:0:0:0\nb:0:0:0\n").unwrap();
    std::fs::write(
        &rutas.hosts,
        b"2606:4700:78::90:0:141 opencode.ai # opencode-pin\n",
    )
    .unwrap();
    let mundo = mundo(|programa, args| match programa {
        "sc" => ok("STATE : 4 RUNNING\n"),
        _ => {
            if args.first().map(String::as_str) == Some("status") {
                ok("Status update: Connected\n")
            } else {
                ok("")
            }
        }
    });
    let ghost = Ghost::nuevo(rutas.clone(), mundo.clone());
    let antes = leer(&rutas.eventos);

    let estado = ghost.estado().await;

    assert_eq!(estado.warp, EstadoWarp::Connected);
    assert!(!estado.rotating);
    assert_eq!(estado.attempt, None);
    assert_eq!(estado.attempts, MAX_INTENTOS);
    assert_eq!(estado.rotations, 2);
    assert_eq!(estado.current.as_deref(), Some("b:0:0:0"));
    assert!(estado.pin);
    assert_eq!(leer(&rutas.eventos), antes, "status writes nothing");
}

#[tokio::test]
async fn status_counts_no_rotations_on_a_stale_day() {
    let (_dir, rutas) = preparar();
    std::fs::write(&rutas.dia, "2026-11-17\n").unwrap();
    std::fs::write(&rutas.memoria, "a:0:0:0\n").unwrap();
    let mundo = mundo(|_, _| ok(""));
    let ghost = Ghost::nuevo(rutas.clone(), mundo);

    let estado = ghost.estado().await;

    assert_eq!(estado.rotations, 0);
    assert_eq!(estado.current, None);
}

#[tokio::test]
async fn history_is_seeded_from_the_event_log_on_its_first_read() {
    let (_dir, rutas) = preparar();
    let hoy = chrono::Local::now().format("%d/%m/%Y").to_string();
    std::fs::write(
        &rutas.eventos,
        format!("[{hoy} 01:02:03] rotate: /64 nuevo 2a09:bac5:6de7:31e1\n"),
    )
    .unwrap();
    let mundo = mundo(|_, _| ok(""));
    let ghost = Ghost::nuevo(rutas.clone(), mundo);

    let historial = ghost.historial().await;

    assert_eq!(historial.len(), 1);
    assert_eq!(historial[0].to.as_deref(), Some("2a09:bac5:6de7:31e1"));
    assert!(rutas.historial.exists());
}

#[tokio::test]
async fn a_disabled_ring_does_nothing() {
    let (_dir, rutas) = preparar();
    let mundo = mundo(|_, _| ok(""));
    let ghost = Ghost::nuevo(rutas.clone(), mundo.clone());
    ghost.activar(false);

    let salida = ghost.rotar(None).await;

    assert!(!salida.ok);
    assert!(mundo.llamadas().is_empty());
    assert!(leer(&rutas.eventos).contains("deshabilitado por flag"));
}

#[test]
fn the_monitor_carries_the_cli_and_the_hidden_window_style() {
    let comando = monitor_command(r"C:\Program Files\Cloudflare\warp-cli.exe");
    assert!(comando.contains("ghost.epoch"));
    assert!(comando.contains("warp-cli.exe' disconnect"));
}
