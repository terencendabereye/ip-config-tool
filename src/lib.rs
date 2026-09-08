//! Library crate holding all testable logic plus the GUI. Split out from
//! `main.rs` so `cargo test` runs against the plain `lib` target, which does
//! NOT get the `requireAdministrator` manifest that `build.rs` embeds only
//! into the `[[bin]]` target (via `rustc-link-arg-bins`) — otherwise even
//! launching the test binary would require elevation.

pub mod ansi;
pub mod autodetect;
pub mod gui;
pub mod identify;
pub mod netconfig;
pub mod netiface;
pub mod oui;
mod pty;
pub mod scanner;
pub mod tools;
pub mod validate;
mod winproc;
