use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::{entorno, normalizar_proxy, Puente, PuenteEgress};
use crate::ghost::Egress;
use crate::mundo::Mundo;
use crate::shell::Salida;

/// A SOCKS5 server that records every dial and echoes what it is given.
struct SocksFalso {
    addr: SocketAddr,
    pedidos: Arc<Mutex<Vec<(u8, String, u16)>>>,
    tarea: JoinHandle<()>,
}

impl Drop for SocksFalso {
    fn drop(&mut self) {
        self.tarea.abort();
    }
}

async fn socks_falso() -> SocksFalso {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let pedidos = Arc::new(Mutex::new(Vec::new()));
    let registro = Arc::clone(&pedidos);
    let tarea = tokio::spawn(async move {
        loop {
            let Ok((flujo, _)) = listener.accept().await else {
                return;
            };
            let registro = Arc::clone(&registro);
            tokio::spawn(async move {
                let _ = atender(flujo, registro).await;
            });
        }
    });
    SocksFalso {
        addr,
        pedidos,
        tarea,
    }
}

async fn atender(
    mut flujo: TcpStream,
    pedidos: Arc<Mutex<Vec<(u8, String, u16)>>>,
) -> std::io::Result<()> {
    let mut saludo = [0u8; 2];
    flujo.read_exact(&mut saludo).await?;
    let mut metodos = vec![0u8; saludo[1] as usize];
    flujo.read_exact(&mut metodos).await?;
    flujo.write_all(&[5, 0]).await?;

    let mut cabecera = [0u8; 4];
    flujo.read_exact(&mut cabecera).await?;
    let atyp = cabecera[3];
    let host = match atyp {
        1 => {
            let mut octetos = [0u8; 4];
            flujo.read_exact(&mut octetos).await?;
            std::net::Ipv4Addr::from(octetos).to_string()
        }
        4 => {
            let mut octetos = [0u8; 16];
            flujo.read_exact(&mut octetos).await?;
            std::net::Ipv6Addr::from(octetos).to_string()
        }
        3 => {
            let mut largo = [0u8; 1];
            flujo.read_exact(&mut largo).await?;
            let mut nombre = vec![0u8; largo[0] as usize];
            flujo.read_exact(&mut nombre).await?;
            String::from_utf8_lossy(&nombre).into_owned()
        }
        _ => return Ok(()),
    };
    let mut puerto = [0u8; 2];
    flujo.read_exact(&mut puerto).await?;
    if let Ok(mut lista) = pedidos.lock() {
        lista.push((atyp, host, u16::from_be_bytes(puerto)));
    }
    flujo.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0]).await?;

    let mut buf = [0u8; 1024];
    loop {
        let leidos = flujo.read(&mut buf).await?;
        if leidos == 0 {
            return Ok(());
        }
        flujo.write_all(&buf[..leidos]).await?;
    }
}

async fn tunel(puente: &Puente, destino: &str) -> TcpStream {
    tunel_en(puente.addr(), destino).await
}

