//! The shared files the ring, the desktop bat and its monitor use.
//!
//! Every path is derived from the process environment exactly like `ghost.ts`
//! derives them, so the button and the bat take the same lock, count the same
//! `/64` memory and land in the same history. [`Rutas::en`] keeps a whole set
//! inside one directory for tests: no test touches the user's profile.

use std::path::{Path, PathBuf};

/// The default pinned AAAA for `opencode.ai`.
pub const PIN_DEFECTO: &str = "2606:4700:78::90:0:141";

/// The bat's literal fallback order after the resolve and the verified pin.
pub const PIN_LITERALES: [&str; 3] = [
    "2606:4700:78::90:0:141",
    "2606:4700:78::90:0:140",
    "2606:4700:78::90:0:142",
];

/// The process images a ghost session serves.
///
/// The first four are the bat's own list, kept verbatim so a machine that still
/// runs the old launcher keeps its session. The last two are Aurea's binaries:
/// the desktop app and its headless engine share the session, so the pin stays
/// up while either of them is alive.
pub const APP_IMAGENES: [&str; 6] = [
    "opencode.exe",
    "opencode dev.exe",
    "openbot.exe",
    "openbot dev.exe",
    "zeron.exe",
    "zeron-engine.exe",
];

#[derive(Debug, Clone)]
pub struct Rutas {
    /// `%ProgramFiles%\Cloudflare\Cloudflare WARP\warp-cli.exe`.
    pub warp_cli: PathBuf,
    /// `%SystemRoot%\System32\drivers\etc\hosts`.
    pub hosts: PathBuf,
    /// `%USERPROFILE%\opencode-warp-ips.log`: the day's `/64` memory.
    pub memoria: PathBuf,
    /// `%USERPROFILE%\ghost-eventos.log`.
    pub eventos: PathBuf,
    /// `%USERPROFILE%\ghost-dia.txt`.
    pub dia: PathBuf,
    /// `%USERPROFILE%\ghost-historial.jsonl`.
    pub historial: PathBuf,
    /// `%USERPROFILE%\ghost-pin.log`.
    pub pin_log: PathBuf,
    /// `%TEMP%\ghost.lock`.
    pub cerrojo: PathBuf,
    /// `%TEMP%\ghost.epoch`, the session monitor's baton.
    pub epoch: PathBuf,
}

impl Rutas {
    /// The machine's own paths, as the TS constants resolve them.
    pub fn del_entorno() -> Self {
        let programas = std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        let sistema = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let casa = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_default();
        let temporal = std::env::temp_dir();
        Self {
            warp_cli: programas
                .join("Cloudflare")
                .join("Cloudflare WARP")
                .join("warp-cli.exe"),
            hosts: sistema
                .join("System32")
                .join("drivers")
                .join("etc")
                .join("hosts"),
            memoria: casa.join("opencode-warp-ips.log"),
            eventos: casa.join("ghost-eventos.log"),
            dia: casa.join("ghost-dia.txt"),
            historial: casa.join("ghost-historial.jsonl"),
            pin_log: casa.join("ghost-pin.log"),
            cerrojo: temporal.join("ghost.lock"),
            epoch: temporal.join("ghost.epoch"),
        }
    }

    /// Every file inside `base`, for tests and for an isolated run.
    pub fn en(base: impl AsRef<Path>) -> Self {
        let base = base.as_ref();
        Self {
            warp_cli: base.join("warp-cli.exe"),
            hosts: base.join("hosts"),
            memoria: base.join("opencode-warp-ips.log"),
            eventos: base.join("ghost-eventos.log"),
            dia: base.join("ghost-dia.txt"),
            historial: base.join("ghost-historial.jsonl"),
            pin_log: base.join("ghost-pin.log"),
            cerrojo: base.join("ghost.lock"),
            epoch: base.join("ghost.epoch"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_set_keeps_every_file_inside_its_base() {
        let rutas = Rutas::en(r"C:\tmp\warp-test");
        for ruta in [
            &rutas.warp_cli,
            &rutas.hosts,
            &rutas.memoria,
            &rutas.eventos,
            &rutas.dia,
            &rutas.historial,
            &rutas.pin_log,
            &rutas.cerrojo,
            &rutas.epoch,
        ] {
            assert!(ruta.starts_with(r"C:\tmp\warp-test"), "{ruta:?}");
        }
    }

    #[test]
    fn the_defaults_are_the_bat_s_own_names() {
        let rutas = Rutas::en("base");
        assert_eq!(rutas.memoria.file_name().unwrap(), "opencode-warp-ips.log");
        assert_eq!(rutas.dia.file_name().unwrap(), "ghost-dia.txt");
        assert_eq!(rutas.historial.file_name().unwrap(), "ghost-historial.jsonl");
        assert_eq!(rutas.cerrojo.file_name().unwrap(), "ghost.lock");
        assert_eq!(rutas.epoch.file_name().unwrap(), "ghost.epoch");
    }
}
