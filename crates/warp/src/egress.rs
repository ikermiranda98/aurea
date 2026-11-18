//! Lane C2: the hot egress the gateway rides, over WARP's local proxy.
//!
//! WARP 2026.8 only speaks SOCKS5 (`warp-cli mode proxy`, default port 40000)
//! and Bun — the runtime under `opencode serve` — only honours HTTP proxies in
//! `HTTPS_PROXY`; it explicitly ignores a `socks5://` value. The app therefore
//! needs its own hop: [`Puente`], a loopback HTTP `CONNECT` proxy that forwards
//! every tunnel to WARP's SOCKS5, resolving names *at the proxy* (socks5h), so
//! the sidecar never needs the machine's resolver — twice disabled while WARP
//! sits in proxy mode.
//!
//! [`PuenteEgress`] mounts that bridge as the ring's [`Egress`]: it answers the
//! trace the app's own traffic sees (ghost's machine `curl` would leave by the
//! router and report `warp=off`), and a rotation cuts every live tunnel so the
//! next request re-dials through the fresh `/64` instead of reusing keep-alive.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::ghost::{Egress, Traza};
use crate::mundo::Mundo;
use crate::shell::trazo;

/// WARP's local SOCKS5 listener in proxy mode.
pub const PROXY_WARP: &str = "127.0.0.1:40000";
/// The trace both Openbot and this crate trust; the bridge relays it.
const TRAZA_URL: &str = "https://cloudflare.com/cdn-cgi/trace";
/// A header bigger than this is not a proxy request, it is noise.
const MAX_CABECERA: usize = 8 * 1024;
/// Shots at the trace before a probe gives up (1 s apart, like ghost's).
const INTENTOS_SONDA: u32 = 3;

/// A loopback HTTP `CONNECT` proxy that tunnels through WARP's SOCKS5.
///
/// One listener per app run, opened on an ephemeral port: the sidecar gets
/// `http://127.0.0.1:<port>` and the machine's own traffic is left alone.
pub struct Puente {
    addr: SocketAddr,
    proxy: String,
    corte: broadcast::Sender<()>,
    destinos: Arc<Mutex<Vec<String>>>,
    tarea: JoinHandle<()>,
}

impl Puente {
    /// Listens on `127.0.0.1:0` and forwards to `proxy` (`host:port`, with or
    /// without the `socks5h://` scheme).
    pub async fn abrir(proxy: impl Into<String>) -> io::Result<Self> {
        Self::abrir_en(SocketAddr::from(([127, 0, 0, 1], 0)), proxy).await
    }

