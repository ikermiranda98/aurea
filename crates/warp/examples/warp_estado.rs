//! Read-only view of the WARP ring on this machine.
//!
//! `cargo run -p zeron-warp --example warp_estado`
//!
//! It reads the shared files, asks the service and `warp-cli` for the tunnel's
//! state and reports the pin. Nothing is written, pinned or rotated: a live
//! rotation belongs to the desktop button, never to an example.

use std::sync::Arc;

use zeron_warp::{Ghost, Rutas, Sistema};

#[tokio::main]
async fn main() {
    let rutas = Rutas::del_entorno();
    println!("warp-cli:  {}", rutas.warp_cli.display());
    println!("hosts:     {}", rutas.hosts.display());
    println!("memoria:   {}", rutas.memoria.display());
    println!("eventos:   {}", rutas.eventos.display());
    println!("dia:       {}", rutas.dia.display());
    println!("historial: {}", rutas.historial.display());
    println!();
    for (nombre, ruta) in [
        ("warp-cli", &rutas.warp_cli),
        ("hosts", &rutas.hosts),
        ("memoria", &rutas.memoria),
        ("eventos", &rutas.eventos),
        ("dia", &rutas.dia),
        ("historial", &rutas.historial),
        ("pin log", &rutas.pin_log),
        ("cerrojo", &rutas.cerrojo),
        ("epoch", &rutas.epoch),
    ] {
        println!("{nombre:9} exists={}", ruta.exists());
    }
    println!();

    let ghost = Ghost::nuevo(rutas.clone(), Arc::new(Sistema));
    let estado = ghost.estado().await;
    println!("estado:    {}", estado.warp.como_texto());
    println!(
        "rotating={} attempts={} attempt={:?} rotations={} current={:?} pin={}",
        estado.rotating,
        estado.attempts,
        estado.attempt,
        estado.rotations,
        estado.current,
        estado.pin
    );

    if let Ok(texto) = std::fs::read_to_string(&rutas.dia) {
        println!("dia file:  {:?}", texto.trim());
    }
    if let Ok(texto) = std::fs::read_to_string(&rutas.memoria) {
        for linea in texto.lines().filter(|linea| !linea.trim().is_empty()) {
            println!("  /64 {linea}");
        }
    }
    if let Ok(texto) = std::fs::read_to_string(&rutas.eventos) {
        println!("ultimos eventos:");
        for linea in texto.lines().rev().take(5).collect::<Vec<_>>().iter().rev() {
            println!("  {linea}");
        }
    }
}
