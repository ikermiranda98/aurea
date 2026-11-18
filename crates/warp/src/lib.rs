//! The WARP ring (lane C1), a port of Openbot's `ghost.ts`.
//!
//! The ring rotates the machine's Cloudflare WARP exit until it lands on a new
//! `/64`, keeps the day's buckets in the shared memory, pins `opencode.ai` in
//! the hosts file and hangs the tunnel up when the app goes. The process-wide
//! pieces (lock file, `warp-cli`, hosts writes) arrive with the rest of the
//! lane; this crate starts with the pure decisions so they can be tested
//! without a network, a service or elevation.

pub mod bucket;
pub mod history;
pub mod lock;
pub mod memory;
pub mod pin;

pub use bucket::cubo;
pub use history::GhostRun;
pub use lock::{Cerrojo, librar};
pub use memory::{ajustar_memoria, dia_girado, podar_eventos, recordado};
pub use pin::{pin_vivo, sin_pin};
