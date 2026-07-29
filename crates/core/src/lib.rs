//! Core logic for nixos-update-notifier.
//!
//! This crate is deliberately free of any GUI/tray dependencies (no GTK, no ksni) so it
//! builds and unit-tests on a bare machine with no system libraries. The daemon and the
//! GTK client both depend on it.

pub mod apply;
pub mod changelog;
pub mod check;
pub mod config;
pub mod diff;
pub mod lock;
pub mod nix;
pub mod pkgs;

/// D-Bus well-known name, object path, and interface the daemon exposes and the GTK
/// client talks to. Kept here so both sides share one definition.
pub mod ipc {
    pub const BUS_NAME: &str = "org.nixos.UpdateNotifier";
    pub const OBJECT_PATH: &str = "/org/nixos/UpdateNotifier";
    pub const INTERFACE: &str = "org.nixos.UpdateNotifier1";
}
