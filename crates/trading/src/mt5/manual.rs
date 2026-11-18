//! Manual close from the desk (`mt5-ordenes.ts`).
//!
//! One short-lived Python run per request, over the same MetaTrader5 package
//! the read bridge uses: it attaches to the running terminal, checks that the
//! terminal is logged into the very account the request names (never another
//! one), and closes at market every position the account holds on a symbol
//! (the systems open them). The terminal must allow algorithmic trading
//! ("Algo Trading" on); the run says so plainly when it does not.
//!
//! The app never launches MT5 on its own, so the script only attaches while
//! `terminal64.exe` is running: when the process list cannot be read at all it
//! answers unknown and sends nothing.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// A close run's cap, like the TS side.
pub const ESPERA_ORDEN: Duration = Duration::from_secs(30);

/// What to close: every position the account holds on `symbolo`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PeticionCierre {
    pub accion: String,
    pub login: i64,
    pub server: String,
    pub symbol: String,
}

impl PeticionCierre {
    pub fn cerrar(login: i64, servidor: impl Into<String>, simbolo: impl Into<String>) -> Self {
        Self {
            accion: "cerrar".to_string(),
            login,
            server: servidor.into(),
            symbol: simbolo.into(),
        }
    }
}

/// The run's verdict, in the user's words.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RespuestaMt5 {
    pub ok: bool,
    pub message: String,
}

impl RespuestaMt5 {
    pub fn bien(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }

    pub fn mal(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
        }
    }
}

/// The python side. Every answer is one JSON line `{ok, message}` in the
/// user's words, and its verdict is the last line of the run.
pub const ORDEN_PY: &str = r#"import json, subprocess, sys

