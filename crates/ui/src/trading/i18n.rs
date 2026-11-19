//! B4 — engine messages in Spanish.
//!
//! The engine words every log row and verdict in English and attaches an i18n
//! key (`trading.close.*`, `trading.log.*`, `trading.panic.*`) plus the params
//! its template interpolates. This module renders that key from Openbot's
//! Spanish catalog ([`super::i18n_es`], generated) or from Aurea's own keys
//! ([`aurea`]), with the same fallback rule Openbot's `textoRegistro` uses: a
//! key the catalog does not know — or a template whose `{{param}}` the row
//! does not carry — shows the engine's raw message instead of a half-filled
//! sentence.

use serde_json::{Map, Value};
use zeron_proto::trading::{Log, PanicResult, PositionCloseResult};

use super::i18n_es;

/// Aurea's keys: the ones its engine (and its demo fixture) emits that
/// Openbot's catalog never had.
fn aurea(key: &str) -> Option<&'static str> {
    Some(match key {
        "trading.close.ok" => "Posición cerrada.",
        "trading.close.not_open" => "Esa posición ya no está abierta.",
        "trading.close.nothing" => "Sin posiciones abiertas: nada que cerrar.",
        "trading.close.all_ok" => "Todas las posiciones cerradas.",
        "trading.close.all_failed" => {
            "Cierre incompleto: {{failed}} fallo(s), {{open}} posición(es) siguen abiertas."
        }
        "trading.close.wire_unsupported" => {
            "Esta versión de TWS ({{version}}) no está soportada para órdenes: cierra {{symbol}} desde TWS."
        }
        "trading.close.limit_sent" => {
            "El cierre a mercado falló{{reason}}; enviado un límite {{action}} {{quantity}} {{symbol}} a {{prices}} GTC (orden {{orders}}). Revisa Órdenes."
        }
        "trading.close.oca_sent" => {
            "El cierre a mercado falló{{reason}}; enviados 2 límites OCA {{action}} {{quantity}} {{symbol}} a {{prices}} GTC (órdenes {{orders}}). Uno cancela al otro. Revisa Órdenes."
        }
        "trading.panic.done" => "Pánico ejecutado.",
        "trading.log.system_started" => "Bot arrancado.",
        "trading.log.system_stopped" => "Bot detenido.",
        "trading.log.bot.stopped_user" => "Bot detenido por el usuario.",
        "trading.log.bot.stopped_task" => "Bot detenido por una tarea programada.",
        "trading.log.bot.stopped_panic" => "Bot detenido por el Pánico.",
        "trading.log.tws_reconnect" => "TWS: reconectando.",
        "trading.log.task_added" => "Tarea programada.",
        "trading.log.task_cancelled" => "Tarea cancelada.",
        "trading.log.link_added" => "Conexión añadida.",
        "trading.log.link_removed" => "Conexión eliminada.",
        "trading.log.pool_selected" => "Pool seleccionado.",
        _ => return None,
    })
}

/// The template for `key`: Openbot's Spanish first, then Aurea's own.
fn template(key: &str) -> Option<&'static str> {
    i18n_es::es(key).or_else(|| aurea(key))
}

/// One param as the sentence shows it, or `None` when it cannot stand in a
/// sentence (null, an object).
fn param_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(match number.as_f64() {
            Some(float) if float.fract() == 0.0 && float.abs() < 1e15 => format!("{}", float as i64),
            _ => number.to_string(),
        }),
        Value::Bool(flag) => Some(if *flag { "sí" } else { "no" }.into()),
        Value::Array(items) => {
            let parts: Option<Vec<String>> = items.iter().map(param_text).collect();
            parts.map(|parts| parts.join(", "))
        }
        Value::Null | Value::Object(_) => None,
    }
}

/// Fills `{{name}}` placeholders; `None` if any of them has no usable param.
fn fill(template: &str, params: Option<&Map<String, Value>>) -> Option<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find("}}")?;
        let name = after[..end].trim();
        let value = params.and_then(|params| params.get(name)).and_then(param_text)?;
        out.push_str(&value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Some(out)
}

/// `key` rendered in Spanish, else `fallback` (the engine's raw message).
pub fn text(key: Option<&str>, params: Option<&Map<String, Value>>, fallback: &str) -> String {
    key.and_then(template)
        .and_then(|template| fill(template, params))
        .unwrap_or_else(|| fallback.to_string())
}

