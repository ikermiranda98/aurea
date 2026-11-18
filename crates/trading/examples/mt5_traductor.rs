//! Drives the A3 translator against the running MetaTrader 5, end to end.
//!
//! ```text
//! $env:MT5_LOGIN="<login>"; $env:MT5_SERVER="<server>"
//! cargo run -p zeron-trading --example mt5_traductor
//! ```
//!
//! What the fake-bridge e2e (`crates/trading/tests/mt5_e2e.rs`) proves with a
//! scripted broker, this proves with the real terminal: `qualify` resolving a
//! futures root to the broker's CFD, the bot's units turned into lots, a market
//! entry, a stop turned into the position's SL, an OCA pair (limit + stop),
//! cancellations, the opposite market order closing it, bars, and the account
//! summary. Demo accounts only, minimum exposure, and whatever it opened is
//! closed on every exit path.
//!
//! The engine ticks the translator every second; protection is applied on that
//! tick (throttled), so this example runs the same pump.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zeron_trading::mt5::enlace::{ConfiguracionMt5, EnlaceMt5, Enlaces, SinAlmacen};
use zeron_trading::mt5::{
    EstadoInicial, PeticionContrato, PeticionOrden, PosicionMt5, Traductor,
};

const SISTEMA: &str = "7";
const MAGIC: i64 = 7_700_007;

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

fn orden(con_id: i64, accion: &str, tipo: &str, cantidad: f64, precio: f64) -> PeticionOrden {
    PeticionOrden {
        order_id: None,
        client_id: Some(1),
        con_id,
        action: accion.to_string(),
        order_type: tipo.to_string(),
        total_quantity: cantidad,
        aux_price: (tipo == "STP").then_some(precio),
        lmt_price: (tipo == "LMT").then_some(precio),
        tif: Some("DAY".to_string()),
        oca_group: None,
    }
}

