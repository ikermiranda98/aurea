//! `zeron-tws` — the Interactive Brokers TWS API client for Aurea.
//!
//! Ported behaviour of Openbot's patched `@stoqey/ib` link layer (lane D1):
//! length-prefixed framing, field codec, client-id resolution, the 326
//! "client id already in use" verdict, and the reconnect/watchdog loop.
//!
//! Wire protocol: TCP, every message is a big-endian `u32` byte length
//! followed by NUL-terminated fields. See `SPECS/D1.md`.

pub mod client;
pub mod codec;
pub mod inbound;
pub mod requests;
