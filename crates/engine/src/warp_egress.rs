//! Lane C2 wiring: the agents' egress when the machine already sits in WARP's
//! proxy mode.
//!
//! Nothing here changes WARP's mode — switching is a machine-wide decision the
//! operator (or the ring's own button) makes. At boot, if the local SOCKS5
//! proxy answers *and* `warp-cli` reports proxy mode, the engine opens a
//! loopback HTTP `CONNECT` bridge ([`zeron_warp::egress::Puente`]) and stamps
//! its variables onto every agent spawn, so `opencode serve` — Bun, which only
//! honours HTTP proxies — leaves by the tunnel. In any other mode this is a
//! no-op and runs dial exactly as before.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use zeron_warp::egress::{PROXY_WARP, Puente, PuenteEgress};
use zeron_warp::mundo::{Mundo, Sistema};
use zeron_warp::{Egress, Ghost, Rutas};

use crate::registry::HarnessRegistry;

/// How long boot waits for WARP's local proxy before deciding the machine is
/// not in proxy mode (a closed loopback port refuses at once on Windows).
const SONDA_ARRANQUE: Duration = Duration::from_millis(400);
/// `warp-cli settings` must not hang a boot.
const ESPERA_WARP: Duration = Duration::from_secs(5);

static EGRESS: OnceLock<Arc<PuenteEgress>> = OnceLock::new();

/// The mounted bridge, once the machine booted in WARP's proxy mode. The ring's
/// rotation trigger asks it for the exit and cuts the keep-alives.
pub fn egress() -> Option<Arc<PuenteEgress>> {
    EGRESS.get().cloned()
}

/// The ring over this machine's shared files, riding the mounted bridge when
/// there is one: the rotation then proves the exit the agents' next request
/// will use, not just the shield's own trace.
pub fn ghost() -> Arc<Ghost> {
    let ghost = Ghost::nuevo(Rutas::del_entorno(), Arc::new(Sistema));
    match egress() {
        Some(egress) => Arc::new(ghost.con_egress(egress as Arc<dyn Egress>)),
        None => Arc::new(ghost),
    }
}

/// Opt-out for tests and for machines whose agents must bypass WARP.
fn desactivado() -> bool {
    matches!(
        std::env::var("ZERON_WARP_EGRESS").as_deref(),
        Ok("0") | Ok("off") | Ok("false")
    )
}

/// The automatic rotation on a rejected exit has its own switch: the bridge can
/// be wanted while the ring must not hang up the tunnel under other traffic.
fn rotacion_automatica() -> bool {
    !matches!(
        std::env::var("ZERON_WARP_AUTO_ROTATE").as_deref(),
        Ok("0") | Ok("off") | Ok("false")
    )
}

/// One automatic rotation per window: a provider that keeps answering 429 after
/// a fresh `/64` will not be helped by hanging up the tunnel every retry.
const ESPERA_ENTRE_ROTACIONES: Duration = Duration::from_secs(300);

static ULTIMA_ROTACION: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// The words a rate-limited provider retry carries. The harness forwards the
/// gateway's own message, so the ring recognizes the rejection without parsing
/// provider-specific JSON.
fn es_limite(message: &str) -> bool {
    let minusculas = message.to_lowercase();
    [
        "429",
        "rate limit",
        "rate-limit",
        "ratelimit",
        "too many requests",
        "quota",
        "usage limit",
        "limit reached",
    ]
    .iter()
    .any(|marca| minusculas.contains(marca))
}

/// A provider retry that names a rate limit: rotate the ring once, in the
/// background, so opencode's next attempt leaves by a fresh `/64`. Any other
/// provider fault leaves the tunnel exactly as it was.
pub fn retry_rechazado(message: &str) {
    if desactivado() || !rotacion_automatica() || !es_limite(message) {
        return;
    }
    let ahora = std::time::Instant::now();
    {
        let mut ultima = ULTIMA_ROTACION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ultima.is_some_and(|antes| ahora.duration_since(antes) < ESPERA_ENTRE_ROTACIONES) {
            return;
        }
        *ultima = Some(ahora);
    }
    tokio::spawn(async move {
        let rotacion = ghost().rotar(Some("limit")).await;
        if rotacion.ok {
            tracing::info!(
                salida = rotacion.address.as_deref().unwrap_or("desconocida"),
                "warp: el gateway rechazo la salida; ya hay otro /64"
            );
        } else {
            tracing::warn!("warp: el gateway rechazo la salida y la rotacion no aterrizo");
        }
    });
}

/// Boot: mount the bridge when WARP's proxy is the path, and hand its variables
/// to every run this registry prepares.
pub async fn montar_en(registry: &HarnessRegistry) {
    if desactivado() {
        return;
    }
    montar_con(PROXY_WARP, Arc::new(Sistema), registry).await;
}

/// [`montar_en`] with an injectable proxy address and machine (tests).
pub async fn montar_con(
    proxy: &str,
    mundo: Arc<dyn Mundo>,
    registry: &HarnessRegistry,
) -> Option<Arc<PuenteEgress>> {
    if registry.warp_environment().is_some() {
        return egress();
    }
    if !proxy_vivo(proxy).await || !modo_proxy(mundo.as_ref()).await {
        return None;
    }
    let puente = match Puente::abrir(proxy).await {
        Ok(puente) => puente,
        Err(error) => {
            tracing::warn!(%error, "warp: no pude abrir el puente del egress");
            return None;
        }
    };
    let egress = Arc::new(PuenteEgress::nuevo(puente, mundo));
    let _ = EGRESS.set(Arc::clone(&egress));
    registry.set_warp_environment(egress.entorno());
    tracing::info!(
        puerto = egress.addr().port(),
        proxy,
        "warp: el egress de los agentes sale por el proxy local"
    );
    Some(egress)
}