async fn esperar<T>(
    mut que: impl FnMut() -> Option<T>,
    ms: u64,
    que_esperaba: &str,
) -> Option<T> {
    let limite = Instant::now() + Duration::from_millis(ms);
    loop {
        if let Some(valor) = que() {
            return Some(valor);
        }
        if Instant::now() >= limite {
            println!("  (no llegó: {que_esperaba})");
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn posicion_con(enlace: &EnlaceMt5, ticket: i64, que: impl Fn(&PosicionMt5) -> bool) -> Option<PosicionMt5> {
    enlace
        .libro()
        .and_then(|libro| libro.positions.into_iter().find(|p| p.ticket == ticket && que(p)))
}

/// Closes and cancels everything this example opened (magic `MAGIC`).
async fn limpiar(enlace: &EnlaceMt5) {
    if let Some(libro) = enlace.libro() {
        for orden in libro.orders.iter().filter(|o| o.magic == MAGIC) {
            let borrada = enlace.puente().comando("DELETE", &orden.ticket.to_string()).await;
            println!(
                "  limpieza: DELETE {} ok={} {}",
                orden.ticket, borrada.ok, borrada.message
            );
        }
        for posicion in libro.positions.iter().filter(|p| p.magic == MAGIC) {
            let cierre = enlace
                .puente()
                .comando("CLOSEVOL", &posicion.ticket.to_string())
                .await;
            println!(
                "  limpieza: CLOSEVOL {} ok={} {}",
                posicion.ticket, cierre.ok, cierre.message
            );
        }
    }
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

    let carpeta = std::env::temp_dir().join("aurea-mt5-traductor");
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

    let config = ConfiguracionMt5 {
        login: login.parse().unwrap_or(0),
        servidor: servidor.unwrap_or_default(),
        simbolo: None,
        magic: None,
    };
    let id = config.id();
    let enlace = match EnlaceMt5::arrancar(&python, &script, &desfase, config, &[]) {
        Ok(enlace) => enlace,
        Err(error) => {
            println!("cannot start the bridge: {error}");
            return;
        }
    };

    let Some(libro) = esperar(|| enlace.libro(), 20_000, "el libro del puente").await else {
        println!("no book after 20 s; is the terminal open and logged in?");
        println!("bridge: {:?}", enlace.motivo());
        enlace.parar().await;
        return;
    };
    println!(
        "cuenta {} {} (demo={}) balance={:?}",
        libro.login, libro.server, libro.demo, libro.balance
    );
    if !libro.demo {
        println!("refusing to trade: this is not a demo account");
        enlace.parar().await;
        return;
    }

    let mut enlaces = Enlaces::nuevo(Arc::new(SinAlmacen));
    enlaces.añadir(enlace);
    enlaces.asignar(SISTEMA, id.clone());
    let enlace = enlaces.enlace(&id).expect("link in the registry");
    let traductor = Arc::new(Traductor::new(Arc::new(enlaces), EstadoInicial::default()));
    let bombeo = {
        let traductor = Arc::clone(&traductor);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                traductor.tick().await;
            }
        })
    };

    let mut fallos: Vec<String> = Vec::new();

    // 1. The futures root resolves to the broker's own CFD symbol.
    // MetaQuotes-Demo serves indices (USTEC) read-only — trade_mode disabled —
    // so the default is micro gold, which this server does trade. Override
    // with MT5_ROOT (MNQ→US100/USTEC, MGC→XAUUSD, MES→SP500…).
    let root = std::env::var("MT5_ROOT").unwrap_or_else(|_| "MGC".to_string());
    let contrato = match traductor
        .qualify(
            SISTEMA,
            PeticionContrato {
                sec_type: Some("FUT".to_string()),
                symbol: Some(root.clone()),
                ..PeticionContrato::default()
            },
        )
        .await
    {
        Ok(contrato) => contrato,
        Err(error) => {
            println!("qualify {root}: {error}");
            enlace.parar().await;
            return;
        }
    };
    println!(
        "qualify {root} → {} conId={} (lotes por unidad {})",
        contrato.mt5, contrato.con_id, contrato.lots_per_unit
    );
    if contrato.mt5.is_empty() {
        fallos.push(format!("{root} no resolvió a ningún símbolo"));
    }
    let esperado = match root.as_str() {
        "MGC" | "GC" => "XAUUSD",
        _ => "",
    };
    if !esperado.is_empty() && contrato.mt5 != esperado {
        fallos.push(format!("qualify {root} resolvió {} en vez de {esperado}", contrato.mt5));
    }
    let con_id = contrato.con_id;

    // 2. A market buy: one bot unit becomes two lots, booked as filled.
    let compra = traductor
        .place(SISTEMA, orden(con_id, "BUY", "MKT", 1.0, 0.0))
        .await;
    match &compra {
        Ok(orden) => println!(
            "place BUY MKT: role={} status={} filled={} {}",
            orden.role, orden.status, orden.filled, orden.message
        ),
        Err(error) => println!("place BUY MKT: {error}"),
    }
    let Ok(compra) = compra else {
        limpiar(&enlace).await;
        enlace.parar().await;
        return;
    };
    if compra.status != "Filled" {
        fallos.push(format!("la compra quedó en {}", compra.status));
    }
    let posicion = esperar(
        || {
            enlace.libro().and_then(|libro| {
                libro
                    .positions
                    .into_iter()
                    .find(|p| p.magic == MAGIC)
            })
        },
        10_000,
        "la posición de la compra",
    )
    .await;
    let Some(posicion) = posicion else {
        limpiar(&enlace).await;
        enlace.parar().await;
        return;
    };
    println!(
        "  posición {} {} {} lotes @ {} (actual {})",
        posicion.ticket, posicion.side, posicion.volume, posicion.entry, posicion.current
    );
    if (posicion.volume - contrato.lots_per_unit).abs() > 1e-9 {
        fallos.push(format!(
            "1 unidad de {root} abrió {} lotes, no {}",
            posicion.volume, contrato.lots_per_unit
        ));
    }
    // Stops and targets as a fraction of the price: gold, indices and forex
    // live at very different scales.
    let cerca = (posicion.current * 0.002).max(0.5);

    // 3. A stop against it becomes the position's stop loss.
    let stop = traductor
        .place(
            SISTEMA,
            orden(con_id, "SELL", "STP", 1.0, posicion.current - cerca),
        )
        .await;
    match &stop {
        Ok(orden) => println!("place SELL STP: role={} status={}", orden.role, orden.status),
        Err(error) => println!("place SELL STP: {error}"),
    }
    let Ok(stop) = stop else {
        limpiar(&enlace).await;
        enlace.parar().await;
        return;
    };
    if stop.role != "exit_sl" {
        fallos.push(format!("el stop tomó el rol {}", stop.role));
    }
    let protegida = esperar(
        || posicion_con(&enlace, posicion.ticket, |p| p.sl > 0.0),
        10_000,
        "el SL en la posición",
    )
    .await;
    match &protegida {
        Some(p) => println!("  SL en la posición: {} (pedido {})", p.sl, posicion.current - cerca),
        None => fallos.push("el stop no llegó a la posición como SL".to_string()),
    }

    // 4. Cancelling the stop takes the SL off the position.
    match traductor.cancel(SISTEMA, stop.order_id).await {
        Ok(orden) => println!("cancel stop: status={}", orden.status),
        Err(error) => fallos.push(format!("cancel stop: {error}")),
    }
    let libre = esperar(
        || posicion_con(&enlace, posicion.ticket, |p| p.sl == 0.0 && p.tp == 0.0),
        10_000,
        "la posición sin SL ni TP",
    )
    .await;
    if libre.is_none() {
        fallos.push("el SL siguió en la posición tras cancelar".to_string());
    }

    // 5. A limit target and a stop below protect the same position (OCA pair).
    let objetivo = traductor
        .place(
            SISTEMA,
            orden(con_id, "SELL", "LMT", 1.0, posicion.current + cerca * 1.5),
        )
        .await;
    match &objetivo {
        Ok(orden) => println!("place SELL LMT: role={} status={}", orden.role, orden.status),
        Err(error) => println!("place SELL LMT: {error}"),
    }
    let Ok(objetivo) = objetivo else {
        limpiar(&enlace).await;
        enlace.parar().await;
        return;
    };
    let stop2 = traductor
        .place(
            SISTEMA,
            orden(con_id, "SELL", "STP", 1.0, posicion.current - cerca * 1.5),
        )
        .await;
    match &stop2 {
        Ok(orden) => println!("place SELL STP: role={} status={}", orden.role, orden.status),
        Err(error) => println!("place SELL STP: {error}"),
    }
    let Ok(stop2) = stop2 else {
        limpiar(&enlace).await;
        enlace.parar().await;
        return;
    };
    let con_ambos = esperar(
        || posicion_con(&enlace, posicion.ticket, |p| p.sl > 0.0 && p.tp > 0.0),
        10_000,
        "SL y TP a la vez",
    )
    .await;
    match &con_ambos {
        Some(p) => println!("  par OCA en la posición: sl={} tp={}", p.sl, p.tp),
        None => fallos.push("el par LMT+STP no llegó a la posición".to_string()),
    }

    // 6. Cancelling both clears the pair.
    for (nombre, orden) in [("objetivo", &objetivo), ("stop", &stop2)] {
        match traductor.cancel(SISTEMA, orden.order_id).await {
            Ok(orden) => println!("cancel {nombre}: status={}", orden.status),
            Err(error) => fallos.push(format!("cancel {nombre}: {error}")),
        }
    }
    let limpia = esperar(
        || posicion_con(&enlace, posicion.ticket, |p| p.sl == 0.0 && p.tp == 0.0),
        10_000,
        "la posición sin protección",
    )
    .await;
    if limpia.is_none() {
        if let Some(p) = posicion_con(&enlace, posicion.ticket, |_| true) {
            println!("  la posición conserva sl={} tp={}", p.sl, p.tp);
        }
        fallos.push("la protección siguió en la posición".to_string());
    }

    // 7. The opposite market order closes it, and the bot's units go back to 0.
    let venta = traductor
        .place(SISTEMA, orden(con_id, "SELL", "MKT", 1.0, 0.0))
        .await;
    match &venta {
        Ok(orden) => println!(
            "place SELL MKT: status={} filled={} {}",
            orden.status, orden.filled, orden.message
        ),
        Err(error) => println!("place SELL MKT: {error}"),
    }
    if venta.as_ref().map(|orden| orden.status.as_str()) != Ok("Filled") {
        fallos.push("la venta de cierre no quedó Filled".to_string());
    }
    let cerrada = esperar(
        || {
            enlace
                .libro()
                .map(|libro| libro.positions.iter().all(|p| p.magic != MAGIC))
                .filter(|vacia| *vacia)
        },
        10_000,
        "la posición cerrada",
    )
    .await;
    if cerrada.is_none() {
        fallos.push("la posición siguió abierta tras la venta".to_string());
    }
    let ejecuciones = traductor.ejecuciones().await;
    println!("ejecuciones registradas: {}", ejecuciones.len());
    if ejecuciones.len() < 2 {
        fallos.push(format!(
            "sólo {} ejecuciones para una ida y vuelta",
            ejecuciones.len()
        ));
    }

    // 8. Bars and the account summary come from the same link.
    match traductor.bars(SISTEMA, con_id, None, "1 D", "1 min").await {
        Ok(barras) => println!("bars 1 min: {} (última {})", barras.len(), barras.last().map(|b| b.0).unwrap_or(0)),
        Err(error) => fallos.push(format!("bars: {error}")),
    }
    let resumen = traductor.account_summary(SISTEMA).await;
    for etiqueta in &resumen {
        println!("  cuenta: {} = {} {}", etiqueta.tag, etiqueta.value, etiqueta.currency);
    }
    if resumen.is_empty() {
        fallos.push("account_summary vino vacío".to_string());
    }
    println!(
        "positions del bot: {:?}",
        traductor.positions(SISTEMA).await.iter().map(|p| p.position).collect::<Vec<_>>()
    );

    // 9. Nothing of ours is left working.
    limpiar(&enlace).await;
    if let Some(libro) = enlace.libro() {
        let nuestras = libro.positions.iter().filter(|p| p.magic == MAGIC).count()
            + libro.orders.iter().filter(|o| o.magic == MAGIC).count();
        if nuestras > 0 {
            fallos.push(format!("quedan {nuestras} cosas de la prueba abiertas"));
        }
    }

    bombeo.abort();
    enlace.parar().await;
    if fallos.is_empty() {
        println!("OK: el traductor opera la cuenta real (demo) de punta a punta");
    } else {
        println!("FALLOS:");
        for fallo in &fallos {
            println!("  - {fallo}");
        }
        std::process::exit(1);
    }
}
