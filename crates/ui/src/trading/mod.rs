//! The trading surface (lane B): Aurea's desk dashboard.
//!
//! [`surface::TradingSurface`] renders the live snapshot streamed by
//! `WatchTradingSnapshot`; [`model`] holds the pure presentation helpers
//! (Spanish number formatting, KPI derivation, orderings) and is unit-tested
//! without a window.

pub mod close_gate;
pub mod i18n;
mod i18n_es;
pub mod model;
pub mod surface;

pub use surface::TradingSurface;
