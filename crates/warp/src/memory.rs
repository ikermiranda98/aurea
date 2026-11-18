//! The day's `/64` memory: `opencode-warp-ips.log` and its rotation.
//!
//! The counter lives on the UTC date file: when the day moves the memory is
//! erased, so a bucket that was used yesterday may be used again today. The
//! log is also bounded — over 60 lines it keeps the last 50 — and every
//! rotation reads it fresh so a bucket recorded by one attempt counts for the
//! next.

/// The memory is pruned once it grows past this many lines.
pub const LIMITE_MEMORIA: usize = 60;
/// How many newest buckets survive a prune.
pub const MEMORIA_CONSERVADA: usize = 50;
/// The event log is trimmed to this many lines before the ladder starts.
pub const LIMITE_EVENTOS: usize = 200;

/// Whether the UTC day moved since the date file was last written. An empty
/// previous day (first run) does not count as a rotation.
pub fn dia_girado(anterior: &str, hoy: &str) -> bool {
    let anterior = anterior.lines().next().unwrap_or("").trim();
    !anterior.is_empty() && anterior != hoy
}

/// The memory as it should be written after a prune, or `None` when it is
/// still inside the limit and must not be touched.
pub fn ajustar_memoria(lineas: &[String]) -> Option<Vec<String>> {
    if lineas.len() <= LIMITE_MEMORIA {
        return None;
    }
    Some(lineas[lineas.len() - MEMORIA_CONSERVADA..].to_vec())
}

/// The event log trimmed to its newest lines, or `None` when it fits already.
pub fn podar_eventos(lineas: &[String]) -> Option<Vec<String>> {
    if lineas.len() <= LIMITE_EVENTOS {
        return None;
    }
    Some(lineas[lineas.len() - LIMITE_EVENTOS..].to_vec())
}

/// Whether this exact bucket is already in the memory.
pub fn recordado(lineas: &[String], bucket: &str) -> bool {
    lineas.iter().any(|linea| linea.trim() == bucket)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lineas(cantidad: usize) -> Vec<String> {
        (1..=cantidad).map(|n| format!("bucket-{n}")).collect()
    }

    #[test]
    fn a_new_utc_day_wipes_the_memory_once() {
        assert!(dia_girado("2026-11-17\n", "2026-11-18"));
        assert!(!dia_girado("2026-11-18\n", "2026-11-18"));
        assert!(!dia_girado("", "2026-11-18"));
        assert!(!dia_girado("\n", "2026-11-18"));
        // Only the first line is the date; a trailing note is ignored.
        assert!(!dia_girado("2026-11-18\nnota", "2026-11-18"));
    }

    #[test]
    fn the_memory_keeps_its_newest_fifty_only_when_it_overflows() {
        assert_eq!(ajustar_memoria(&lineas(60)), None);
        let ajustadas = ajustar_memoria(&lineas(61)).unwrap();
        assert_eq!(ajustadas.len(), 50);
        assert_eq!(ajustadas.first().unwrap(), "bucket-12");
        assert_eq!(ajustadas.last().unwrap(), "bucket-61");
    }

    #[test]
    fn the_event_log_keeps_its_newest_two_hundred() {
        assert_eq!(podar_eventos(&lineas(200)), None);
        let podadas = podar_eventos(&lineas(201)).unwrap();
        assert_eq!(podadas.len(), 200);
        assert_eq!(podadas.first().unwrap(), "bucket-2");
        assert_eq!(podadas.last().unwrap(), "bucket-201");
    }

    #[test]
    fn remembered_buckets_match_whole_lines() {
        let memoria = vec!["2606:4700:78:0".to_string(), "2a09:bac5:6de7:31e1".to_string()];
        assert!(recordado(&memoria, "2606:4700:78:0"));
        assert!(recordado(&memoria, "2a09:bac5:6de7:31e1"));
        assert!(!recordado(&memoria, "2606:4700:78"));
        assert!(!recordado(&memoria, "2606:4700:78:0 ")); // trailing space is trimmed
        assert!(!recordado(&memoria, "2606:4700:78:0:1"));
    }
}