def terminal_abierto():
    try:
        hecho = subprocess.run(["tasklist", "/FI", "IMAGENAME eq terminal64.exe", "/NH"], capture_output=True, text=True, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    except Exception:
        return None
    if hecho.returncode != 0:
        return None
    return "terminal64.exe" in hecho.stdout.lower()

def responder(ok, mensaje):
    print(json.dumps({"ok": ok, "message": mensaje}), flush=True)
    sys.exit(0)

def relleno(mt5, info):
    # The symbol's own filling policy: FOK or IOC when it allows them, else RETURN.
    modos = getattr(info, "filling_mode", 0) or 0
    if modos & 1:
        return mt5.ORDER_FILLING_FOK
    if modos & 2:
        return mt5.ORDER_FILLING_IOC
    return mt5.ORDER_FILLING_RETURN

def enviar(mt5, peticion):
    resultado = mt5.order_send(peticion)
    if resultado is None:
        codigo, texto = mt5.last_error()
        return False, "MetaTrader 5 no aceptó la orden (" + str(codigo) + ": " + str(texto) + ")"
    if resultado.retcode in (mt5.TRADE_RETCODE_DONE, mt5.TRADE_RETCODE_PLACED, mt5.TRADE_RETCODE_DONE_PARTIAL):
        return True, resultado
    if resultado.retcode == mt5.TRADE_RETCODE_CLIENT_DISABLES_AT:
        return False, "MetaTrader 5 tiene desactivado el trading algorítmico: pulsa «Algo Trading» en la barra del terminal y reintenta"
    if resultado.retcode == mt5.TRADE_RETCODE_MARKET_CLOSED:
        return False, "El mercado de " + peticion["symbol"] + " está cerrado ahora"
    if resultado.retcode == mt5.TRADE_RETCODE_NO_MONEY:
        return False, "No hay margen suficiente en la cuenta para esa orden"
    if resultado.retcode == mt5.TRADE_RETCODE_INVALID_VOLUME:
        return False, "Volumen no válido para " + peticion["symbol"]
    return False, "MetaTrader 5 rechazó la orden (" + str(resultado.retcode) + ": " + str(resultado.comment) + ")"

def main():
    pedido = json.loads(sys.argv[1])
    try:
        import MetaTrader5 as mt5
    except Exception as causa:
        responder(False, "Falta el paquete de Python MetaTrader5: " + str(causa))
    # initialize() would launch a closed terminal: a close only ever attaches to a running one.
    abierto = terminal_abierto()
    if abierto is None:
        responder(False, "No puedo comprobar si el terminal de MetaTrader 5 está abierto (falla la lista de procesos): no se envió nada")
    if not abierto:
        responder(False, "El terminal de MetaTrader 5 está cerrado: ábrelo, inicia sesión en la cuenta y reintenta. No se envió nada")
    if not mt5.initialize():
        responder(False, "El terminal de MetaTrader 5 no responde: ábrelo e inicia sesión en la cuenta")
    cuenta = mt5.account_info()
    if cuenta is None:
        responder(False, "El terminal de MetaTrader 5 no tiene ninguna cuenta con sesión iniciada")
    if cuenta.login != pedido["login"] or cuenta.server != pedido["server"]:
        responder(False, "El terminal está en la cuenta " + str(cuenta.login) + " (" + cuenta.server + "), no en la " + str(pedido["login"]) + ": no se envió nada")
    terminal = mt5.terminal_info()
    if terminal is not None and not terminal.trade_allowed:
        responder(False, "MetaTrader 5 tiene desactivado el trading algorítmico: pulsa «Algo Trading» en la barra del terminal y reintenta")
    simbolo = pedido["symbol"]
    if not mt5.symbol_select(simbolo, True):
        responder(False, "MetaTrader 5 no conoce el símbolo " + simbolo + " en esta cuenta")
    info = mt5.symbol_info(simbolo)
    if info is None:
        responder(False, "MetaTrader 5 no da información de " + simbolo)

    abiertas = mt5.positions_get(symbol=simbolo) or ()
    if len(abiertas) == 0:
        responder(True, "La posición de " + simbolo + " ya no está abierta")
    cerradas = 0
    fallos = []
    for posicion in abiertas:
        tick = mt5.symbol_info_tick(simbolo)
        larga = posicion.type == mt5.POSITION_TYPE_BUY
        ok, salida = enviar(mt5, {
            "action": mt5.TRADE_ACTION_DEAL,
            "symbol": simbolo,
            "volume": posicion.volume,
            "type": mt5.ORDER_TYPE_SELL if larga else mt5.ORDER_TYPE_BUY,
            "position": posicion.ticket,
            "price": (tick.bid if larga else tick.ask) if tick is not None else 0.0,
            "deviation": 20,
            "magic": 70910,
            "comment": "manual close",
            "type_time": mt5.ORDER_TIME_GTC,
            "type_filling": relleno(mt5, info),
        })
        if ok:
            cerradas += 1
        else:
            fallos.append(str(posicion.ticket) + ": " + salida)
    if fallos:
        responder(False, "Cerradas " + str(cerradas) + " de " + str(len(abiertas)) + " posiciones de " + simbolo + "; fallaron " + "; ".join(fallos))
    responder(True, "Cierre de " + simbolo + " ejecutado (" + str(cerradas) + " posición" + ("" if cerradas == 1 else "es") + ")")

main()
"#;

/// Closes at market every position of the account on the request's symbol.
pub async fn cerrar_manual(python: &Path, peticion: &PeticionCierre) -> RespuestaMt5 {
    orden_mt5(python, peticion, ESPERA_ORDEN).await
}

/// One order run, capped at `espera`; its verdict is the run's own JSON line.
pub async fn orden_mt5(
    python: &Path,
    peticion: &PeticionCierre,
    espera: Duration,
) -> RespuestaMt5 {
    let pedido = match serde_json::to_string(peticion) {
        Ok(pedido) => pedido,
        Err(error) => return RespuestaMt5::mal(format!("petición ilegible: {error}")),
    };
    let mut command = Command::new(python);
    command
        .arg("-c")
        .arg(ORDEN_PY)
        .arg(pedido)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        // No console window for the one-shot run.
        command.creation_flags(0x0800_0000);
    }
    let mut hijo = match command.spawn() {
        Ok(hijo) => hijo,
        Err(error) => {
            return RespuestaMt5::mal(format!(
                "No se pudo lanzar Python para MetaTrader 5: {error}"
            ));
        }
    };
    let Some(stdout) = hijo.stdout.take() else {
        let _ = hijo.start_kill();
        return RespuestaMt5::mal("Python no abrió la salida: no se pudo leer la respuesta");
    };
    let lectura = tokio::spawn(async move {
        let mut salida = String::new();
        let mut lineas = BufReader::new(stdout).lines();
        while let Ok(Some(linea)) = lineas.next_line().await {
            salida.push_str(&linea);
            salida.push('\n');
        }
        salida
    });
    match tokio::time::timeout(espera, hijo.wait()).await {
        Ok(_) => {
            let salida = lectura.await.unwrap_or_default();
            leer_respuesta(&salida)
        }
        Err(_) => {
            let _ = hijo.start_kill();
            let _ = hijo.wait().await;
            lectura.abort();
            RespuestaMt5::mal(
                "MetaTrader 5 no contestó en 30 s: revisa el terminal y el registro antes de reintentar",
            )
        }
    }
}

