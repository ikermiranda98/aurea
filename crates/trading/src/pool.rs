//! Static pool scan and per-system dossier, ported from Openbot's `trading.ts`.
//!
//! [`scan_dirs`] finds the `NN_name` folders in a pool; [`dossier`] gathers one
//! folder's own facts from `config.py`, `reglas.json` and `system.json`. No async
//! runtime, network or database is involved: the engine layer builds its
//! snapshot on top of these pure filesystem reads.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// One `NN_name` folder's facts, as stated by the files inside it.
#[derive(Debug, Clone, PartialEq)]
pub struct Dossier {
    /// Folder name as found in the pool, e.g. `"07_oro_nube_ichimoku"`.
    pub dir_name: String,
    /// The folder's two-digit prefix, e.g. `"07"`.
    pub id: String,
    /// Display symbol from the fixed folder→metadata map; `None` for unknown folders.
    pub symbol: Option<String>,
    /// Display timeframe from the fixed folder→metadata map.
    pub timeframe: Option<String>,
    /// `CLIENT_ID` from `config.py`; absent when missing or zero.
    pub client_id: Option<i64>,
    /// `HOST` from `config.py`.
    pub host: Option<String>,
    /// `PUERTO` from `config.py`.
    pub port: Option<u16>,
    /// `HORA_CIERRE` from `config.py`, as `"HH:MM"` or `"HH:MM:SS"`.
    pub close_hour: Option<String>,
    /// The one-paragraph `idea` from `reglas.json`.
    pub description: Option<String>,
    /// String-valued `reglas` entries from `reglas.json`.
    pub rules: Option<BTreeMap<String, String>>,
    /// Capital assigned to the system, from `system.json`; `None` when undeclared.
    pub capital: Option<f64>,
    /// MT5 destination from `system.json` when the folder asks for MT5.
    pub mt5: Option<Mt5Target>,
}

/// An MT5 destination read from a system's `system.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mt5Target {
    /// MetaTrader 5 account login (positive).
    pub login: i64,
    /// Server name the terminal logs into (non-empty).
    pub server: String,
    /// Instrument the bot trades (non-empty).
    pub symbol: String,
    /// Magic number; the folder's default when `system.json` carries no positive one.
    pub magic: i64,
}

/// The 15 fixed folder→metadata pairs from the TS `META` map.
fn meta(dir_name: &str) -> Option<(&'static str, &'static str)> {
    match dir_name {
        "01_nasdaq_apertura_alcista" => Some(("NQ", "5m")),
        "02_nasdaq_ruptura_media_hora" => Some(("NQ", "30m")),
        "03_nasdaq_impulso_con_tendencia" => Some(("NQ", "15m")),
        "04_oro_rebote_del_viernes" => Some(("GC", "30m")),
        "05_oro_fuerza_de_madrugada" => Some(("GC", "15m")),
        "06_nasdaq_pullback" => Some(("NQ", "1h")),
        "07_oro_nube_ichimoku" => Some(("GC", "1h")),
        "08_russell_estocastico_h2" => Some(("RTY", "2h")),
        "09_russell_cci_tendencia" => Some(("RTY", "1h")),
        "10_russell_estocastico_h4" => Some(("RTY", "4h")),
        "11_usdjpy_ruptura_bollinger" => Some(("USDJPY", "1h")),
        "12_usdjpy_ruptura_keltner" => Some(("USDJPY", "1h")),
        "13_usdjpy_ruptura_donchian" => Some(("USDJPY", "1h")),
        "14_usdjpy_cruce_ema_10_40" => Some(("USDJPY", "1h")),
        "15_usdjpy_precio_ema30_h3" => Some(("USDJPY", "3h")),
        _ => None,
    }
}

/// Returns the pool's directory names matching `NN_name`, in lexicographic order.
/// An unreadable directory yields an empty list, never an error.
pub fn scan_dirs(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_pool_name(&name) {
            names.push(name);
        }
    }
    names.sort();
    names
}

fn is_pool_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[2] == b'_'
}

/// The default MT5 magic for a system id: `7_700_000 + NN` (non-numeric ids add 0).
pub fn default_magic(system_id: &str) -> i64 {
    7_700_000 + system_id.parse::<i64>().unwrap_or(0)
}

