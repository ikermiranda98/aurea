//! The instrument a bot asked for, and the MT5 symbol that stands for it.
//!
//! A direct port of the constants and identity helpers in `bots-mt5.ts`:
//! futures roots and the broker symbols that follow the same market, the
//! stable `conId` every root gets, IB duration strings and default magics.
//! Resolving a symbol needs the link's bridge, so [`Instrumento`] is built by
//! the translator; these helpers stay pure.

use serde::{Deserialize, Serialize};

/// One tradable thing a bot asked for, and the MT5 symbol standing for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Instrumento {
    pub con_id: i64,
    /// MNQ, MGC, USDJPY…
    pub root: String,
    /// What the bot asked for: FUT, CONTFUT, CASH, CFD.
    pub sec_type: String,
    pub exchange: String,
    pub currency: String,
    /// Money per point of one bot contract (futures and CFDs); unused for forex.
    pub multiplier: f64,
    /// Bot quantities are units of the base currency.
    pub forex: bool,
    /// The MT5 link (`mt5-<login>@<server>`) the symbol was resolved on.
    pub account: String,
    pub mt5: String,
    pub digits: i32,
    pub contract_size: f64,
    pub tick_size: f64,
    pub tick_value: f64,
    pub volume_min: f64,
    pub volume_step: f64,
    pub volume_max: f64,
}

const NASDAQ: &[&str] = &[
    "US100", "USTEC", "NAS100", "NDX100", "USTECH", "NQ100", "US100Cash", "NASDAQ",
];
const SP500: &[&str] = &["US500", "SPX500", "SP500", "US500Cash", "USA500", "SPX"];
const RUSSELL: &[&str] = &[
    "US2000",
    "RUS2000",
    "RUSSELL2000",
    "US2000Cash",
    "R2000",
    "USSC2000",
    "RTY",
];
const DOW: &[&str] = &["US30", "DJ30", "WS30", "US30Cash", "DOW30", "USA30"];
const ORO: &[&str] = &["XAUUSD", "GOLD", "XAUUSD.", "GOLDmicro"];
const PLATA: &[&str] = &["XAGUSD", "SILVER"];
const PETROLEO: &[&str] = &["USOIL", "WTI", "XTIUSD", "CL-OIL", "USOUSD"];

/// Futures roots: money per point of one contract and the MT5 symbols that
/// follow the same market.
pub fn futuro(root: &str) -> Option<(f64, &'static [&'static str])> {
    Some(match root {
        "MNQ" => (2.0, NASDAQ),
        "NQ" => (20.0, NASDAQ),
        "MES" => (5.0, SP500),
        "ES" => (50.0, SP500),
        "M2K" => (5.0, RUSSELL),
        "RTY" => (50.0, RUSSELL),
        "MYM" => (0.5, DOW),
        "YM" => (5.0, DOW),
        "MGC" => (10.0, ORO),
        "GC" => (100.0, ORO),
        "SIL" => (1000.0, PLATA),
        "SI" => (5000.0, PLATA),
        "MCL" => (100.0, PETROLEO),
        "CL" => (1000.0, PETROLEO),
        _ => return None,
    })
}

/// Common broker suffixes for the same symbol.
pub const SUFIJOS: &[&str] = &[
    "", ".cash", ".a", "m", ".m", ".pro", ".raw", "+", ".i", "_i",
];

/// IB bar sizes → MT5 timeframes.
pub fn marco(tamano: &str) -> Option<&'static str> {
    Some(match tamano.trim().to_lowercase().as_str() {
        "1 min" => "M1",
        "2 mins" => "M2",
        "3 mins" => "M3",
        "5 mins" => "M5",
        "10 mins" => "M10",
        "15 mins" => "M15",
        "20 mins" => "M20",
        "30 mins" => "M30",
        "1 hour" | "1 hours" | "60 mins" => "H1",
        "2 hours" => "H2",
        "3 hours" => "H3",
        "4 hours" => "H4",
        "8 hours" => "H8",
        "1 day" => "D1",
        "1 week" => "W1",
        _ => return None,
    })
}

/// A non-future root of exactly six letters is a forex pair.
pub fn es_forex(root: &str) -> bool {
    futuro(root).is_none()
        && root.len() == 6
        && root.bytes().all(|byte| byte.is_ascii_alphabetic())
}