async fn proxy_vivo(proxy: &str) -> bool {
    matches!(
        tokio::time::timeout(SONDA_ARRANQUE, tokio::net::TcpStream::connect(proxy)).await,
        Ok(Ok(_))
    )
}

/// The mode read from `warp-cli`, not just a listener: any SOCKS5 on 40000 must
/// not be mistaken for WARP's tunnel.
async fn modo_proxy(mundo: &dyn Mundo) -> bool {
    let salida = mundo
        .ejecutar(
            "warp-cli",
            &["--accept-tos".to_string(), "settings".to_string()],
            Some(ESPERA_WARP),
        )
        .await;
    salida
        .stdout
        .to_lowercase()
        .lines()
        .any(|linea| linea.replace([' ', '\t'], "").contains("mode:proxy"))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tokio::net::TcpListener;

    use super::*;
    use zeron_warp::shell::Salida;

    struct MundoGuion {
        respuestas: Mutex<VecDeque<String>>,
    }

    impl MundoGuion {
        fn con(respuestas: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                respuestas: Mutex::new(respuestas.iter().map(|r| r.to_string()).collect()),
            })
        }
    }

    #[async_trait]
    impl Mundo for MundoGuion {
        async fn ejecutar(
            &self,
            _programa: &str,
            _args: &[String],
            _espera: Option<Duration>,
        ) -> Salida {
            Salida {
                status: 0,
                stdout: self
                    .respuestas
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_default(),
            }
        }

        async fn dormir(&self, _espera: Duration) {}
        fn ahora_ms(&self) -> i64 {
            0
        }
        fn hoy_utc(&self) -> String {
            "2026-11-18".to_string()
        }
        fn pid_vivo(&self, _pid: u32) -> bool {
            false
        }
        fn pid_propio(&self) -> u32 {
            1
        }
        fn digito(&self) -> u32 {
            1
        }
        fn es_windows(&self) -> bool {
            true
        }
        async fn lanzar_oculto(&self, _programa: &str, _args: &[String]) {}
    }

    fn ajustes(modo: &str) -> String {
        format!("(user set)\tMode: {modo}\n")
    }

    #[tokio::test]
    async fn sin_proxy_escuchando_no_hay_egress() {
        // Nothing listens: the port is closed, so the bridge never opens.
        let registry = HarnessRegistry::new();
        let mundo = MundoGuion::con(&[&ajustes("Proxy")]);
        assert!(montar_con("127.0.0.1:9", mundo, &registry).await.is_none());
        assert!(registry.warp_environment().is_none());
    }

    #[tokio::test]
    async fn un_puerto_abierto_que_no_es_warp_no_es_egress() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let registry = HarnessRegistry::new();
        let mundo = MundoGuion::con(&[&ajustes("Warp")]);
        assert!(
            montar_con(&addr.to_string(), mundo, &registry)
                .await
                .is_none()
        );
        assert!(registry.warp_environment().is_none());
    }

    #[tokio::test]
    async fn con_warp_en_proxy_el_entorno_ride_el_puente() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let registry = HarnessRegistry::new();
        let mundo = MundoGuion::con(&[&ajustes("Proxy")]);

        let egress = montar_con(&addr.to_string(), mundo, &registry)
            .await
            .expect("el proxy local y el modo proxy montan el puente");

        let vars = registry
            .warp_environment()
            .expect("el registro lleva el entorno");
        let https = vars
            .iter()
            .find(|(clave, _)| clave == "HTTPS_PROXY")
            .map(|(_, valor)| valor.clone())
            .unwrap();
        assert_eq!(https, format!("http://{}", egress.addr()));
        assert_ne!(egress.addr(), addr, "el puente escucha en su propio puerto");
        assert!(
            vars.iter()
                .any(|(clave, valor)| clave == "NO_PROXY" && valor.contains("localhost")),
            "loopback debe quedar fuera del proxy"
        );
    }

    #[test]
    fn el_chip_de_reintento_dice_cuando_es_un_limite() {
        // opencode forwards the provider's own words; these are the shapes a
        // gateway's 429 takes in practice.
        for mensaje in [
            "AI_APICallError: 429 Too Many Requests",
            "Rate limit reached for requests",
            "You have exceeded your weekly usage limit",
            "insufficient_quota: quota exceeded",
            "rate-limit exceeded",
        ] {
            assert!(es_limite(mensaje), "{mensaje}");
        }
        for mensaje in [
            "The provider is failing and opencode is retrying (attempt 3): connection reset",
            "AI_APICallError: 500 Internal Server Error",
            "invalid api key",
            "model overloaded",
        ] {
            assert!(!es_limite(mensaje), "{mensaje}");
        }
    }

    #[test]
    fn la_rotacion_automatica_se_apaga_por_entorno() {
        // The matcher never spawns from here: with the switch off, a limit
        // message is ignored before any work is scheduled.
        // SAFETY: this test owns the variable for its duration.
        unsafe { std::env::set_var("ZERON_WARP_AUTO_ROTATE", "off") };
        assert!(!rotacion_automatica());
        retry_rechazado("429 Too Many Requests");
        unsafe { std::env::remove_var("ZERON_WARP_AUTO_ROTATE") };
        assert!(rotacion_automatica());
    }
}