/// Builds one system's dossier from its own files; missing or malformed files
/// leave only their own facts unset.
pub fn dossier(pool: &Path, dir_name: &str) -> Dossier {
    let folder = pool.join(dir_name);
    let id: String = dir_name.chars().take(2).collect();
    let config = fs::read_to_string(folder.join("config.py")).unwrap_or_default();

    let client_id = config_line(&config, "CLIENT_ID")
        .and_then(leading_digits)
        .and_then(|digits| digits.parse::<i64>().ok())
        .filter(|value| *value != 0);
    let host = config_line(&config, "HOST").and_then(quoted_value);
    let port = config_line(&config, "PUERTO")
        .and_then(leading_digits)
        .and_then(|digits| digits.parse::<u16>().ok())
        .filter(|value| *value != 0);
    let close_hour = config.lines().find_map(close_hour_from_line);

    let (description, rules) = read_rules(&folder);
    let (symbol, timeframe) = match meta(dir_name) {
        Some((symbol, timeframe)) => (Some(symbol.to_owned()), Some(timeframe.to_owned())),
        None => (None, None),
    };
    let system = read_system_json(&folder);
    let capital = system.as_ref().and_then(capital_from);
    let mt5 = read_mt5(system.as_ref(), &id);

    Dossier {
        dir_name: dir_name.to_owned(),
        id,
        symbol,
        timeframe,
        client_id,
        host,
        port,
        close_hour,
        description,
        rules,
        capital,
        mt5,
    }
}

/// Value following `KEY = ` at the start of a line, without the leading `\s*`
/// that only `HORA_CIERRE`'s expression has.
fn config_line<'a>(config: &'a str, key: &str) -> Option<&'a str> {
    config
        .lines()
        .find_map(|line| value_after_assignment(line, key))
}

fn value_after_assignment<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?;
    let rest = rest.trim_start_matches(char::is_whitespace);
    let rest = rest.strip_prefix('=')?;
    Some(rest.trim_start_matches(char::is_whitespace))
}

fn leading_digits(rest: &str) -> Option<&str> {
    let count = rest.bytes().take_while(|byte| byte.is_ascii_digit()).count();
    (count > 0).then(|| &rest[..count])
}

fn quoted_value(rest: &str) -> Option<String> {
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let value = &rest[..end];
    (!value.is_empty()).then(|| value.to_owned())
}

/// Parses `^\s*HORA_CIERRE\s*=\s*\((\d+),\s*(\d+)(?:,\s*(\d+))?\)` on one line.
fn close_hour_from_line(line: &str) -> Option<String> {
    let line = line.trim_start_matches(char::is_whitespace);
    let rest = value_after_assignment(line, "HORA_CIERRE")?;
    let rest = rest.strip_prefix('(')?;
    let (hour, rest) = take_digits(rest)?;
    let rest = rest.strip_prefix(',')?;
    let rest = rest.trim_start_matches(char::is_whitespace);
    let (minute, rest) = take_digits(rest)?;
    let (seconds, rest) = match rest.strip_prefix(',') {
        Some(after_comma) => {
            let after_comma = after_comma.trim_start_matches(char::is_whitespace);
            let (seconds, tail) = take_digits(after_comma)?;
            (Some(seconds), tail)
        }
        None => (None, rest),
    };
    let _ = rest.strip_prefix(')')?;
    let hour = pad2(hour);
    let minute = pad2(minute);
    match seconds {
        Some(seconds) => Some(format!("{hour}:{minute}:{}", pad2(seconds))),
        None => Some(format!("{hour}:{minute}")),
    }
}

fn take_digits(rest: &str) -> Option<(&str, &str)> {
    let count = rest.bytes().take_while(|byte| byte.is_ascii_digit()).count();
    (count > 0).then(|| rest.split_at(count))
}

fn pad2(digits: &str) -> String {
    if digits.len() < 2 {
        format!("0{digits}")
    } else {
        digits.to_owned()
    }
}

/// Reads `idea` and the string-valued `reglas` entries; malformed JSON nulls both.
fn read_rules(folder: &Path) -> (Option<String>, Option<BTreeMap<String, String>>) {
    let Ok(text) = fs::read_to_string(folder.join("reglas.json")) else {
        return (None, None);
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return (None, None);
    };
    let description = json
        .get("idea")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let rules = json.get("reglas").and_then(serde_json::Value::as_object).map(|entries| {
        entries
            .iter()
            .filter_map(|(key, value)| value.as_str().map(|text| (key.clone(), text.to_owned())))
            .collect::<BTreeMap<String, String>>()
    });
    (description, rules)
}

