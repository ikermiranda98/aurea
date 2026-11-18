//! The WARP ring (lane C1), a port of Openbot's `ghost.ts`.
//!
//! The ring rotates the machine's Cloudflare WARP exit until it lands on a new
//! `/64`, keeps the day's buckets in the shared memory, pins `opencode.ai` in
//! the hosts file and hangs the tunnel up when the app goes. [`ghost::Ghost`]
//! runs the ladder over [`Rutas`] and a [`Mundo`] (the machine's commands and
//! clock), so tests exercise it whole without a network or a service.

pub mod bucket;
pub mod ghost;
pub mod history;
pub mod lock;
pub mod memory;
pub mod mundo;
pub mod pin;
pub mod rutas;
pub mod shell;

pub use bucket::cubo;
pub use ghost::{Estado, EstadoWarp, Egress, Ghost, MAX_INTENTOS, Rotacion, monitor_command};
pub use history::GhostRun;
pub use lock::{Cerrojo, librar};
pub use memory::{ajustar_memoria, dia_girado, podar_eventos, recordado};
pub use mundo::{Mundo, Sistema};
pub use pin::{pin_vivo, sin_pin};
pub use rutas::{APP_IMAGENES, PIN_DEFECTO, PIN_LITERALES, Rutas};
pub use shell::{Salida, ejecutar, trazo};
