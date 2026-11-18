//! Live check of the MT5 bridge client against a running MetaTrader 5.
//!
//! ```text
//! $env:MT5_LOGIN="<login>"; $env:MT5_SERVER="<server>"
//! cargo run -p zeron-trading --example mt5_live
//! ```
//!
//! Writes the embedded `puente.py` next to a temp offset file, starts it with
//! the Python that has `MetaTrader5` installed (`MT5_PYTHON` overrides the
//! lookup), waits for the first book and asks for one symbol. Read-only: it
//! never sends `DEAL`, `PENDING`, `SLTP`, `CLOSEVOL` or `DELETE`.

use std::path::PathBuf;
use std::time::Duration;

use zeron_trading::mt5::puente_cliente::PuenteMt5;

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

    let carpeta = std::env::temp_dir().join("aurea-mt5-live");
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

    println!("python: {}", python.display());
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

    let mut libro = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(leido) = puente.libro().await {
            libro = Some(leido);
            break;
        }
        if let Some(motivo) = puente.motivo().await {
            println!("bridge: {motivo}");
        }
    }
    let Some(libro) = libro else {
        println!("no book after 20 s; is the terminal open and logged in?");
        puente.parar().await;
        return;
    };

    println!(
        "libro: {}{} {} demo={} leverage={} balance={:?} equity={:?} positions={} pending={}",
        if libro.demo { "DU" } else { "U" },
        libro.login,
        libro.server,
        libro.demo,
        libro.leverage,
        libro.balance,
        libro.equity,
        libro.positions.len(),
        libro.orders.len()
    );
    for posicion in &libro.positions {
        println!(
            "  position {} {} {} {} lots @ {} now {} sl={} tp={} magic={}",
            posicion.ticket,
            posicion.symbol,
            posicion.side,
            posicion.volume,
            posicion.entry,
            posicion.current,
            posicion.sl,
            posicion.tp,
            posicion.magic
        );
    }

    let spec = puente.comando("SYMBOL", "EURUSD").await;
    println!(
        "SYMBOL EURUSD: ok={} retcode={} message={:?}",
        spec.ok, spec.retcode, spec.message
    );
    if let Some(datos) = spec.data.as_ref() {
        println!("  {}", serde_json::to_string(datos).unwrap_or_default());
    }

    puente.parar().await;
    println!("bridge stopped");
}