    /// The same bridge on a chosen loopback address (the live check pins the
    /// port so another process can be pointed at it from the command line).
    pub async fn abrir_en(escucha: SocketAddr, proxy: impl Into<String>) -> io::Result<Self> {
        let proxy = proxy.into();
        normalizar_proxy(&proxy)?;
        let listener = TcpListener::bind(escucha).await?;
        let addr = listener.local_addr()?;
        let (corte, _) = broadcast::channel(16);
        let destinos = Arc::new(Mutex::new(Vec::new()));
        let tarea = {
            let proxy = proxy.clone();
            let destinos = Arc::clone(&destinos);
            let corte_tarea = corte.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((cliente, _)) = listener.accept().await else {
                        return;
                    };
                    let proxy = proxy.clone();
                    let destinos = Arc::clone(&destinos);
                    let corte = corte_tarea.subscribe();
                    tokio::spawn(atender(cliente, proxy, destinos, corte));
                }
            })
        };
        Ok(Self {
            addr,
            proxy,
            corte,
            destinos,
            tarea,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The SOCKS5 hop this bridge forwards to.
    pub fn proxy(&self) -> &str {
        &self.proxy
    }

    /// `host:port` of every tunnel this bridge opened, in order: the sidecar's
    /// own traffic, visible for the log and for the live check.
    pub fn destinos(&self) -> Vec<String> {
        self.destinos
            .lock()
            .map(|lista| lista.clone())
            .unwrap_or_default()
    }

    /// Drops every live tunnel. The listeners stay: the next request re-dials,
    /// which is exactly what a rotation needs after a new `/64` lands.
    pub fn cortar(&self) {
        let _ = self.corte.send(());
    }

    /// The `HTTPS_PROXY`/`NO_PROXY` set the sidecar must boot with.
    pub fn entorno(&self) -> Vec<(String, String)> {
        entorno(self.addr.port())
    }
}

impl Drop for Puente {
    fn drop(&mut self) {
        self.tarea.abort();
    }
}

/// The environment that points a child process (the `opencode serve` sidecar)
/// at this bridge while keeping loopback traffic off it.
///
/// Bun reads `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` (either case) on every
/// fetch and skips hosts listed in `NO_PROXY`; the engine's own IPC with the
/// sidecar lives on `127.0.0.1`, so it must bypass the tunnel.
pub fn entorno(puerto: u16) -> Vec<(String, String)> {
    let url = format!("http://127.0.0.1:{puerto}");
    let locales = "127.0.0.1,localhost,::1,[::1]";
    vec![
        ("HTTPS_PROXY".to_string(), url.clone()),
        ("https_proxy".to_string(), url.clone()),
        ("HTTP_PROXY".to_string(), url.clone()),
        ("http_proxy".to_string(), url.clone()),
        ("ALL_PROXY".to_string(), url),
        ("NO_PROXY".to_string(), locales.to_string()),
        ("no_proxy".to_string(), locales.to_string()),
    ]
}

/// The ring's egress over the bridge: probes what the app's traffic sees and
/// cuts the tunnels on rotation or release.
pub struct PuenteEgress {
    puente: Puente,
    mundo: Arc<dyn Mundo>,
}

impl PuenteEgress {
    pub fn nuevo(puente: Puente, mundo: Arc<dyn Mundo>) -> Self {
        Self { puente, mundo }
    }

    pub fn addr(&self) -> SocketAddr {
        self.puente.addr()
    }

    pub fn entorno(&self) -> Vec<(String, String)> {
        self.puente.entorno()
    }

    pub fn destinos(&self) -> Vec<String> {
        self.puente.destinos()
    }

    /// The trace as this bridge leaves it, shots 1 s apart until an `ip=` shows.
    pub async fn sonda(&self) -> Option<Traza> {
        let proxy = format!("http://{}", self.puente.addr());
        for intento in 0..INTENTOS_SONDA {
            let salida = self
                .mundo
                .ejecutar(
                    "curl",
                    &args(&["-sS", "--max-time", "8", "--proxy", &proxy, TRAZA_URL]),
                    None,
                )
                .await;
            if let Some(ip) = trazo("ip", &salida.stdout) {
                return Some(Traza {
                    ip,
                    warp: trazo("warp", &salida.stdout),
                });
            }
            if intento + 1 < INTENTOS_SONDA {
                self.mundo.dormir(Duration::from_secs(1)).await;
            }
        }
        None
    }
}

#[async_trait]
impl Egress for PuenteEgress {
    /// A fresh `/64` is only a rotation once the app's *next* request can leave
    /// by it: drop the old keep-alives, then prove the exit over the bridge.
    /// The candidate addresses are not dialled here — the proxy picks the path
    /// and the trace is the verdict — but they stay in the signature C1 fixed.
    async fn apuntar(&self, _direcciones: &[String]) -> Option<String> {
        self.puente.cortar();
        let traza = self.sonda().await?;
        if !traza.ip.contains(':') || traza.warp.as_deref() != Some("on") {
            return None;
        }
        Some(traza.ip)
    }

    async fn soltar(&self) {
        self.puente.cortar();
    }

    async fn traza(&self) -> Option<Traza> {
        self.sonda().await
    }
}

/// One client connection: `CONNECT host:port`, then bytes both ways.
async fn atender(
    mut cliente: TcpStream,
    proxy: String,
    destinos: Arc<Mutex<Vec<String>>>,
    mut corte: broadcast::Receiver<()>,
) {
    let Ok((cabecera, resto)) = leer_cabecera(&mut cliente).await else {
        return;
    };
    let pedido = cabecera.lines().next().unwrap_or_default();
    let Some((host, puerto)) = destino_connect(pedido) else {
        let _ = cliente
            .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
            .await;
        return;
    };
    if let Ok(mut lista) = destinos.lock() {
        lista.push(format!("{host}:{puerto}"));
    }
    let salida = tokio::select! {
        conectado = conectar_socks(&proxy, &host, puerto) => conectado,
        _ = corte.recv() => return,
    };
    let mut salida = match salida {
        Ok(salida) => salida,
        Err(_) => {
            let _ = cliente
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n")
                .await;
            return;
        }
    };
    if cliente
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
    {
        return;
    }
    if !resto.is_empty() && salida.write_all(&resto).await.is_err() {
        return;
    }
    tokio::select! {
        _ = tokio::io::copy_bidirectional(&mut cliente, &mut salida) => {}
        _ = corte.recv() => {}
    }
}

async fn leer_cabecera(cliente: &mut TcpStream) -> io::Result<(String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    loop {
        if let Some(fin) = buf.windows(4).position(|ventana| ventana == b"\r\n\r\n") {
            let fin = fin + 4;
            let cabecera = String::from_utf8_lossy(&buf[..fin]).into_owned();
            return Ok((cabecera, buf[fin..].to_vec()));
        }
        if buf.len() > MAX_CABECERA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cabecera de proxy enorme",
            ));
        }
        let mut trozo = [0u8; 1024];
        let leidos = cliente.read(&mut trozo).await?;
        if leidos == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "conexion sin cabecera",
            ));
        }
        buf.extend_from_slice(&trozo[..leidos]);
    }
}

