//! One console command, its exit status and its stdout.
//!
//! `warp-cli` has no timeout of its own, and a hung child would hold the ghost
//! lock for the whole stale window, so every bounded verb kills the process and
//! reports a failed exit (-1) when its cap elapses. `curl` keeps its own
//! `--max-time` and is left unbounded, exactly like the TS `run`.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// A finished (or killed) console command.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Salida {
    /// The process exit status, or `-1` when it could not start or timed out.
    pub status: i32,
    pub stdout: String,
}

impl Salida {
    pub fn bien(stdout: impl Into<String>) -> Self {
        Self {
            status: 0,
            stdout: stdout.into(),
        }
    }

    pub fn malo() -> Self {
        Self {
            status: -1,
            stdout: String::new(),
        }
    }

    /// `line` present as a whole line (the TS `textLines` then `includes`).
    pub fn lineas(&self) -> Vec<&str> {
        self.stdout
            .split(['\r', '\n'])
            .filter(|linea| !linea.is_empty())
            .collect()
    }
}

/// Runs `programa args`, collecting stdout, and kills it after `espera`.
pub async fn ejecutar(programa: &str, args: &[String], espera: Option<Duration>) -> Salida {
    let mut command = Command::new(programa);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        command.creation_flags(0x0800_0000);
    }
    let mut hijo = match command.spawn() {
        Ok(hijo) => hijo,
        Err(_) => return Salida::malo(),
    };
    let Some(stdout) = hijo.stdout.take() else {
        let _ = hijo.start_kill();
        return Salida::malo();
    };
    let lectura = tokio::spawn(async move {
        let mut texto = String::new();
        let mut lineas = BufReader::new(stdout).lines();
        while let Ok(Some(linea)) = lineas.next_line().await {
            texto.push_str(&linea);
            texto.push('\n');
        }
        texto
    });
    let estado = match espera {
        None => hijo.wait().await.ok(),
        Some(espera) => match tokio::time::timeout(espera, hijo.wait()).await {
            Ok(estado) => estado.ok(),
            Err(_) => {
                let _ = hijo.start_kill();
                let _ = hijo.wait().await;
                let _ = lectura.await;
                return Salida::malo();
            }
        },
    };
    let stdout = lectura.await.unwrap_or_default();
    Salida {
        status: estado.and_then(|estado| estado.code()).unwrap_or(-1),
        stdout,
    }
}

/// Splits `clave=valor` out of a `cdn-cgi/trace` body (case-insensitive key).
pub fn trazo(clave: &str, cuerpo: &str) -> Option<String> {
    cuerpo
        .split(['\r', '\n'])
        .find(|linea| {
            linea
                .to_lowercase()
                .starts_with(&format!("{}=", clave.to_lowercase()))
        })
        .map(|linea| linea[clave.len() + 1..].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trace_line_is_found_by_its_key() {
        let cuerpo = "fl=123f\nh=cloudflare.com\nip=2a09:bac5:6de7:31e1\nwarp=on\nts=1\n";
        assert_eq!(trazo("ip", cuerpo).as_deref(), Some("2a09:bac5:6de7:31e1"));
        assert_eq!(trazo("warp", cuerpo).as_deref(), Some("on"));
        assert_eq!(trazo("IP", cuerpo).as_deref(), Some("2a09:bac5:6de7:31e1"));
        assert_eq!(trazo("nope", cuerpo), None);
    }

    #[test]
    fn the_trace_lines_keep_their_order_and_drop_empty_ones() {
        let salida = Salida::bien("a\r\n\r\nb\n");
        assert_eq!(salida.lineas(), vec!["a", "b"]);
        assert_eq!(Salida::malo().status, -1);
        assert!(Salida::malo().lineas().is_empty());
    }
}
