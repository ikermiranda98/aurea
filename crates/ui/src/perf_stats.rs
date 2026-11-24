//! TEMPORARY (performance study): counts frames and desk snapshots and writes
//! a line every 10 s to `%TEMP%\aurea-frames.log` when `AUREA_FRAME_STATS` is
//! set. Not for commit.

use std::sync::atomic::{AtomicU64, Ordering};

pub static FRAMES: AtomicU64 = AtomicU64::new(0);
pub static SNAPSHOTS: AtomicU64 = AtomicU64::new(0);
pub static SNAPSHOT_BYTES: AtomicU64 = AtomicU64::new(0);
pub static DESK_RENDERS: AtomicU64 = AtomicU64::new(0);
pub static RAF_SHELL: AtomicU64 = AtomicU64::new(0);
pub static RAF_DOCK: AtomicU64 = AtomicU64::new(0);
pub static RAF_PROBE: AtomicU64 = AtomicU64::new(0);

pub fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("AUREA_FRAME_STATS").is_some();
        if on {
            std::thread::spawn(|| {
                let path = std::env::temp_dir().join("aurea-frames.log");
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(10));
                    let line = format!(
                        "{} frames={} snapshots={} snapshot_kb={} desk_renders={} raf_shell={} raf_dock={} raf_probe={}\n",
                        chrono::Local::now().format("%H:%M:%S"),
                        FRAMES.swap(0, Ordering::Relaxed),
                        SNAPSHOTS.swap(0, Ordering::Relaxed),
                        SNAPSHOT_BYTES.swap(0, Ordering::Relaxed) / 1024,
                        DESK_RENDERS.swap(0, Ordering::Relaxed),
                        RAF_SHELL.swap(0, Ordering::Relaxed),
                        RAF_DOCK.swap(0, Ordering::Relaxed),
                        RAF_PROBE.swap(0, Ordering::Relaxed),
                    );
                    use std::io::Write;
                    if let Ok(mut file) =
                        std::fs::OpenOptions::new().create(true).append(true).open(&path)
                    {
                        let _ = file.write_all(line.as_bytes());
                    }
                }
            });
        }
        on
    })
}

pub fn bump(counter: &AtomicU64, by: u64) {
    if enabled() {
        counter.fetch_add(by, Ordering::Relaxed);
    }
}