/// Reads `system.json`, tolerating a UTF-8 BOM; malformed JSON reads as absent.
fn read_system_json(folder: &Path) -> Option<serde_json::Value> {
    let bytes = fs::read(folder.join("system.json")).ok()?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    let text = String::from_utf8(bytes.to_vec()).ok()?;
    serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')).ok()
}

/// `capital` from `system.json` as a positive amount; anything else is unset.
fn capital_from(json: &serde_json::Value) -> Option<f64> {
    let amount = match json.get("capital")? {
        serde_json::Value::Number(number) => number.as_f64()?,
        serde_json::Value::String(text) => text.trim().replace(',', ".").parse::<f64>().ok()?,
        _ => return None,
    };
    (amount.is_finite() && amount > 0.0).then_some(amount)
}

/// Reads an MT5 destination: the folder must ask for `mt5` and carry all three
/// required facts; a missing magic falls back to the folder's default.
fn read_mt5(json: Option<&serde_json::Value>, id: &str) -> Option<Mt5Target> {
    let json = json?;

    let asks_mt5 = json
        .get("broker")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|broker| broker.eq_ignore_ascii_case("mt5"));
    if !asks_mt5 {
        return None;
    }

    let login = positive_int(json.get("login"))?;
    let server = non_empty_str(json.get("server"))?.to_owned();
    let symbol = non_empty_str(json.get("symbol"))?.to_owned();
    let magic = positive_int(json.get("magic")).unwrap_or_else(|| default_magic(id));
    Some(Mt5Target {
        login,
        server,
        symbol,
        magic,
    })
}

fn positive_int(value: Option<&serde_json::Value>) -> Option<i64> {
    let number = match value? {
        serde_json::Value::Number(number) => number.as_i64()?,
        serde_json::Value::String(text) => text.parse::<i64>().ok()?,
        _ => return None,
    };
    (number > 0).then_some(number)
}

fn non_empty_str(value: Option<&serde_json::Value>) -> Option<&str> {
    value?.as_str().filter(|text| !text.is_empty())
}

/// Byte offset of the first line starting with `"<today_prefix> "`, or the
/// file's size when no such line exists (0 when the file is absent).
pub fn today_offset(path: &Path, today_prefix: &str) -> u64 {
    let Ok(bytes) = fs::read(path) else {
        return file_tail_size(path);
    };
    let needle = format!("{today_prefix} ");
    let needle = needle.as_bytes();
    let mut line_start = 0usize;
    for line in bytes.split(|byte| *byte == b'\n') {
        if line.starts_with(needle) {
            return line_start as u64;
        }
        line_start += line.len() + 1;
    }
    bytes.len() as u64
}

