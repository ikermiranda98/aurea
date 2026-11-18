//! The shared rotation lock (`%TEMP%\ghost.lock`).
//!
//! A rotation takes the lock by creating the file exclusively and writing its
//! PID. A second run finds it and retires: a live PID is busy, and so is a
//! dead run's file younger than thirty minutes (one run can spend ten
//! attempts on the ladder). Only a dead, old file is removed and retried
//! once. The guard deletes the file when the run ends.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long a dead run's lock still counts as busy.
pub const LOCK_FRESH: Duration = Duration::from_secs(30 * 60);

/// The lock a run holds, and removes on drop.
#[derive(Debug)]
pub struct Cerrojo {
    ruta: PathBuf,
}

impl Cerrojo {
    /// Creates the lock file exclusively and writes `pid`.
    pub fn crear(ruta: impl Into<PathBuf>, pid: u32) -> Option<Self> {
        let ruta = ruta.into();
        let mut archivo = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&ruta)
            .ok()?;
        archivo.write_all(pid.to_string().as_bytes()).ok()?;
        Some(Self { ruta })
    }

    /// Path of the held lock.
    pub fn ruta(&self) -> &Path {
        &self.ruta
    }
}

impl Drop for Cerrojo {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.ruta);
    }
}

/// Takes the lock, or reports that another run holds it.
///
/// `pid_vivo` answers whether a PID from the file is still running (the real
/// caller probes the OS; tests inject it).
pub fn librar(
    ruta: impl Into<PathBuf>,
    pid: u32,
    pid_vivo: impl Fn(u32) -> bool,
    ahora: SystemTime,
) -> Option<Cerrojo> {
    let ruta = ruta.into();
    if let Some(cerrojo) = Cerrojo::crear(ruta.clone(), pid) {
        return Some(cerrojo);
    }
    let anterior = std::fs::read_to_string(&ruta).ok();
    if let Some(anterior) = anterior {
        let anterior = anterior.trim();
        if let Ok(anterior_pid) = anterior.parse::<u32>()
            && pid_vivo(anterior_pid)
        {
            return None;
        }
        if fresco(&ruta, ahora) {
            return None;
        }
    }
    let _ = std::fs::remove_file(&ruta);
    Cerrojo::crear(ruta, pid)
}

/// Whether the lock's `mtime` is younger than [`LOCK_FRESH`]. An unreadable
/// or future timestamp counts as fresh (busy), like the bat's arithmetic.
fn fresco(ruta: &Path, ahora: SystemTime) -> bool {
    match std::fs::metadata(ruta).and_then(|meta| meta.modified()) {
        Ok(modificado) => ahora
            .duration_since(modificado)
            .map(|edad| edad < LOCK_FRESH)
            .unwrap_or(true),
        Err(_) => true,
    }
}

/// Replaces a lock file's timestamp, so a test can age it (or not).
pub fn envejecer(ruta: impl AsRef<Path>, cuando: SystemTime) -> std::io::Result<()> {
    let archivo = File::options().write(true).open(ruta)?;
    archivo.set_modified(cuando)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn vencido() -> SystemTime {
        SystemTime::now() - LOCK_FRESH - Duration::from_secs(60)
    }

    #[test]
    fn a_free_lock_is_taken_and_removed_on_drop() {
        let dir = tempdir().unwrap();
        let ruta = dir.path().join("ghost.lock");
        let cerrojo = librar(&ruta, std::process::id(), |_| false, SystemTime::now())
            .expect("the lock starts free");
        assert_eq!(std::fs::read_to_string(cerrojo.ruta()).unwrap(), std::process::id().to_string());
        drop(cerrojo);
        assert!(!ruta.exists(), "the guard deletes its file");
    }

    #[test]
    fn a_live_pid_holds_the_lock_even_when_the_file_is_old() {
        let dir = tempdir().unwrap();
        let ruta = dir.path().join("ghost.lock");
        std::fs::write(&ruta, "4242").unwrap();
        envejecer(&ruta, vencido()).unwrap();
        assert!(librar(&ruta, 1, |pid| pid == 4242, SystemTime::now()).is_none());
        assert_eq!(std::fs::read_to_string(&ruta).unwrap(), "4242");
    }

    #[test]
    fn a_dead_run_younger_than_thirty_minutes_still_holds_the_lock() {
        let dir = tempdir().unwrap();
        let ruta = dir.path().join("ghost.lock");
        std::fs::write(&ruta, "4242").unwrap();
        assert!(librar(&ruta, 1, |_| false, SystemTime::now()).is_none());
        assert_eq!(std::fs::read_to_string(&ruta).unwrap(), "4242");
    }

    #[test]
    fn a_dead_old_lock_is_removed_and_taken_once() {
        let dir = tempdir().unwrap();
        let ruta = dir.path().join("ghost.lock");
        std::fs::write(&ruta, "4242").unwrap();
        envejecer(&ruta, vencido()).unwrap();
        let cerrojo =
            librar(&ruta, 7, |_| false, SystemTime::now()).expect("a stale lock is retaken");
        assert_eq!(std::fs::read_to_string(cerrojo.ruta()).unwrap(), "7");
    }

    #[test]
    fn an_unreadable_pid_with_a_fresh_file_stays_busy() {
        let dir = tempdir().unwrap();
        let ruta = dir.path().join("ghost.lock");
        std::fs::write(&ruta, "not-a-pid").unwrap();
        assert!(librar(&ruta, 1, |_| false, SystemTime::now()).is_none());
    }

    #[test]
    fn the_freshness_check_uses_the_file_mtime() {
        let dir = tempdir().unwrap();
        let ruta = dir.path().join("ghost.lock");
        std::fs::write(&ruta, "1").unwrap();
        assert!(fresco(&ruta, SystemTime::now()));
        envejecer(&ruta, vencido()).unwrap();
        assert!(!fresco(&ruta, SystemTime::now()));
        // A future mtime reads as fresh, never as stale.
        envejecer(&ruta, SystemTime::now() + Duration::from_secs(3600)).unwrap();
        assert!(fresco(&ruta, SystemTime::now()));
    }
}