/// A log row's message.
pub fn log_text(log: &Log) -> String {
    text(log.message_key.as_deref(), log.message_params.as_ref(), &log.message)
}

/// A close verdict (one position or close-all).
pub fn verdict_text(result: &PositionCloseResult) -> String {
    text(result.message_key.as_deref(), result.message_params.as_ref(), &result.message)
}

/// The panic summary, worded here from the result's facts: Openbot's
/// `trading.panic.summary` interpolates sub-phrases (`failed`, `cancelled`,
/// `mt5`) that its app pre-renders, so the template alone would read wrong.
/// Leftovers come last, as Openbot's `textoSobrante` adds them.
pub fn panic_text(result: &PanicResult) -> String {
    if result.message_key.as_deref() != Some("trading.panic.summary") {
        return text(
            result.message_key.as_deref(),
            result.message_params.as_ref(),
            &result.message,
        );
    }
    let params = result.message_params.as_ref();
    let number = |name: &str| {
        params
            .and_then(|params| params.get(name))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };
    let mut out = format!("Pánico: {} bot(s) parado(s)", result.stopped.len());
    let failed = number("failed");
    if failed > 0 {
        out.push_str(&format!(", {failed} sin poder parar"));
    }
    let cancelled = number("cancelled");
    if cancelled > 0 {
        out.push_str(&format!(", {cancelled} tarea(s) cancelada(s)"));
    }
    out.push_str(". ");
    out.push_str(&verdict_text(&result.close));
    if number("leftover_count") > 0 {
        let symbols = params
            .and_then(|params| params.get("leftover_symbols"))
            .and_then(param_text)
            .unwrap_or_default();
        if let Some(line) = fill(
            template("trading.engine.panic.summary.unclosed").unwrap_or(""),
            Some(&Map::from_iter([("symbols".to_string(), Value::String(symbols))])),
        ) {
            out.push(' ');
            out.push_str(&line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    fn verdict(key: &str, value: Value, message: &str) -> PositionCloseResult {
        PositionCloseResult {
            ok: false,
            message: message.into(),
            message_key: Some(key.into()),
            message_params: Some(params(value)),
        }
    }

    #[test]
    fn a_catalog_key_renders_in_spanish_with_its_params() {
        let result = verdict("trading.close.no_size", json!({ "symbol": "MNQZ6" }), "No size");
        assert_eq!(verdict_text(&result), "La posición MNQZ6 no tiene tamaño que cerrar.");
    }

    #[test]
    fn an_unknown_key_or_a_missing_param_keeps_the_raw_message() {
        assert_eq!(text(Some("trading.close.from_the_future"), None, "raw"), "raw");
        assert_eq!(text(Some("trading.close.no_size"), None, "raw"), "raw");
        assert_eq!(text(None, None, "raw"), "raw");
    }

    #[test]
    fn aurea_keys_cover_what_openbot_never_had() {
        let result = verdict(
            "trading.close.all_failed",
            json!({ "count": 3, "failed": 1, "open": 2 }),
            "1 failed, 2 still open.",
        );
        assert_eq!(
            verdict_text(&result),
            "Cierre incompleto: 1 fallo(s), 2 posición(es) siguen abiertas."
        );
    }

    #[test]
    fn params_read_as_a_sentence_would() {
        assert_eq!(param_text(&json!(3.0)).as_deref(), Some("3"));
        assert_eq!(param_text(&json!(1.5)).as_deref(), Some("1.5"));
        assert_eq!(param_text(&json!(["A", "B"])).as_deref(), Some("A, B"));
        assert_eq!(param_text(&json!(null)), None);
    }

    #[test]
    fn the_panic_summary_names_stops_tasks_close_and_leftovers() {
        let result = PanicResult {
            stopped: vec!["01_a".into(), "02_b".into()],
            close: verdict("trading.close.all_ok", json!({ "count": 1 }), "All closed."),
            message: "Panic executed.".into(),
            message_key: Some("trading.panic.summary".into()),
            message_params: Some(params(json!({
                "failed": 0,
                "cancelled": 1,
                "leftover_symbols": ["EURUSD"],
                "leftover_count": 1,
            }))),
        };
        let line = panic_text(&result);
        assert!(line.starts_with("Pánico: 2 bot(s) parado(s), 1 tarea(s) cancelada(s). "), "{line}");
        assert!(line.contains("Todas las posiciones cerradas."), "{line}");
        assert!(line.contains("EURUSD"), "{line}");
    }
}
