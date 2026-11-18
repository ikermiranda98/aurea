//! Live check for lane C2: the bridge the sidecar will ride.
//!
//! It does not touch WARP's mode — that stays an explicit operator decision
//! (`warp-cli mode proxy`). With WARP already in proxy mode this prints the
//! trace the app's traffic lands on, the environment to hand `opencode serve`
//! and, with `--segundos`, every tunnel the client opens while it runs.
//!
//! ```text
//! cargo run -p zeron-warp --example warp_egress -- --escucha 127.0.0.1:40123 --segundos 30
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use zeron_warp::egress::{PROXY_WARP, Puente, PuenteEgress};
use zeron_warp::mundo::Sistema;

#[tokio::main]
async fn main() {
    let mut proxy = PROXY_WARP.to_string();
    let mut escucha: SocketAddr = "127.0.0.1:40123".parse().unwrap();
    let mut segundos = 0u64;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--proxy" => proxy = args.next().unwrap_or(proxy),
            "--escucha" => {
                if let Some(valor) = args.next() {
                    escucha = valor.parse().unwrap_or(escucha);
                }
            }
            "--segundos" => {
                segundos = args
                    .next()
                    .and_then(|valor| valor.parse().ok())
                    .unwrap_or(segundos);
            }
            otro => {
                eprintln!("argumento desconocido: {otro}");
                std::process::exit(2);
            }
        }
    }

    let puente = match Puente::abrir_en(escucha, proxy.clone()).await {
        Ok(puente) => puente,
        Err(error) => {
            eprintln!("no pude abrir el puente en {escucha}: {error}");
            std::process::exit(1);
        }
    };
    let addr = puente.addr();
    let egress = PuenteEgress::nuevo(puente, Arc::new(Sistema));

    println!("puente: http://{addr} -> socks5h://{proxy}");
    for (clave, valor) in egress.entorno() {
        println!("  {clave}={valor}");
    }

    match egress.sonda().await {
        Some(traza) => println!(
            "traza por el puente: ip={} warp={}",
            traza.ip,
            traza.warp.unwrap_or_else(|| "?".to_string())
        ),
        None => println!("traza por el puente: sin respuesta (¿WARP en mode proxy?)"),
    }

    if segundos > 0 {
        println!("escuchando {segundos}s; destinos del cliente:");
        for _ in 0..segundos {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let destinos = egress.destinos();
        if destinos.is_empty() {
            println!("  (ninguno)");
        }
        for destino in destinos {
            println!("  {destino}");
        }
    }
}
