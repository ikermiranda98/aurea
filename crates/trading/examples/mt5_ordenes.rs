//! Exercises the bridge's order verbs against the running MetaTrader 5.
//!
//! ```text
//! $env:MT5_LOGIN="<login>"; $env:MT5_SERVER="<server>"
//! cargo run -p zeron-trading --example mt5_ordenes
//! ```
//!
//! Demo accounts only (the bridge refuses orders on a real one, and so does
//! this example before it sends anything). It trades the minimum volume of
//! EURUSD: a market buy, its stop loss and take profit, a pending stop that it
//! cancels, then closes with the desk's `CLOSEVOL ticket;volume` and reopens
//! one to close it with the desk's one-shot close (`mt5-ordenes.ts`, by
//! symbol). Whatever it opened it closes on every exit path, so a failure
//! half-way never leaves the account holding it.
//!
//! What it proves: `DEAL`, `SLTP`, `PENDING`, `DELETE` and `CLOSEVOL` reach the
//! terminal and are acknowledged (retcode in 10008/10009/10010), and so does
//! the separate Python run the desk's close button uses.
//! `mt5_traductor` is the same against the translator itself.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use zeron_trading::mt5::libro::{LibroMt5, PosicionMt5};
use zeron_trading::mt5::puente_cliente::PuenteMt5;

/// Not a bot's magic: nothing else should touch what this opens.
const MAGIC: i64 = 990_001;
const COMENTARIO: &str = "aurea a3 prueba";
const SIMBOLO: &str = "EURUSD";

fn python() -> Option<PathBuf> {
    if let Ok(ruta) = std::env::var("MT5_PYTHON") {
        return Some(PathBuf::from(ruta));
    }
    let local = std::env::var("LOCALAPPDATA").ok()?;
    let por_defecto = PathBuf::from(local)
        .join("Programs")
        .join("Python")
        .join("Python312")
        .join("python.exe");
    if por_defecto.is_file() {
        return Some(por_defecto);
    }
    Some(PathBuf::from("python"))
}

