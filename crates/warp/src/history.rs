//! The rotation history: one JSON line per finished run.
//!
//! `ghost-historial.jsonl` is the engine's own log. On a machine whose runs
//! predate it, the first read seeds it from today's landed rotations in the
//! event log — the bat and the engine land with the same words, so one
//! pattern catches both.

use chrono::{Datelike, Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};

/// One finished rotation, as the settings page and its day count read it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GhostRun {
    /// Unix milliseconds when the run started.
    pub at: i64,
    /// `manual`, or whoever asked (the gateway's limit).
    pub origin: String,
    pub ok: bool,
    pub attempts: u32,
    #[serde(rename = "durationMs")]
    pub duration_ms: u64,
    /// The bucket the machine left, when it was known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The `/64` it landed on, only when it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

impl GhostRun {
    /// The line a finished run appends to the history.
    pub fn registrar(
        at: i64,
        origin: Option<&str>,
        ok: bool,
        attempts: u32,
        duration_ms: u64,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Self {
        Self {
            at,
            origin: origin.unwrap_or("manual").to_string(),
            ok,
            attempts,
            duration_ms,
            from: from.map(str::to_string),
            to: to.filter(|_| ok).map(str::to_string),
        }
    }
}

/// Serializes runs as the JSONL file holds them (one per line, nothing when
/// empty).
pub fn a_jsonl(runs: &[GhostRun]) -> String {
    runs.iter()
        .filter_map(|run| serde_json::to_string(run).ok())
        .map(|linea| format!("{linea}\n"))
        .collect()
}

/// Reads the history file, skipping lines that are not a run.
pub fn leer_historial(texto: &str) -> Vec<GhostRun> {
    texto
        .lines()
        .filter_map(|linea| serde_json::from_str(linea.trim()).ok())
        .collect()
}

/// Today's landed rotations from the event log, in file order.
pub fn seed(eventos: &str, hoy: NaiveDate) -> Vec<GhostRun> {
    eventos
        .lines()
        .filter_map(parse_evento)
        .filter(|(dia, ..)| *dia == hoy)
        .filter_map(|(_, (hora, minuto, segundo), bucket)| {
            let at = Local
                .with_ymd_and_hms(
                    hoy.year(),
                    hoy.month(),
                    hoy.day(),
                    hora,
                    minuto,
                    segundo,
                )
                .single()?
                .timestamp_millis();
            Some(GhostRun {
                at,
                origin: "manual".into(),
                ok: true,
                attempts: 1,
                duration_ms: 0,
                from: None,
                to: Some(bucket),
            })
        })
        .collect()
}

/// `[18/11/2026 03:04:05] rotate: /64 nuevo 2a09:bac5:6de7:31e1`
fn parse_evento(linea: &str) -> Option<(NaiveDate, (u32, u32, u32), String)> {
    let resto = linea.strip_prefix('[')?;
    let (sello, resto) = resto.split_once("] ")?;
    let mut partes = sello.split(['/', ' ', ':']);
    let dia: u32 = partes.next()?.parse().ok()?;
    let mes: u32 = partes.next()?.parse().ok()?;
    let anio: i32 = partes.next()?.parse().ok()?;
    let hora: u32 = partes.next()?.parse().ok()?;
    let minuto: u32 = partes.next()?.parse().ok()?;
    let segundo: u32 = partes.next()?.parse().ok()?;
    if partes.next().is_some() {
        return None;
    }
    let resto = resto
        .strip_prefix("sesion: ")
        .or_else(|| resto.strip_prefix("rotate: "))?;
    let bucket = resto
        .strip_prefix("/64 nuevo ")?
        .split_whitespace()
        .next()?;
    Some((
        NaiveDate::from_ymd_opt(anio, mes, dia)?,
        (hora, minuto, segundo),
        bucket.to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    const EVENTOS: &str = "[17/11/2026 21:00:00] sesion: /64 nuevo 2606:4700:78:0\n\
[18/11/2026 01:02:03] rotate: /64 nuevo 2a09:bac5:6de7:31e1\n\
[18/11/2026 04:05:06] sesion: /64 nuevo 2606:4700:78:1\n\
[18/11/2026 04:06:07] algo distinto\n\
basura\n";

    fn hoy() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 18).unwrap()
    }

    #[test]
    fn only_todays_landed_rotations_are_seeded() {
        let runs = seed(EVENTOS, hoy());
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].to.as_deref(), Some("2a09:bac5:6de7:31e1"));
        assert_eq!(runs[1].to.as_deref(), Some("2606:4700:78:1"));
        for run in &runs {
            assert!(run.ok);
            assert_eq!(run.attempts, 1);
            assert_eq!(run.duration_ms, 0);
            assert_eq!(run.origin, "manual");
        }
    }

    #[test]
    fn the_event_timestamp_becomes_local_unix_milliseconds() {
        let runs = seed(EVENTOS, hoy());
        let primero = Local
            .with_ymd_and_hms(2026, 11, 18, 1, 2, 3)
            .single()
            .unwrap();
        assert_eq!(runs[0].at, primero.timestamp_millis());
    }

    #[test]
    fn a_malformed_line_is_skipped_not_fatal() {
        assert!(seed("[xx/yy/2026 01:02:03] rotate: /64 nuevo a::b", hoy()).is_empty());
        assert!(seed("[18/11/2026 01:02:03] rotate: sin bucket", hoy()).is_empty());
        assert!(seed("[18/11/2026] rotate: /64 nuevo a::b", hoy()).is_empty());
    }

    #[test]
    fn registration_omits_absent_fields_and_never_records_a_target_on_failure() {
        let ok = GhostRun::registrar(1, None, true, 3, 250, Some("a:0:0:0"), Some("b:0:0:0"));
        let json = serde_json::to_value(&ok).unwrap();
        assert_eq!(json["origin"], "manual");
        assert_eq!(json["to"], "b:0:0:0");
        assert_eq!(json["from"], "a:0:0:0");
        assert_eq!(json["attempts"], 3);
        assert_eq!(json["durationMs"], 250);

        let fallo = GhostRun::registrar(1, Some("gateway"), false, 10, 900, None, Some("b:0:0:0"));
        let json = serde_json::to_value(&fallo).unwrap();
        assert_eq!(json["origin"], "gateway");
        assert!(json.get("to").is_none());
        assert!(json.get("from").is_none());
    }

    #[test]
    fn jsonl_round_trips_and_skips_damaged_lines() {
        let runs = seed(EVENTOS, hoy());
        let texto = a_jsonl(&runs);
        assert_eq!(texto.lines().count(), 2);
        let mut con_basura = texto.clone();
        con_basura.push_str("no-json\n");
        assert_eq!(leer_historial(&con_basura), runs);

        // The minute the file is read is not the minute it was written.
        let run = &runs[0];
        assert_eq!(Local.timestamp_millis_opt(run.at).unwrap().hour(), 1);
    }
}