/// `CONNECT host:port HTTP/1.1` (and `[v6]:port`).
fn destino_connect(linea: &str) -> Option<(String, u16)> {
    let resto = linea.strip_prefix("CONNECT ")?;
    let autoridad = resto.split_whitespace().next()?;
    separar_autoridad(autoridad)
}

fn separar_autoridad(autoridad: &str) -> Option<(String, u16)> {
    if let Some(resto) = autoridad.strip_prefix('[') {
        let (host, cola) = resto.split_once("]:")?;
        return Some((host.to_string(), cola.parse().ok()?));
    }
    let (host, puerto) = autoridad.rsplit_once(':')?;
    Some((host.to_string(), puerto.parse().ok()?))
}

fn normalizar_proxy(proxy: &str) -> io::Result<String> {
    let sin_esquema = proxy
        .strip_prefix("socks5h://")
        .or_else(|| proxy.strip_prefix("socks5://"))
        .unwrap_or(proxy);
    let addr = if sin_esquema.contains(':') {
        sin_esquema.to_string()
    } else {
        format!("{sin_esquema}:40000")
    };
    addr.parse::<SocketAddr>()
        .map(|_| addr)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "proxy SOCKS5 invalido"))
}

/// SOCKS5 (no auth): the name travels to the proxy — `socks5h` — because in
/// proxy mode the machine's own resolver is not the tunnel's.
async fn conectar_socks(proxy: &str, host: &str, puerto: u16) -> io::Result<TcpStream> {
    let mut flujo = TcpStream::connect(normalizar_proxy(proxy)?).await?;
    flujo.set_nodelay(true).ok();
    flujo.write_all(&[5, 1, 0]).await?;
    let mut saludo = [0u8; 2];
    flujo.read_exact(&mut saludo).await?;
    if saludo != [5, 0] {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "el proxy SOCKS5 pide autenticacion",
        ));
    }

    let destino = host.trim_matches(|c| c == '[' || c == ']');
    let mut pedido = vec![5u8, 1, 0];
    if let Ok(v4) = destino.parse::<Ipv4Addr>() {
        pedido.push(1);
        pedido.extend_from_slice(&v4.octets());
    } else if let Ok(v6) = destino.parse::<Ipv6Addr>() {
        pedido.push(4);
        pedido.extend_from_slice(&v6.octets());
    } else {
        let bytes = destino.as_bytes();
        if bytes.is_empty() || bytes.len() > 255 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "destino invalido para SOCKS5",
            ));
        }
        pedido.push(3);
        pedido.push(bytes.len() as u8);
        pedido.extend_from_slice(bytes);
    }
    pedido.extend_from_slice(&puerto.to_be_bytes());
    flujo.write_all(&pedido).await?;

    let mut respuesta = [0u8; 4];
    flujo.read_exact(&mut respuesta).await?;
    if respuesta[0] != 5 || respuesta[1] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("el proxy SOCKS5 rechazo el destino ({})", respuesta[1]),
        ));
    }
    let largo = match respuesta[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut uno = [0u8; 1];
            flujo.read_exact(&mut uno).await?;
            uno[0] as usize
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "respuesta SOCKS5 invalida",
            ))
        }
    };
    let mut cola = vec![0u8; largo + 2];
    flujo.read_exact(&mut cola).await?;
    Ok(flujo)
}

fn args(valores: &[&str]) -> Vec<String> {
    valores.iter().map(|valor| valor.to_string()).collect()
}

#[cfg(test)]
mod tests;