/// A stable `conId` per root (FNV-1a), the same on every run and machine.
pub fn con_id_de(root: &str) -> i64 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in root.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    i64::from((hash & 0x3fff_ffff) + 100_000)
}

/// IB duration strings in seconds. `"N D"` counts trading days, so weekends
/// are added; anything else is unsupported.
pub fn duracion_segundos(d: &str) -> Option<i64> {
    let mut partes = d.trim().split_whitespace();
    let cantidad = partes.next()?.parse::<f64>().ok()?;
    if !cantidad.is_finite() || cantidad.fract() != 0.0 {
        return None;
    }
    let cantidad = cantidad as i64;
    let unidad = partes.next()?.to_uppercase();
    const DIA: i64 = 86_400;
    Some(match unidad.as_str() {
        "S" => cantidad,
        "D" => (cantidad + 2 * ((cantidad + 4) / 5) + 1) * DIA,
        "W" => cantidad * 7 * DIA,
        "M" => cantidad * 31 * DIA,
        "Y" => cantidad * 366 * DIA,
        _ => return None,
    })
}

/// The magic a system's orders carry unless its `system.json` overrides it.
pub fn magic_por_defecto(sistema: &str) -> i64 {
    7_700_000 + parse_entero(sistema).unwrap_or(0)
}

/// `parseInt`-style leading integer: trims, then accepts a leading sign and
/// digits, ignoring whatever follows.
fn parse_entero(texto: &str) -> Option<i64> {
    let texto = texto.trim();
    let (signo, resto) = match texto.as_bytes().first() {
        Some(b'-') => (-1i64, &texto[1..]),
        Some(b'+') => (1, &texto[1..]),
        _ => (1, texto),
    };
    let digitos: String = resto.chars().take_while(char::is_ascii_digit).collect();
    if digitos.is_empty() {
        return None;
    }
    digitos.parse::<i64>().ok().map(|valor| signo * valor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn futures_roots_map_to_their_markets() {
        let (multiplier, simbolos) = futuro("MNQ").unwrap();
        assert_eq!(multiplier, 2.0);
        assert_eq!(simbolos[0], "US100");
        assert_eq!(futuro("MGC").unwrap().0, 10.0);
        assert_eq!(futuro("MCL").unwrap().1[0], "USOIL");
        assert!(futuro("USDJPY").is_none());
    }

    #[test]
    fn six_letter_roots_are_forex_but_futures_are_not() {
        assert!(es_forex("USDJPY"));
        assert!(es_forex("EURusd"));
        assert!(!es_forex("MNQ"));
        assert!(!es_forex("US100"));
        assert!(!es_forex("USDJP"));
    }

    #[test]
    fn ib_durations() {
        assert_eq!(duracion_segundos("1 M"), Some(31 * 86_400));
        assert_eq!(duracion_segundos("1 D"), Some(4 * 86_400));
        assert!(duracion_segundos("1 D").unwrap() >= 3 * 86_400);
        assert_eq!(duracion_segundos("2 W"), Some(14 * 86_400));
        assert_eq!(duracion_segundos("1 S"), Some(1));
        assert_eq!(duracion_segundos("x"), None);
        assert_eq!(duracion_segundos("1"), None);
        assert_eq!(duracion_segundos("1.5 D"), None);
    }

    #[test]
    fn con_ids_are_stable_and_distinct() {
        assert_eq!(con_id_de("MNQ"), con_id_de("MNQ"));
        assert_ne!(con_id_de("MNQ"), con_id_de("MGC"));
        assert_eq!(con_id_de("MNQ"), 100_000 + (con_id_de("MNQ") - 100_000));
        assert!(con_id_de("MNQ") >= 100_000);
    }

    #[test]
    fn default_magic_numbers_follow_the_system_id() {
        assert_eq!(magic_por_defecto("07"), 7_700_007);
        assert_eq!(magic_por_defecto("12"), 7_700_012);
        assert_eq!(magic_por_defecto("no-number"), 7_700_000);
    }

    #[test]
    fn bar_sizes_match_the_mt5_timeframes() {
        assert_eq!(marco("1 min"), Some("M1"));
        assert_eq!(marco("1 Hour"), Some("H1"));
        assert_eq!(marco("60 mins"), Some("H1"));
        assert_eq!(marco("1 day"), Some("D1"));
        assert_eq!(marco("5 mins"), Some("M5"));
        assert_eq!(marco("7 mins"), None);
    }
}