/// File size in bytes, or 0 when the file does not exist (the log-tail offset).
pub fn file_tail_size(path: &Path) -> u64 {
    fs::metadata(path).map(|metadata| metadata.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    /// A uniquely named temporary pool, removed on drop even if a test fails.
    struct TempPool(PathBuf);

    impl TempPool {
        fn new(tag: &str) -> Self {
            let sequence = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "zeron_pool_{tag}_{}_{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn system(&self, name: &str) -> PathBuf {
            let folder = self.0.join(name);
            fs::create_dir_all(&folder).unwrap();
            folder
        }
    }

    impl Drop for TempPool {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(folder: &Path, name: &str, contents: &str) {
        fs::write(folder.join(name), contents).unwrap();
    }

    #[test]
    fn scan_dirs_filters_and_sorts() {
        let pool = TempPool::new("scan");
        pool.system("07_oro_nube_ichimoku");
        pool.system("02_nasdaq_apertura_alcista");
        pool.system("10_russell_estocastico_h4");
        pool.system("xx_no_dos_digitos");
        pool.system("3_corto");
        pool.system("11sin_guion");
        fs::write(pool.path().join("03_archivo.txt"), "").unwrap();
        assert_eq!(
            scan_dirs(pool.path()),
            vec![
                "02_nasdaq_apertura_alcista",
                "07_oro_nube_ichimoku",
                "10_russell_estocastico_h4",
            ]
        );
        assert!(scan_dirs(&pool.path().join("no_existe")).is_empty());
    }

    #[test]
    fn dossier_full_config() {
        let pool = TempPool::new("completo");
        let folder = pool.system("07_oro_nube_ichimoku");
        write(
            &folder,
            "config.py",
            "CLIENT_ID = 4210\nHOST = \"127.0.0.1\"\nPUERTO = 7497\nHORA_CIERRE = (16, 30, 5)\n",
        );
        write(
            &folder,
            "reglas.json",
            r#"{"idea":"Idea del oro.","reglas":{"entrada":"rompe","salida":"cierra"}}"#,
        );
        write(
            &folder,
            "system.json",
            r#"{"broker":"mt5","login":5058203356,"server":"AureaDemo","symbol":"XAUUSD.cash","magic":7700007,"capital":12500}"#,
        );

        let def = dossier(pool.path(), "07_oro_nube_ichimoku");
        assert_eq!(def.id, "07");
        assert_eq!(def.symbol.as_deref(), Some("GC"));
        assert_eq!(def.timeframe.as_deref(), Some("1h"));
        assert_eq!(def.client_id, Some(4210));
        assert_eq!(def.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(def.port, Some(7497));
        assert_eq!(def.close_hour.as_deref(), Some("16:30:05"));
        assert_eq!(def.description.as_deref(), Some("Idea del oro."));
        assert_eq!(def.capital, Some(12_500.0));
        let rules = def.rules.expect("debe leer las reglas");
        assert_eq!(rules.get("entrada").map(String::as_str), Some("rompe"));
        assert_eq!(rules.get("salida").map(String::as_str), Some("cierra"));
        let mt5 = def.mt5.expect("debe leer el destino mt5");
        assert_eq!(mt5.login, 5_058_203_356);
        assert_eq!(mt5.server, "AureaDemo");
        assert_eq!(mt5.symbol, "XAUUSD.cash");
        assert_eq!(mt5.magic, 7_700_007);
    }

    #[test]
    fn dossier_partial_config() {
        let pool = TempPool::new("parcial");
        let folder = pool.system("99_desconocido");
        write(&folder, "config.py", "CLIENT_ID=7\nHORA_CIERRE=(9,5)\n");

        let def = dossier(pool.path(), "99_desconocido");
        assert_eq!(def.dir_name, "99_desconocido");
        assert_eq!(def.id, "99");
        assert_eq!(def.client_id, Some(7));
        assert_eq!(def.host, None);
        assert_eq!(def.port, None);
        assert_eq!(def.close_hour.as_deref(), Some("09:05"));
        assert!(def.symbol.is_none());
        assert!(def.timeframe.is_none());
        assert!(def.description.is_none());
        assert!(def.rules.is_none());
        assert!(def.mt5.is_none());
    }

    #[test]
    fn close_hour_two_three_and_absent() {
        let pool = TempPool::new("hora");
        let two = pool.system("01_dos");
        write(&two, "config.py", "  HORA_CIERRE = (9, 5)\n");
        let three = pool.system("02_tres");
        write(&three, "config.py", "HORA_CIERRE=(16,30,7)\n");
        let absent = pool.system("03_ausente");
        write(&absent, "config.py", "HOST=\"localhost\"\n");

        assert_eq!(
            dossier(pool.path(), "01_dos").close_hour.as_deref(),
            Some("09:05")
        );
        assert_eq!(
            dossier(pool.path(), "02_tres").close_hour.as_deref(),
            Some("16:30:07")
        );
        assert!(dossier(pool.path(), "03_ausente").close_hour.is_none());
    }

    #[test]
    fn reglas_filters_non_string_values() {
        let pool = TempPool::new("reglas");
        let good = pool.system("01_buenas");
        write(
            &good,
            "reglas.json",
            r#"{"idea":"idea","reglas":{"a":"uno","b":7,"c":null,"d":"dos","e":true}}"#,
        );
        let def = dossier(pool.path(), "01_buenas");
        let rules = def.rules.expect("debe leer las reglas");
        assert_eq!(rules.len(), 2);
        assert_eq!(rules.get("a").map(String::as_str), Some("uno"));
        assert_eq!(rules.get("d").map(String::as_str), Some("dos"));

        let broken = pool.system("02_malas");
        write(&broken, "reglas.json", "{ json invalido");
        let def = dossier(pool.path(), "02_malas");
        assert!(def.description.is_none());
        assert!(def.rules.is_none());
    }

    #[test]
    fn system_json_bom_broker_and_magic() {
        let pool = TempPool::new("mt5");
        let with_bom = pool.system("07_bom");
        write(
            &with_bom,
            "system.json",
            "\u{feff}{\"broker\":\"MT5\",\"login\":12345,\"server\":\"Demo\",\"symbol\":\"US100.cash\"}",
        );
        let def = dossier(pool.path(), "07_bom");
        let mt5 = def.mt5.expect("debe leer el destino con BOM");
        assert_eq!(mt5.login, 12_345);
        assert_eq!(mt5.server, "Demo");
        assert_eq!(mt5.symbol, "US100.cash");
        assert_eq!(mt5.magic, default_magic("07"));

        let other_broker = pool.system("08_ibkr");
        write(
            &other_broker,
            "system.json",
            r#"{"broker":"ibkr","login":99,"server":"Demo","symbol":"NQ"}"#,
        );
        assert!(dossier(pool.path(), "08_ibkr").mt5.is_none());

        let incomplete = pool.system("09_incompleto");
        write(
            &incomplete,
            "system.json",
            r#"{"broker":"mt5","login":0,"server":"","symbol":"NQ"}"#,
        );
        assert!(dossier(pool.path(), "09_incompleto").mt5.is_none());
    }

    #[test]
    fn capital_reads_numbers_and_text_and_ignores_the_rest() {
        let pool = TempPool::new("capital");
        let numeric = pool.system("01_numero");
        write(&numeric, "system.json", r#"{"capital":10000}"#);
        assert_eq!(dossier(pool.path(), "01_numero").capital, Some(10_000.0));

        let text = pool.system("02_texto");
        write(&text, "system.json", r#"{"capital":"12500,50"}"#);
        assert_eq!(dossier(pool.path(), "02_texto").capital, Some(12_500.50));

        let zero = pool.system("03_cero");
        write(&zero, "system.json", r#"{"capital":0}"#);
        assert!(dossier(pool.path(), "03_cero").capital.is_none());

        let negative = pool.system("04_negativo");
        write(&negative, "system.json", r#"{"capital":-50}"#);
        assert!(dossier(pool.path(), "04_negativo").capital.is_none());

        let broken = pool.system("05_roto");
        write(&broken, "system.json", r#"{"capital":"mucho"}"#);
        assert!(dossier(pool.path(), "05_roto").capital.is_none());

        let absent = pool.system("06_ausente");
        write(&absent, "system.json", r#"{"broker":"ibkr"}"#);
        assert!(dossier(pool.path(), "06_ausente").capital.is_none());
    }

    #[test]
    fn default_magic_parses_numeric_id() {
        assert_eq!(default_magic("07"), 7_700_007);
        assert_eq!(default_magic("abc"), 7_700_000);
    }

    #[test]
    fn today_offset_finds_first_today_line() {
        let pool = TempPool::new("offset_hoy");
        let log = pool.path().join("registro.log");
        let body = "2026-11-16 10:00:00 anterior\n2026-11-17 09:00:00 primera de hoy\n2026-11-17 09:01:00 segunda\n";
        let first_today = "2026-11-16 10:00:00 anterior\n";
        fs::write(&log, body).unwrap();

        assert_eq!(today_offset(&log, "2026-11-17"), first_today.len() as u64);
        assert_eq!(file_tail_size(&log), body.len() as u64);
    }

    #[test]
    fn today_offset_falls_back_to_size_and_missing_file() {
        let pool = TempPool::new("offset_fin");
        let log = pool.path().join("registro.log");
        let body = "2026-11-16 10:00:00 anterior\nnota 2026-11-17 09:00:00 no empieza la linea\n";
        fs::write(&log, body).unwrap();
        assert_eq!(today_offset(&log, "2026-11-17"), body.len() as u64);

        let missing = pool.path().join("no_existe.log");
        assert_eq!(today_offset(&missing, "2026-11-17"), 0);
        assert_eq!(file_tail_size(&missing), 0);
    }
}
