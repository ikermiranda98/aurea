//! This device's platform string and the edge HTTP client (what remained of
//! Zeron Cloud's runner mode, gone from Aurea).


/// `ZERON_DEVICE_PLATFORM` overrides the OS in `Device::platform`.
pub const PLATFORM_ENV: &str = "ZERON_DEVICE_PLATFORM";
/// This engine's `Device::platform`: `ZERON_DEVICE_PLATFORM` when set, else
/// the OS (`macos` / `linux` / `windows`).
pub fn device_platform() -> String {
    platform_from(std::env::var(PLATFORM_ENV).ok())
}

fn platform_from(value: Option<String>) -> String {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| std::env::consts::OS.to_string())
}
