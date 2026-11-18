//! Spawns the Python bridge and speaks its JSON-lines protocol.
//!
//! One `PuenteMt5` per MT5 link: the child prints `bot`/`cuenta`/`estado`
//! lines as it reads the terminal, and answers each command sent on stdin with
//! one `resultado` line carrying the same `id`. Commands are awaited with the
//! same 30 s cap as the TS side (`comandoMT5`), and a bridge that dies fails
//! everything still waiting.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex, RwLock};

use super::libro::{CuentaMt5, LibroMt5, ResultadoMt5};

/// How long a command may wait for its `resultado`.
pub const ESPERA_COMANDO: Duration = Duration::from_secs(30);

type Pendientes = Arc<Mutex<HashMap<u64, oneshot::Sender<ResultadoMt5>>>>;

/// A running MT5 bridge process.
pub struct PuenteMt5 {
    hijo: Mutex<Child>,
    entrada: Mutex<ChildStdin>,
    libro: Arc<RwLock<Option<LibroMt5>>>,
    cuenta: Arc<RwLock<Option<CuentaMt5>>>,
    /// Why the bridge is not attached, when it is not.
    motivo: Arc<RwLock<Option<String>>>,
    pendientes: Pendientes,
    siguiente: AtomicU64,
}

impl PuenteMt5 {
    /// Starts `python <script> --login <login> [--server <server>] --desfase <file>`.
    ///
    /// `variables` are extra environment entries for the child (the fake
    /// `MetaTrader5` test package is found through `PYTHONPATH`).
    ///
    /// The bridge never launches the terminal itself; if it is closed, the
    /// child reports `estado` and keeps retrying.
    pub fn arrancar(
        ejecutable: &Path,
        script: &Path,
        login: &str,
        servidor: Option<&str>,
        desfase: &Path,
        variables: &[(String, String)],
    ) -> io::Result<Self> {
        let mut command = Command::new(ejecutable);
        command.arg(script).arg("--login").arg(login);
        if let Some(servidor) = servidor {
            command.arg("--server").arg(servidor);
        }
        for (clave, valor) in variables {
            command.env(clave, valor);
        }
        command
            .arg("--desfase")
            .arg(desfase)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            // No console window for the sidecar.
            command.creation_flags(0x0800_0000);
        }
        let mut hijo = command.spawn()?;
        let entrada = hijo.stdin.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "el puente no abrió stdin")
        })?;
        let salida = hijo.stdout.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "el puente no abrió stdout")
        })?;
        if let Some(error) = hijo.stderr.take() {
            tokio::spawn(async move {
                let mut lineas = BufReader::new(error).lines();
                while let Ok(Some(linea)) = lineas.next_line().await {
                    tracing::warn!(target: "mt5", "puente: {linea}");
                }
            });
        }

        let pendientes: Pendientes = Arc::new(Mutex::new(HashMap::new()));
        let libro = Arc::new(RwLock::new(None));
        let cuenta = Arc::new(RwLock::new(None));
        let motivo = Arc::new(RwLock::new(None));

        {
            let pendientes = Arc::clone(&pendientes);
            let libro = Arc::clone(&libro);
            let cuenta = Arc::clone(&cuenta);
            let motivo = Arc::clone(&motivo);
            tokio::spawn(async move {
                leer(salida, libro, cuenta, motivo, pendientes).await;
            });
        }

        Ok(Self {
            hijo: Mutex::new(hijo),
            entrada: Mutex::new(entrada),
            libro,
            cuenta,
            motivo,
            pendientes,
            siguiente: AtomicU64::new(1),
        })
    }

    /// The link's last book, or `None` while the bridge reads nothing.
    pub async fn libro(&self) -> Option<LibroMt5> {
        self.libro.read().await.clone()
    }

    /// The last `cuenta` line.
    pub async fn cuenta(&self) -> Option<CuentaMt5> {
        self.cuenta.read().await.clone()
    }

    /// Why the bridge is not attached, when it is not.
    pub async fn motivo(&self) -> Option<String> {
        self.motivo.read().await.clone()
    }

    /// Whether the child is still running.
    pub async fn vivo(&self) -> bool {
        let mut hijo = self.hijo.lock().await;
        matches!(hijo.try_wait(), Ok(None))
    }

    /// One bridge command (`SYMBOL`, `DEAL`, `SLTP`, …).
    pub async fn comando(&self, verbo: &str, arg: &str) -> ResultadoMt5 {
        let id = self.siguiente.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pendientes.lock().await.insert(id, tx);
        let linea = serde_json::json!({"id": id.to_string(), "verb": verbo, "arg": arg});
        let mut texto = match serde_json::to_vec(&linea) {
            Ok(texto) => texto,
            Err(error) => {
                self.pendientes.lock().await.remove(&id);
                return ResultadoMt5::fallo(-1, format!("comando ilegible: {error}"));
            }
        };
        texto.push(b'\n');
        let escritura = async {
            let mut entrada = self.entrada.lock().await;
            entrada.write_all(&texto).await?;
            entrada.flush().await
        };
        if let Err(error) = escritura.await {
            self.pendientes.lock().await.remove(&id);
            return ResultadoMt5::fallo(-1, format!("no se pudo escribir al puente: {error}"));
        }
        match tokio::time::timeout(ESPERA_COMANDO, rx).await {
            Ok(Ok(resultado)) => resultado,
            Ok(Err(_)) => ResultadoMt5::fallo(-1, "el puente de MetaTrader 5 terminó"),
            Err(_) => {
                self.pendientes.lock().await.remove(&id);
                ResultadoMt5::fallo(-2, "el puente de MetaTrader 5 no respondió en 30 s")
            }
        }
    }

    /// Stops the bridge for good.
    pub async fn parar(&self) {
        let mut hijo = self.hijo.lock().await;
        let _ = hijo.start_kill();
        let _ = hijo.wait().await;
    }
}