/// The verdict is the last readable `{ok, message}` line of the run.
pub fn leer_respuesta(salida: &str) -> RespuestaMt5 {
    let linea = salida
        .lines()
        .map(str::trim)
        .filter(|linea| !linea.is_empty())
        .next_back()
        .unwrap_or("");
    serde_json::from_str(linea).unwrap_or_else(|_| {
        RespuestaMt5::mal(
            "MetaTrader 5 no devolvió una respuesta legible: revisa el terminal antes de reintentar",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_names_the_verb_account_and_symbol() {
        let pedido = PeticionCierre::cerrar(115_771_412, "MetaQuotes-Demo", "EURUSD");
        assert_eq!(
            serde_json::to_string(&pedido).unwrap(),
            r#"{"accion":"cerrar","login":115771412,"server":"MetaQuotes-Demo","symbol":"EURUSD"}"#
        );
    }

    #[test]
    fn the_verdict_is_the_last_readable_line() {
        let salida = "ruido\n{\"ok\": true, \"message\": \"Cierre de XAUUSD ejecutado (1 posición)\"}\n";
        assert_eq!(
            leer_respuesta(salida),
            RespuestaMt5::bien("Cierre de XAUUSD ejecutado (1 posición)")
        );
    }

    #[test]
    fn a_run_without_a_json_line_says_so() {
        let respuesta = leer_respuesta("Traceback (most recent call last):\n  ...\n");
        assert!(!respuesta.ok);
        assert!(respuesta.message.contains("no devolvió una respuesta legible"));
        assert!(!leer_respuesta("").ok);
    }

    #[test]
    fn a_failure_verdict_is_passed_through() {
        let salida = "{\"ok\": false, \"message\": \"MetaTrader 5 tiene desactivado el trading algorítmico\"}";
        let respuesta = leer_respuesta(salida);
        assert!(!respuesta.ok);
        assert!(respuesta.message.contains("trading algorítmico"));
    }

    #[test]
    fn the_script_attaches_to_a_running_terminal_only() {
        assert!(ORDEN_PY.contains("def terminal_abierto()"));
        assert!(ORDEN_PY.contains("tasklist"));
        assert!(ORDEN_PY.contains("if not abierto:"));
        assert!(ORDEN_PY.contains("mt5.initialize()"));
        assert!(ORDEN_PY.contains("cuenta.login != pedido[\"login\"]"));
        assert!(ORDEN_PY.contains("\"magic\": 70910"));
    }
}
