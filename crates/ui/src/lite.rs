//! Aurea Lite: the build for small machines without a GPU (a Windows VPS),
//! where every frame is drawn by the CPU. Same binary as the full app; Lite
//! is switched on by an `aurea-lite` marker file next to the executable (the
//! Lite installer writes it, and in-app updates keep it) or by `AUREA_LITE`.
//!
//! Lite trades looks for CPU: no animations (reduced motion, always), an
//! opaque window (no glass, no backdrop blur) and the desk redrawn at most
//! once per [`DESK_REFRESH`]. The full app keeps all of it.

use std::sync::OnceLock;
use std::time::Duration;

/// The marker file the Lite installer puts beside `zeron.exe`.
pub const MARKER: &str = "aurea-lite";

/// The desk redraws at most this often in Lite, whatever TWS sends.
pub const DESK_REFRESH: Duration = Duration::from_secs(1);

/// Whether this run is Aurea Lite (read once).
pub fn enabled() -> bool {
    static LITE: OnceLock<bool> = OnceLock::new();
    *LITE.get_or_init(|| {
        if let Some(value) = std::env::var_os("AUREA_LITE") {
            return !value.is_empty() && value != "0";
        }
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(MARKER)))
            .is_some_and(|marker| marker.exists())
    })
}

/// The app's name as the window shows it.
pub fn app_name() -> &'static str {
    if enabled() { "Aurea Lite" } else { "Aurea" }
}

/// The theme as Lite draws it: opaque surfaces (no glass, no backdrop blur).
pub fn adjust_theme(mut theme: crate::theme::Theme) -> crate::theme::Theme {
    if enabled() {
        theme.surface_treatment = zeron_theme::SurfaceTreatment::Opaque;
    }
    theme
}
