//! `zwire-host` — one small self-contained binary that exposes the local
//! machine to any app over a JSON message protocol.
//!
//! Originally the Chrome native-messaging host for the [`zwire`] HUD, it is now a
//! **universal local endpoint**: system stats, a namespaced key/value store, a
//! filesystem crawler, subprocess exec, clipboard/notify/open, and multiplexed
//! PTY terminals — reachable from a browser extension *and* from tmux, emacs,
//! desktop apps, plugins, and any language.
//!
//! # Transports
//! * **Native messaging** (default): `u32`-length-prefixed JSON on stdio, for
//!   Chrome. Just run the binary with no recognised subcommand.
//! * **Socket daemon**: `zwire-host serve` listens on a Unix socket speaking
//!   newline-delimited JSON — the lingua franca every tool already has.
//! * **Client**: `zwire-host call '{"cmd":"hostinfo"}'` sends one request and
//!   prints the reply frames.
//!
//! Both transports feed the same [`session::Session`] dispatcher, so every
//! capability is reachable from every client.
//!
//! [`zwire`]: https://github.com/MenkeTechnologies/zwire
pub mod api;
pub mod exec;
pub mod fsops;
pub mod osops;
pub mod proto;
pub mod pty;
pub mod session;
pub mod store;
pub mod sysmon;
pub mod transport;

// Re-export the handful of types a dependent binary needs to embed the host.
// This lets sibling hosts (e.g. `zpwrchrome-host`) pull this crate in and reuse
// the dispatcher + transports directly:
//
// ```no_run
// // in another crate's main.rs, with `zwire-host` as a dependency:
// fn main() {
//     // zero-config: full native-messaging + `serve`/`call` behaviour
//     zwire_host::run(std::env::args().skip(1).collect());
// }
// ```
//
// Or drive the dispatcher yourself over any transport:
//
// ```no_run
// use zwire_host::{Peer, Session};
// let out = Peer::ndjson(Box::new(std::io::stdout()));
// let mut sess = Session::new();
// sess.handle(&out, &serde_json::json!({"cmd": "hostinfo"}));
// ```
pub use proto::{Framing, Out, Peer};
pub use session::{Session, CAPS};

use std::io::Read;
use std::path::PathBuf;

/// Crate version, surfaced in `hello`/`hostinfo` replies.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the socket daemon listens by default. `$ZWIRE_HOST_SOCK` overrides on
/// every platform. Otherwise:
///   * Windows — a per-user named pipe `\\.\pipe\zwire-host-<user>`.
///   * Unix — `$XDG_RUNTIME_DIR/zwire-host.sock`, else `~/.zwire/host.sock`.
///
/// On Windows the returned path's *leaf* is used as the pipe's namespaced name;
/// the directory portion is ignored.
pub fn default_socket() -> PathBuf {
    if let Ok(p) = std::env::var("ZWIRE_HOST_SOCK") {
        return PathBuf::from(p);
    }
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
        let safe: String = user
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect();
        let name = if safe.is_empty() { "user".into() } else { safe };
        PathBuf::from(format!("zwire-host-{name}"))
    }
    #[cfg(not(windows))]
    {
        if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
            if !rt.is_empty() {
                return PathBuf::from(rt).join("zwire-host.sock");
            }
        }
        store::app_dir("zwire").join("host.sock")
    }
}

const USAGE: &str = "\
zwire-host — universal local host (system stats · fs · exec · pty · kv · os)

USAGE:
    zwire-host                     native-messaging mode on stdio (Chrome default)
    zwire-host serve               run the NDJSON socket daemon
    zwire-host call '<json>'       send one request to the daemon, print reply
    zwire-host call                ...reading the request JSON from stdin
    zwire-host call --stream '...'  keep printing frames (sysinfo/pty streams)
    zwire-host version | help

OPTIONS:
    --socket <path>   override the socket path (default: $ZWIRE_HOST_SOCK or
                      $XDG_RUNTIME_DIR/zwire-host.sock or ~/.zwire/host.sock)
    --stream, -f      relay every reply frame instead of just the first

Examples:
    zwire-host serve &
    zwire-host call '{\"cmd\":\"hostinfo\"}'
    zwire-host call '{\"cmd\":\"fs_walk\",\"path\":\"~/src\",\"ext\":\"rs\"}'
    echo '{\"cmd\":\"exec\",\"program\":\"git\",\"args\":[\"status\"]}' | zwire-host call
    zwire-host call --stream '{\"cmd\":\"sysinfo_start\"}'
";

/// Entry point: interpret `args` (everything after `argv[0]`) and run the chosen
/// transport. Any unrecognised first token falls through to native-messaging
/// mode, because Chrome launches the host with extension-origin arguments we
/// must ignore.
pub fn run(args: Vec<String>) {
    // Pull optional flags (`--socket <path>`, `--stream`) out of the arg list.
    let mut socket = default_socket();
    let mut follow = false;
    let mut positional: Vec<String> = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--socket" | "-s" => {
                if let Some(p) = it.next() {
                    socket = PathBuf::from(p);
                }
            }
            "--stream" | "--follow" | "-f" => follow = true,
            _ => positional.push(a),
        }
    }

    match positional.first().map(String::as_str) {
        Some("serve") => transport::serve(&socket),
        Some("call") => {
            let rest = positional[1..].join(" ");
            let request = if rest.trim().is_empty() {
                let mut s = String::new();
                let _ = std::io::stdin().read_to_string(&mut s);
                s
            } else {
                rest
            };
            transport::call(&socket, &request, follow);
        }
        Some("version") | Some("--version") | Some("-V") => {
            println!("zwire-host {VERSION}");
        }
        Some("help") | Some("--help") | Some("-h") => {
            print!("{USAGE}");
        }
        // `stdio`, no args, or Chrome's origin argument → native messaging.
        _ => transport::stdio(),
    }
}