impl Drop for PuenteMt5 {
    fn drop(&mut self) {
        if let Ok(mut hijo) = self.hijo.try_lock() {
            let _ = hijo.start_kill();
        }
    }
}

async fn leer(
    salida: tokio::process::ChildStdout,
    libro: Arc<RwLock<Option<LibroMt5>>>,
    cuenta: Arc<RwLock<Option<CuentaMt5>>>,
    motivo: Arc<RwLock<Option<String>>>,
    pendientes: Pendientes,
) {
    let mut lineas = BufReader::new(salida).lines();
    while let Ok(Some(linea)) = lineas.next_line().await {
        let Ok(valor) = serde_json::from_str::<serde_json::Value>(&linea) else {
            continue;
        };
        match valor.get("type").and_then(serde_json::Value::as_str) {
            Some("resultado") => {
                let Ok(resultado) = serde_json::from_value::<ResultadoMt5>(valor) else {
                    continue;
                };
                let Ok(id) = resultado.id.parse::<u64>() else {
                    continue;
                };
                if let Some(tx) = pendientes.lock().await.remove(&id) {
                    let _ = tx.send(resultado);
                }
            }
            Some("estado") => {
                let texto = valor
                    .get("motivo")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("el puente no está enganchado")
                    .to_string();
                *motivo.write().await = Some(texto);
            }
            Some("cuenta") => {
                if let Ok(leida) = serde_json::from_value::<CuentaMt5>(valor) {
                    *cuenta.write().await = Some(leida);
                    *motivo.write().await = None;
                }
            }
            Some("bot") => {
                if let Ok(leido) = serde_json::from_value::<LibroMt5>(valor) {
                    *libro.write().await = Some(leido);
                    *motivo.write().await = None;
                }
            }
            _ => {}
        }
    }
    // The child ended: nothing else will answer.
    let mut pendientes = pendientes.lock().await;
    for (_, tx) in pendientes.drain() {
        let _ = tx.send(ResultadoMt5::fallo(-1, "el puente de MetaTrader 5 terminó"));
    }
}