async fn esperar_libro(puente: &PuenteMt5, ms: u64) -> Option<LibroMt5> {
    let limite = Instant::now() + Duration::from_millis(ms);
    loop {
        if let Some(libro) = puente.libro().await {
            return Some(libro);
        }
        if Instant::now() >= limite {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn esperar_posicion(puente: &PuenteMt5, ms: u64) -> Option<PosicionMt5> {
    let limite = Instant::now() + Duration::from_millis(ms);
    loop {
        if let Some(libro) = puente.libro().await {
            if let Some(posicion) = libro
                .positions
                .into_iter()
                .find(|p| p.magic == MAGIC && p.symbol == SIMBOLO)
            {
                return Some(posicion);
            }
        }
        if Instant::now() >= limite {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn esperar_sin_posicion(puente: &PuenteMt5, ms: u64) -> bool {
    let limite = Instant::now() + Duration::from_millis(ms);
    loop {
        let quedan = puente
            .libro()
            .await
            .map(|libro| {
                libro
                    .positions
                    .iter()
                    .filter(|p| p.magic == MAGIC && p.symbol == SIMBOLO)
                    .count()
            })
            .unwrap_or(0);
        if quedan == 0 {
            return true;
        }
        if Instant::now() >= limite {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn esperar_pendiente(puente: &PuenteMt5, ticket: i64, ms: u64) -> bool {
    let limite = Instant::now() + Duration::from_millis(ms);
    loop {
        let vistas: Vec<i64> = puente
            .libro()
            .await
            .map(|libro| libro.orders.into_iter().map(|o| o.ticket).collect())
            .unwrap_or_default();
        if vistas.contains(&ticket) {
            return true;
        }
        if Instant::now() >= limite {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Waits until the book no longer lists `ticket` as pending.
async fn esperar_sin_pendiente(puente: &PuenteMt5, ticket: i64, ms: u64) -> bool {
    let limite = Instant::now() + Duration::from_millis(ms);
    loop {
        let vistas: Vec<i64> = puente
            .libro()
            .await
            .map(|libro| libro.orders.into_iter().map(|o| o.ticket).collect())
            .unwrap_or_default();
        if !vistas.contains(&ticket) {
            return true;
        }
        if Instant::now() >= limite {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn num(datos: &serde_json::Value, campo: &str) -> f64 {
    datos
        .get(campo)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0)
}

#[tokio::main]
async fn main() {
    let Ok(login) = std::env::var("MT5_LOGIN") else {
        println!("set MT5_LOGIN (and MT5_SERVER) to the account the terminal is logged into");
        return;
    };
    let servidor = std::env::var("MT5_SERVER").ok();
    let Some(python) = python() else {
        println!("no Python found: set MT5_PYTHON to the interpreter with MetaTrader5");
        return;
    };

    let carpeta = std::env::temp_dir().join("aurea-mt5-ordenes");
    if let Err(error) = std::fs::create_dir_all(&carpeta) {
        println!("cannot create {}: {error}", carpeta.display());
        return;
    }
    let script = carpeta.join("puente.py");
    let desfase = carpeta.join("desfase.json");
    if let Err(error) = std::fs::write(&script, include_str!("../src/mt5/puente.py")) {
        println!("cannot write the bridge: {error}");
        return;
    }

    let puente = match PuenteMt5::arrancar(
        &python,
        &script,
        &login,
        servidor.as_deref(),
        &desfase,
        &[],
    ) {
        Ok(puente) => puente,
        Err(error) => {
            println!("cannot start the bridge: {error}");
            return;
        }
    };

    let Some(libro) = esperar_libro(&puente, 20_000).await else {
        println!("no book after 20 s; is the terminal open and logged in?");
        puente.parar().await;
        return;
    };
    println!(
        "cuenta {} {} (demo={}) balance={:?} equity={:?}",
        libro.login, libro.server, libro.demo, libro.balance, libro.equity
    );
    if !libro.demo {
        println!("refusing to trade: this is not a demo account");
        puente.parar().await;
        return;
    }

    let spec = puente.comando("SYMBOL", SIMBOLO).await;
    if !spec.ok {
        println!("SYMBOL {SIMBOLO}: {} {}", spec.retcode, spec.message);
        puente.parar().await;
        return;
    }
    let datos = spec.data.clone().unwrap_or_default();
    let volumen = datos
        .get("volume_min")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.01);
    let digitos = datos
        .get("digits")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(5);
    let punto = 10f64.powi(-(digitos as i32));
    println!(
        "{SIMBOLO}: volumen mínimo {volumen}, {digitos} dígitos, bid {} ask {}",
        num(&datos, "bid"),
        num(&datos, "ask")
    );

    let mut fallos = Vec::new();

    // 1. Market buy at the minimum volume.
    let compra = puente
        .comando(
            "DEAL",
            &format!("{SIMBOLO};buy;{volumen};{MAGIC};{COMENTARIO}"),
        )
        .await;
    println!(
        "DEAL compra: ok={} retcode={} {}",
        compra.ok, compra.retcode, compra.message
    );
    if !compra.ok {
        fallos.push(format!("DEAL: {} {}", compra.retcode, compra.message));
    }

    let Some(posicion) = esperar_posicion(&puente, 10_000).await else {
        println!("la posición no llegó al libro; no hay nada que cerrar");
        puente.parar().await;
        return;
    };
    println!(
        "posición {} {} {} @ {} (actual {})",
        posicion.ticket, posicion.side, posicion.volume, posicion.entry, posicion.current
    );

    // 2. Its stop loss and take profit, 30 pips either side of the price.
    let sl = posicion.current - 300.0 * punto;
    let tp = posicion.current + 300.0 * punto;
    let slltp = puente
        .comando("SLTP", &format!("{};{sl};{tp}", posicion.ticket))
        .await;
    println!(
        "SLTP {sl}/{tp}: ok={} retcode={} {}",
        slltp.ok, slltp.retcode, slltp.message
    );
    if !slltp.ok {
        fallos.push(format!("SLTP: {} {}", slltp.retcode, slltp.message));
    } else {
        let mut puesto = false;
        let limite = Instant::now() + Duration::from_secs(5);
        while Instant::now() < limite {
            if let Some(libro) = puente.libro().await {
                if let Some(p) = libro.positions.iter().find(|p| p.ticket == posicion.ticket) {
                    if p.sl > 0.0 && p.tp > 0.0 {
                        println!("  libro: sl={} tp={}", p.sl, p.tp);
                        puesto = true;
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if !puesto {
            fallos.push("SLTP aplicado pero el libro no lo refleja".to_string());
        }
    }

    // 3. A pending sell stop far from the price, and its cancellation.
    let precio_pendiente = posicion.current - 1_000.0 * punto;
    let pendiente = puente
        .comando(
            "PENDING",
            &format!("{SIMBOLO};sell;stop;{volumen};{precio_pendiente};{MAGIC};{COMENTARIO};0"),
        )
        .await;
    let ticket_pendiente = pendiente
        .data
        .as_ref()
        .map(|datos| num(datos, "order") as i64)
        .unwrap_or(0);
    println!(
        "PENDING venta stop {precio_pendiente}: ok={} retcode={} ticket={ticket_pendiente} {}",
        pendiente.ok, pendiente.retcode, pendiente.message
    );
    if !pendiente.ok || ticket_pendiente == 0 {
        fallos.push(format!("PENDING: {} {}", pendiente.retcode, pendiente.message));
    } else {
        if esperar_pendiente(&puente, ticket_pendiente, 5_000).await {
            println!("  el libro la lista");
        } else {
            fallos.push("la pendiente no llegó al libro".to_string());
        }
        let borrada = puente.comando("DELETE", &ticket_pendiente.to_string()).await;
        println!(
            "DELETE {ticket_pendiente}: ok={} retcode={} {}",
            borrada.ok, borrada.retcode, borrada.message
        );
        if !borrada.ok {
            fallos.push(format!("DELETE: {} {}", borrada.retcode, borrada.message));
        } else if !esperar_sin_pendiente(&puente, ticket_pendiente, 5_000).await {
            fallos.push("la pendiente cancelada sigue en el libro".to_string());
        }
    }

    // 4. The desk's manual close (`CLOSEVOL ticket;volume` on the bridge) takes
    //    the position off, volume and all.
    let cierre = puente
        .comando(
            "CLOSEVOL",
            &format!("{};{}", posicion.ticket, posicion.volume),
        )
        .await;
    println!(
        "CLOSEVOL {};{}: ok={} retcode={} {}",
        posicion.ticket, posicion.volume, cierre.ok, cierre.retcode, cierre.message
    );
    if !cierre.ok {
        fallos.push(format!("CLOSEVOL: {} {}", cierre.retcode, cierre.message));
    } else if !esperar_sin_posicion(&puente, 10_000).await {
        fallos.push("la posición sigue abierta tras CLOSEVOL".to_string());
    }

    // 5. The desk's one-shot close (`mt5-ordenes.ts`): it refuses to touch an
    //    account with positions it did not open, and closes the symbol at
    //    market. Reopen one first.
    let otra = puente
        .comando(
            "DEAL",
            &format!("{SIMBOLO};buy;{volumen};{MAGIC};{COMENTARIO}"),
        )
        .await;
    if !otra.ok {
        fallos.push(format!("DEAL (para el cierre manual): {}", otra.message));
    } else if esperar_posicion(&puente, 10_000).await.is_none() {
        fallos.push("la posición para el cierre manual no llegó al libro".to_string());
    } else {
        let ajenas = puente
            .libro()
            .await
            .map(|libro| {
                libro
                    .positions
                    .iter()
                    .filter(|p| p.symbol == SIMBOLO && p.magic != MAGIC)
                    .count()
            })
            .unwrap_or(0);
        if ajenas > 0 {
            println!("  no se prueba el cierre por símbolo: hay {ajenas} posiciones ajenas en {SIMBOLO}");
        } else {
            let pedido = zeron_trading::mt5::PeticionCierre::cerrar(
                libro.login,
                libro.server.clone(),
                SIMBOLO,
            );
            let manual = zeron_trading::mt5::cerrar_manual(&python, &pedido).await;
            println!("cierre manual {SIMBOLO}: ok={} {}", manual.ok, manual.message);
            if !manual.ok {
                fallos.push(format!("cierre manual: {}", manual.message));
            } else if !esperar_sin_posicion(&puente, 10_000).await {
                fallos.push("la posición sigue abierta tras el cierre manual".to_string());
            }
        }
    }

    if let Some(libro) = puente.libro().await {
        let propias = libro
            .positions
            .iter()
            .filter(|p| p.magic == MAGIC)
            .count();
        println!(
            "libro final: {} posiciones ({} de la prueba), {} pendientes",
            libro.positions.len(),
            propias,
            libro.orders.len()
        );
        if propias > 0 {
            // A failure half-way must not leave the account holding anything.
            for posicion in libro.positions.iter().filter(|p| p.magic == MAGIC) {
                let cierre = puente
                    .comando("CLOSEVOL", &posicion.ticket.to_string())
                    .await;
                println!(
                    "  limpieza: CLOSEVOL {} ok={} {}",
                    posicion.ticket, cierre.ok, cierre.message
                );
            }
            for orden in libro.orders.iter().filter(|o| o.magic == MAGIC) {
                let borrada = puente.comando("DELETE", &orden.ticket.to_string()).await;
                println!(
                    "  limpieza: DELETE {} ok={} {}",
                    orden.ticket, borrada.ok, borrada.message
                );
            }
            fallos.push(format!("quedan {propias} posiciones de la prueba"));
        }
    }

    puente.parar().await;
    if fallos.is_empty() {
        println!("OK: DEAL, SLTP, PENDING, DELETE, CLOSEVOL y el cierre manual aceptados por el terminal");
    } else {
        println!("FALLOS:");
        for fallo in &fallos {
            println!("  - {fallo}");
        }
        std::process::exit(1);
    }
}
