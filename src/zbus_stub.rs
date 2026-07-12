//! Non-Unix stub for the GUI Automation Bus ([`crate::zbus`]).
//!
//! The real bus is a Unix-domain-socket singleton daemon (see `zbus.rs`): it binds a private temp
//! socket, atomically `rename`s it over `$XDG_RUNTIME_DIR/zgui/zwire.sock`, and serializes ownership
//! with an advisory `flock`. That model is filesystem-socket-specific and has no direct Windows
//! named-pipe equivalent (pipes are named objects, not renamable paths), so `App::open("zwire")`
//! automation is Unix-only for now.
//!
//! These no-ops keep the host building and running on non-Unix targets — the primary NDJSON control
//! protocol is already cross-platform via [`crate::transport`] (Windows named pipes). Porting the bus
//! to a named-pipe singleton (a named-mutex ownership model) can replace this stub later.

/// No-op: the automation bus daemon is not available on non-Unix targets.
pub fn ensure_daemon() {}

/// No-op entry point for the internal `bus-daemon` subcommand. `ensure_daemon` never spawns it on
/// non-Unix, so this is reached only if invoked directly; exit cleanly.
pub fn run_daemon() -> ! {
    std::process::exit(0);
}
