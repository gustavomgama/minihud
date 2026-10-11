//! Host helpers for the tier-0 (ETW) follow loop.
//!
//! The in-process present hook has been removed — FPS is ETW-only (`src/fps/`).
//! This module is now just a namespace so the follow helpers keep their
//! `crate::hook::…` import paths:
//!
//! - [`follow`] — pure follow-target selection (`--match` glob, foreground
//!   choice, skip rules) and the `--follow` CLI parsing.
//! - [`proc`] — Win32 process enumeration (foreground pid, process list, image
//!   name, liveness). I/O only; no injection.

pub mod follow;
pub mod proc;
