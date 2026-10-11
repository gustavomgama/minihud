//! GUI overlay (in progress).
//!
//! The pure [`model`] is landed and tested; the egui `app` and the `--gui`
//! wiring follow. Until the binary references it, the module is unused, so the
//! dead-code lint is silenced here — remove this once `--gui` lands.
#![allow(dead_code)]

pub mod config;
pub mod model;
pub mod theme;

#[cfg(feature = "gui")]
pub mod app;
