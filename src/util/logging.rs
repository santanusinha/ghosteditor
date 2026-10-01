//! Stderr logging (P0.16). Never write to stdout: stdout carries MCP.

use std::sync::atomic::{AtomicBool, Ordering};

static DEBUG: AtomicBool = AtomicBool::new(false);

/// Turn debug logging on or off.
pub fn set_debug(on: bool) {
    DEBUG.store(on, Ordering::Relaxed);
}

fn prefix() -> &'static str {
    if DEBUG.load(Ordering::Relaxed) {
        "[ghostedit][debug]"
    } else {
        "[ghostedit]"
    }
}

/// Log a line to stderr. Always on: failures must be visible.
pub fn info(message: impl AsRef<str>) {
    eprintln!("{} {}", prefix(), message.as_ref());
}

/// Log a line to stderr only in debug mode.
pub fn debug(message: impl AsRef<str>) {
    if DEBUG.load(Ordering::Relaxed) {
        eprintln!("{} {}", prefix(), message.as_ref());
    }
}
