//! End-to-end A3: the Rust translator driving the Python bridge against the
//! fake `MetaTrader5` package (the same script the TS oracle's
//! `99_prueba_puente` uses, without the bots).
//!
//! Gated like the oracle: set `AUREA_MT5_E2E=1` and have a Python with the
//! bridge's imports (the fake package supplies MetaTrader5 through
//! `PYTHONPATH`). Without the variable the test reports a skip and passes.
//!
//! ```text
//! $env:AUREA_MT5_E2E="1"; cargo test -p zeron-trading --test mt5_e2e -- --nocapture
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zeron_trading::mt5::enlace::{ConfiguracionMt5, EnlaceMt5, Enlaces, SinAlmacen};
use zeron_trading::mt5::{
    EstadoInicial, PeticionContrato, PeticionOrden, PosicionMt5, Traductor,
};

const LOGIN: i64 = 5_550_001;
const SERVIDOR: &str = "OpenBot-Sim";
const SISTEMA: &str = "7";

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

async fn esperar<T>(que: impl Fn() -> Option<T>, ms: u64) -> Option<T> {
    let limite = Instant::now() + Duration::from_millis(ms);
    loop {
        if let Some(valor) = que() {
            return Some(valor);
        }
        if Instant::now() >= limite {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn posiciones(enlace: &EnlaceMt5, cuantas: usize) -> Vec<PosicionMt5> {
    esperar(
        || {
            enlace
                .libro()
                .map(|libro| libro.positions)
                .filter(|posiciones| posiciones.len() == cuantas)
        },
        10_000,
    )
    .await
    .unwrap_or_else(|| panic!("no llegaron {cuantas} posiciones al libro"))
}

async fn esperar_campo(
    enlace: &EnlaceMt5,
    ticket: i64,
    que: impl Fn(&PosicionMt5) -> bool,
) -> PosicionMt5 {
    let limite = Instant::now() + Duration::from_secs(10);
    loop {
        let vistas = enlace.libro().map(|libro| libro.positions).unwrap_or_default();
        if let Some(posicion) = vistas
            .iter()
            .find(|p| p.ticket == ticket && que(p))
            .cloned()
        {
            return posicion;
        }
        if Instant::now() >= limite {
            panic!("la posición {ticket} no llegó al libro con lo pedido: {vistas:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn orden(con_id: i64, accion: &str, tipo: &str, qty: f64, precio: f64) -> PeticionOrden {
    PeticionOrden {
        order_id: None,
        client_id: Some(1),
        con_id,
        action: accion.to_string(),
        order_type: tipo.to_string(),
        total_quantity: qty,
        aux_price: (tipo == "STP").then_some(precio),
        lmt_price: (tipo == "LMT").then_some(precio),
        tif: Some("DAY".to_string()),
        oca_group: None,
    }
}

#[tokio::test]
async fn the_translator_trades_on_the_bridge_and_the_fake_broker() {
    if std::env::var("AUREA_MT5_E2E").ok().as_deref() != Some("1") {
        eprintln!("A3 e2e skipped: set AUREA_MT5_E2E=1 to run it");
        return;
    }
    let Some(python) = python() else {
        eprintln!("A3 e2e skipped: no Python found (set MT5_PYTHON)");
        return;
    };

    let carpeta = std::env::temp_dir().join(format!("aurea-mt5-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&carpeta);
    std::fs::create_dir_all(carpeta.join("MetaTrader5")).expect("temp dir");
    let script = carpeta.join("puente.py");
    std::fs::write(&script, include_str!("../src/mt5/puente.py")).expect("bridge script");
    std::fs::write(
        carpeta.join("MetaTrader5").join("__init__.py"),
        include_str!("fixtures/mt5_falso/MetaTrader5/__init__.py"),
    )
    .expect("fake MetaTrader5");
    let desfase = carpeta.join("desfase.json");
    let pythonpath = match std::env::var("PYTHONPATH") {
        Ok(anterior) if !anterior.is_empty() => {
            let separador = if cfg!(windows) { ";" } else { ":" };
            format!("{}{separador}{anterior}", carpeta.display())
        }
        _ => carpeta.display().to_string(),
    };

    let config = ConfiguracionMt5 {
        login: LOGIN,
        servidor: SERVIDOR.to_string(),
        simbolo: None,
        magic: None,
    };
    let id = config.id();
    let enlace = EnlaceMt5::arrancar(
        &python,
        &script,
        &desfase,
        config,
        &[
            ("PYTHONPATH".to_string(), pythonpath),
            ("MT5_FALSO_LOGIN".to_string(), LOGIN.to_string()),
            ("MT5_FALSO_SERVIDOR".to_string(), SERVIDOR.to_string()),
        ],
    )
    .expect("bridge starts");

    assert!(
        esperar(|| enlace.libro(), 20_000).await.is_some(),
        "el puente no publicó su libro: {:?}",
        enlace.motivo()
    );
    assert_eq!(enlace.libro().unwrap().login, LOGIN);

    let mut enlaces = Enlaces::nuevo(Arc::new(SinAlmacen));
    enlaces.añadir(enlace);
    enlaces.asignar(SISTEMA, id.clone());
    let enlace = enlaces.enlace(&id).expect("link in the registry");
    let traductor = Arc::new(Traductor::new(Arc::new(enlaces), EstadoInicial::default()));
    // The engine ticks the translator every second; protection updates after a
    // throttle are applied there, not on `place`.
    let bombeo = {
        let traductor = Arc::clone(&traductor);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                traductor.tick().await;
            }
        })
    };

    // 1. Resolve MNQ to the broker's NASDAQ CFD and keep the money per point.
    let contrato = traductor
        .qualify(
            SISTEMA,
            PeticionContrato {
                sec_type: Some("FUT".to_string()),
                symbol: Some("MNQ".to_string()),
                ..PeticionContrato::default()
            },
        )
        .await
        .expect("qualify MNQ");
    assert_eq!(contrato.mt5, "US100");
    assert_eq!(contrato.lots_per_unit, 2.0, "1 MNQ = 2 lots of 1 $/point");
    let con_id = contrato.con_id;

    // 2. A market buy opens 2 lots and books one bot unit.
    let compra = traductor
        .place(SISTEMA, orden(con_id, "BUY", "MKT", 1.0, 0.0))
        .await
        .expect("market buy");
    assert_eq!(compra.role, "market");
    assert_eq!(compra.status, "Filled");
    assert_eq!(compra.filled, 1.0);
    let posicion = posiciones(&enlace, 1).await.remove(0);
    assert_eq!(posicion.volume, 2.0);
    assert_eq!(posicion.magic, 7_700_007);
    assert!(posicion.entry > 0.0);
    assert_eq!(traductor.positions(SISTEMA).await[0].position, 1.0);

    // 3. A sell stop against it becomes the position's stop loss.
    let stop = traductor
        .place(
            SISTEMA,
            orden(con_id, "SELL", "STP", 1.0, posicion.current - 50.0),
        )
        .await
        .expect("stop");
    assert_eq!(stop.role, "exit_sl");
    let protegida = esperar_campo(&enlace, posicion.ticket, |p| p.sl > 0.0).await;
    assert!((protegida.sl - (posicion.current - 50.0)).abs() < 0.02);

    // 4. Cancelling it takes the stop off the position.
    let cancelada = traductor.cancel(SISTEMA, stop.order_id).await.expect("cancel");
    assert_eq!(cancelada.status, "Cancelled");
    let libre = esperar_campo(&enlace, posicion.ticket, |p| p.sl == 0.0).await;
    assert_eq!(libre.tp, 0.0);

    // 5. A market sell closes the long position (oldest first).
    let venta = traductor
        .place(SISTEMA, orden(con_id, "SELL", "MKT", 1.0, 0.0))
        .await
        .expect("market sell");
    assert_eq!(venta.status, "Filled");
    assert_eq!(venta.filled, 1.0);
    posiciones(&enlace, 0).await;
    let ejecuciones = traductor.ejecuciones().await;
    assert_eq!(ejecuciones.len(), 2);
    assert_eq!(ejecuciones[0].side, "BOT");
    assert_eq!(ejecuciones[1].side, "SLD");

    // 6. A limit above and a stop below protect the new long like an OCA pair.
    traductor
        .place(SISTEMA, orden(con_id, "BUY", "MKT", 1.0, 0.0))
        .await
        .expect("reopen");
    let nueva = posiciones(&enlace, 1).await.remove(0);
    let objetivo = traductor
        .place(
            SISTEMA,
            orden(con_id, "SELL", "LMT", 1.0, nueva.current + 100.0),
        )
        .await
        .expect("target");
    assert_eq!(objetivo.role, "exit_tp");
    let con_tp = esperar_campo(&enlace, nueva.ticket, |p| p.tp > 0.0).await;
    let stop = traductor
        .place(
            SISTEMA,
            orden(con_id, "SELL", "STP", 1.0, nueva.current - 100.0),
        )
        .await
        .expect("stop");
    assert_eq!(stop.role, "exit_sl");
    let con_ambos = esperar_campo(&enlace, nueva.ticket, |p| p.sl > 0.0 && p.tp > 0.0).await;
    assert!((con_ambos.tp - con_tp.tp).abs() < 0.02);

    // 7. Cancelling the target leaves the stop; cancelling it clears the pair.
    traductor.cancel(SISTEMA, objetivo.order_id).await.expect("cancel tp");
    let solo_sl = esperar_campo(&enlace, nueva.ticket, |p| p.tp == 0.0 && p.sl > 0.0).await;
    assert!((solo_sl.sl - (nueva.current - 100.0)).abs() < 0.02);
    traductor.cancel(SISTEMA, stop.order_id).await.expect("cancel sl");
    esperar_campo(&enlace, nueva.ticket, |p| p.sl == 0.0 && p.tp == 0.0).await;

    // 8. Bars come from the same account.
    let barras = traductor
        .bars(SISTEMA, con_id, None, "1 D", "1 hour")
        .await
        .expect("bars");
    if !barras.is_empty() {
        assert!(barras.first().unwrap().0 < barras.last().unwrap().0);
    }

    // 9. A stopped system's working market orders end.
    traductor.sistema_parado(SISTEMA).await;
    let panico = traductor.cancelar_todo().await;
    assert!(panico.fallos.is_empty());

    enlace.parar().await;
    bombeo.abort();
    let _ = std::fs::remove_dir_all(&carpeta);
}
