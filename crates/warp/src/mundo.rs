//! Everything the ring does outside its own files, behind one trait.
//!
//! The ladder shells out to `warp-cli`, `curl`, `net`, `ipconfig`, `tasklist`
//! and PowerShell, sleeps between polls, reads the clock and probes PIDs. Tests
//! inject a scripted [`Mundo`] so the ladder runs whole without a network, a
//! service or a `warp-cli`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::process::Command;

use crate::shell::{Salida, ejecutar};

/// The outside world as the ring sees it.
#[async_trait]
pub trait Mundo: Send + Sync {
    /// One console command, killed after `espera`.
    async fn ejecutar(&self, programa: &str, args: &[String], espera: Option<Duration>) -> Salida;
    /// Waits between polls (the fake one only records it).
    async fn dormir(&self, espera: Duration);
    /// Unix milliseconds, the history's stamp.
    fn ahora_ms(&self) -> i64;
    /// `YYYY-MM-DD` in UTC: the day the `/64` memory budgets for.
    fn hoy_utc(&self) -> String;
    /// Whether a PID from the lock file is still running.
    fn pid_vivo(&self, pid: u32) -> bool;
    fn pid_propio(&self) -> u32;
    /// Two decimal digits for the session epoch (0..32767 twice, like the TS).
    fn digito(&self) -> u32;
    /// Whether this machine runs the ring at all (`win32` in the TS).
    fn es_windows(&self) -> bool;
    /// Spawns the hidden session monitor without waiting for it.
    async fn lanzar_oculto(&self, programa: &str, args: &[String]);
}

/// The machine's own processes and clock.
#[derive(Debug, Default)]
pub struct Sistema;

#[async_trait]
impl Mundo for Sistema {
    async fn ejecutar(&self, programa: &str, args: &[String], espera: Option<Duration>) -> Salida {
        ejecutar(programa, args, espera).await
    }

    async fn dormir(&self, espera: Duration) {
        tokio::time::sleep(espera).await;
    }

    fn ahora_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|edad| edad.as_millis() as i64)
            .unwrap_or(0)
    }

    fn hoy_utc(&self) -> String {
        chrono::Utc::now().format("%Y-%m-%d").to_string()
    }

    fn pid_vivo(&self, pid: u32) -> bool {
        let mut command = std::process::Command::new("tasklist");
        command.args(["/FI", &format!("PID eq {pid}"), "/NH"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        match command.output() {
            Ok(salida) => String::from_utf8_lossy(&salida.stdout).contains(&pid.to_string()),
            Err(_) => false,
        }
    }

    fn pid_propio(&self) -> u32 {
        std::process::id()
    }

    fn digito(&self) -> u32 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|edad| edad.subsec_nanos())
            .unwrap_or(0)
            % 32_768
    }

    fn es_windows(&self) -> bool {
        cfg!(windows)
    }

    async fn lanzar_oculto(&self, programa: &str, args: &[String]) {
        let mut command = Command::new(programa);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            // Detached and without a console: the monitor outlives this process.
            command.creation_flags(0x0000_0008 | 0x0800_0000);
        }
        if let Ok(mut hijo) = command.spawn() {
            tokio::spawn(async move {
                let _ = hijo.wait().await;
            });
        }
    }
}