async fn tunel_en(puente: SocketAddr, destino: &str) -> TcpStream {
    let mut cliente = TcpStream::connect(puente).await.unwrap();
    cliente
        .write_all(format!("CONNECT {destino} HTTP/1.1\r\nHost: puente\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = [0u8; 512];
    let leidos = cliente.read(&mut buf).await.unwrap();
    let respuesta = String::from_utf8_lossy(&buf[..leidos]).into_owned();
    assert!(
        respuesta.starts_with("HTTP/1.1 200 Connection Established"),
        "respuesta inesperada: {respuesta}"
    );
    cliente
}

#[tokio::test]
async fn un_connect_cruza_el_puente_y_el_nombre_lo_resuelve_el_proxy() {
    let socks = socks_falso().await;
    let puente = Puente::abrir(format!("socks5h://{}", socks.addr))
        .await
        .unwrap();

    let mut cliente = tunel(&puente, "api.example.com:443").await;
    cliente.write_all(b"ping").await.unwrap();
    let mut eco = [0u8; 4];
    cliente.read_exact(&mut eco).await.unwrap();
    assert_eq!(&eco, b"ping");

    // The name travelled as ATYP 3: the bridge never touched a resolver.
    assert_eq!(
        socks.pedidos.lock().unwrap().as_slice(),
        [(3u8, "api.example.com".to_string(), 443u16)]
    );
    assert_eq!(puente.destinos(), ["api.example.com:443"]);
}

#[tokio::test]
async fn una_direccion_literal_viaja_como_tal() {
    let socks = socks_falso().await;
    let puente = Puente::abrir(socks.addr.to_string()).await.unwrap();

    let mut v4 = tunel(&puente, "1.2.3.4:443").await;
    v4.write_all(b"a").await.unwrap();
    let mut uno = [0u8; 1];
    v4.read_exact(&mut uno).await.unwrap();

    let mut v6 = tunel(&puente, "[2606:4700:78::90:0:141]:443").await;
    v6.write_all(b"b").await.unwrap();
    v6.read_exact(&mut uno).await.unwrap();

    assert_eq!(
        socks.pedidos.lock().unwrap().as_slice(),
        [
            (1u8, "1.2.3.4".to_string(), 443u16),
            (4u8, "2606:4700:78::90:0:141".to_string(), 443u16),
        ]
    );
}

#[tokio::test]
async fn lo_que_no_es_un_connect_es_un_400() {
    let socks = socks_falso().await;
    let puente = Puente::abrir(socks.addr.to_string()).await.unwrap();

    let mut cliente = TcpStream::connect(puente.addr()).await.unwrap();
    cliente
        .write_all(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n")
        .await
        .unwrap();
    let mut buf = [0u8; 128];
    let leidos = cliente.read(&mut buf).await.unwrap();
    let respuesta = String::from_utf8_lossy(&buf[..leidos]).into_owned();
    assert!(respuesta.starts_with("HTTP/1.1 400"), "{respuesta}");
}

#[tokio::test]
async fn cortar_cierra_los_tuneles_vivos_y_deja_el_puente_escuchando() {
    let socks = socks_falso().await;
    let puente = Puente::abrir(socks.addr.to_string()).await.unwrap();

    let mut cliente = tunel(&puente, "api.example.com:443").await;
    cliente.write_all(b"x").await.unwrap();
    let mut uno = [0u8; 1];
    cliente.read_exact(&mut uno).await.unwrap();

    puente.cortar();
    let fin = tokio::time::timeout(Duration::from_secs(2), cliente.read(&mut uno)).await;
    assert!(
        matches!(fin, Ok(Ok(0)) | Ok(Err(_))),
        "el tunel sobrevivio al corte: {fin:?}"
    );

    // The listener is still there: the next request dials again.
    let mut otra = tunel(&puente, "api.example.com:443").await;
    otra.write_all(b"y").await.unwrap();
    otra.read_exact(&mut uno).await.unwrap();
}

#[test]
fn el_entorno_apunta_al_puente_y_deja_fuera_loopback() {
    let vars = entorno(40123);
    let buscar = |clave: &str| {
        vars.iter()
            .find(|(nombre, _)| nombre == clave)
            .map(|(_, valor)| valor.as_str())
    };
    assert_eq!(buscar("HTTPS_PROXY"), Some("http://127.0.0.1:40123"));
    assert_eq!(buscar("https_proxy"), Some("http://127.0.0.1:40123"));
    assert_eq!(buscar("HTTP_PROXY"), Some("http://127.0.0.1:40123"));
    assert_eq!(buscar("ALL_PROXY"), Some("http://127.0.0.1:40123"));
    let sin_proxy = buscar("NO_PROXY").unwrap();
    assert!(sin_proxy.contains("127.0.0.1"));
    assert!(sin_proxy.contains("localhost"));
    assert_eq!(buscar("no_proxy"), Some(sin_proxy));
}

#[test]
fn el_proxy_se_normaliza_y_se_valida() {
    assert_eq!(
        normalizar_proxy("socks5h://127.0.0.1:40000").unwrap(),
        "127.0.0.1:40000"
    );
    assert_eq!(
        normalizar_proxy("socks5://127.0.0.1:40000").unwrap(),
        "127.0.0.1:40000"
    );
    assert_eq!(
        normalizar_proxy("127.0.0.1:40000").unwrap(),
        "127.0.0.1:40000"
    );
    assert_eq!(normalizar_proxy("127.0.0.1").unwrap(), "127.0.0.1:40000");
    assert!(normalizar_proxy("no es una direccion").is_err());
}

struct MundoGuion {
    respuestas: Mutex<VecDeque<String>>,
    llamadas: Mutex<Vec<(String, Vec<String>)>>,
}

impl MundoGuion {
    fn con(respuestas: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            respuestas: Mutex::new(respuestas.iter().map(|r| r.to_string()).collect()),
            llamadas: Mutex::new(Vec::new()),
        })
    }

    fn llamadas(&self) -> Vec<(String, Vec<String>)> {
        self.llamadas.lock().unwrap().clone()
    }
}

#[async_trait]
impl Mundo for MundoGuion {
    async fn ejecutar(&self, programa: &str, args: &[String], _espera: Option<Duration>) -> Salida {
        self.llamadas
            .lock()
            .unwrap()
            .push((programa.to_string(), args.to_vec()));
        let stdout = self
            .respuestas
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
        Salida { status: 0, stdout }
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

fn traza(ip: &str, warp: &str) -> String {
    format!("ip={ip}\nwarp={warp}\n")
}

async fn egress(mundo: Arc<MundoGuion>) -> PuenteEgress {
    let puente = Puente::abrir("socks5h://127.0.0.1:40000").await.unwrap();
    PuenteEgress::nuevo(puente, mundo as Arc<dyn Mundo>)
}

#[tokio::test]
async fn apuntar_exige_warp_on_y_una_salida_ipv6() {
    let bueno = egress(MundoGuion::con(&[&traza(
        "2a09:bac1:7681:1318::286:84",
        "on",
    )]))
    .await;
    assert_eq!(
        bueno.apuntar(&[]).await.as_deref(),
        Some("2a09:bac1:7681:1318::286:84")
    );

    let apagado = egress(MundoGuion::con(&[&traza(
        "2a09:bac1:7681:1318::286:84",
        "off",
    )]))
    .await;
    assert!(apagado.apuntar(&[]).await.is_none());

    let ipv4 = egress(MundoGuion::con(&[&traza("203.0.113.7", "on")])).await;
    assert!(ipv4.apuntar(&[]).await.is_none());
}

#[tokio::test]
async fn la_sonda_va_por_el_puente_y_reintenta_sin_ip() {
    let mundo = MundoGuion::con(&[
        "curl: (7) sin ruta\n",
        &traza("2a09:bac1:7681:1318::286:84", "on"),
    ]);
    let egress = egress(Arc::clone(&mundo)).await;

    let salida = egress.sonda().await.expect("la segunda toma responde");
    assert_eq!(salida.ip, "2a09:bac1:7681:1318::286:84");
    assert_eq!(salida.warp.as_deref(), Some("on"));

    let llamadas = mundo.llamadas();
    assert_eq!(llamadas.len(), 2);
    let (programa, args) = &llamadas[0];
    assert_eq!(programa, "curl");
    let texto = args.join(" ");
    assert!(
        texto.contains(&format!("--proxy http://{}", egress.addr())),
        "{texto}"
    );
    assert!(
        !args.iter().any(|arg| arg == "-6"),
        "la sonda no debe forzar IPv6 local: {texto}"
    );
    assert!(
        texto.contains("https://cloudflare.com/cdn-cgi/trace"),
        "{texto}"
    );
}

#[tokio::test]
async fn apuntar_suelta_el_pool_antes_de_sondear() {
    let socks = socks_falso().await;
    let mundo = MundoGuion::con(&[&traza("2a09:bac1:7681:1318::286:84", "on")]);
    let puente = Puente::abrir(socks.addr.to_string()).await.unwrap();
    let egress = PuenteEgress::nuevo(puente, mundo as Arc<dyn Mundo>);

    let mut viejo = tunel_en(egress.addr(), "api.example.com:443").await;
    viejo.write_all(b"x").await.unwrap();
    let mut uno = [0u8; 1];
    viejo.read_exact(&mut uno).await.unwrap();

    assert!(egress.apuntar(&[]).await.is_some());
    let fin = tokio::time::timeout(Duration::from_secs(2), viejo.read(&mut uno)).await;
    assert!(
        matches!(fin, Ok(Ok(0)) | Ok(Err(_))),
        "apuntar dejo vivo el tunel viejo: {fin:?}"
    );
}

#[tokio::test]
async fn soltar_corta_los_tuneles() {
    let socks = socks_falso().await;
    let mundo = MundoGuion::con(&[]);
    let puente = Puente::abrir(socks.addr.to_string()).await.unwrap();
    let egress = PuenteEgress::nuevo(puente, mundo as Arc<dyn Mundo>);

    let mut cliente = tunel_en(egress.addr(), "api.example.com:443").await;
    cliente.write_all(b"x").await.unwrap();
    let mut uno = [0u8; 1];
    cliente.read_exact(&mut uno).await.unwrap();

    egress.soltar().await;
    let fin = tokio::time::timeout(Duration::from_secs(2), cliente.read(&mut uno)).await;
    assert!(
        matches!(fin, Ok(Ok(0)) | Ok(Err(_))),
        "soltar dejo vivo el tunel: {fin:?}"
    );
}
