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
pub mod bus;
pub mod exec;
pub mod fsops;
pub mod jobs;
pub mod osops;
pub mod peer;
#[cfg(feature = "sysinfo-caps")]
pub mod procs;
pub mod proto;
#[cfg(feature = "pty")]
pub mod pty;
pub mod session;
pub mod store;
#[cfg(feature = "sysinfo-caps")]
pub mod sysmon;
pub mod transport;
pub mod watch;

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
pub use session::{caps, Session};

use std::io::Read;
use std::path::PathBuf;

/// Crate version, surfaced in `hello`/`hostinfo` replies.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where the socket daemon listens by default. `$ZWIRE_HOST_SOCK` overrides on
/// every platform. Otherwise:
///   * Windows — a per-user named pipe `\\.\pipe\zwire-host-<user>`.
///   * Unix — a *runtime* dir, never the persistent state dir: Linux
///     `$XDG_RUNTIME_DIR/zwire-host.sock`, macOS `$TMPDIR/zwire-host.sock`
///     (the per-user `/var/folders/…/T`, mode 0700), else `/tmp`.
///
/// A socket is ephemeral runtime state, so it deliberately does not live under
/// the app-data/state dir the scheme + kv files use.
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
        // Linux: the XDG runtime dir (/run/user/<uid>, mode 0700).
        if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
            if !rt.is_empty() {
                return PathBuf::from(rt).join("zwire-host.sock");
            }
        }
        // macOS: TMPDIR is the per-user Darwin temp dir (/var/folders/…/T, mode
        // 0700) — the moral equivalent of XDG_RUNTIME_DIR, and short enough to
        // stay under the 104-byte sun_path limit.
        if let Ok(tmp) = std::env::var("TMPDIR") {
            if !tmp.is_empty() {
                return PathBuf::from(tmp).join("zwire-host.sock");
            }
        }
        // Last resort: a per-user name in the world-shared /tmp. The daemon binds
        // 0600 and clears a stale file first, so a foreign owner just fails bind.
        let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
        let safe: String = user
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect();
        let user = if safe.is_empty() { "user".into() } else { safe };
        PathBuf::from("/tmp").join(format!("zwire-host-{user}.sock"))
    }
}

/// Cyberpunk `--help` wordmark (ANSI-Shadow "ZWIRE-HOST"), cyan→magenta→red.
const BANNER: &str = concat!(
    "\x1b[36m███████╗██╗    ██╗██╗██████╗ ███████╗    ██╗  ██╗ ██████╗ ███████╗████████╗\x1b[0m\n",
    "\x1b[36m╚══███╔╝██║    ██║██║██╔══██╗██╔════╝    ██║  ██║██╔═══██╗██╔════╝╚══██╔══╝\x1b[0m\n",
    "\x1b[35m  ███╔╝ ██║ █╗ ██║██║██████╔╝█████╗█████╗███████║██║   ██║███████╗   ██║   \x1b[0m\n",
    "\x1b[35m ███╔╝  ██║███╗██║██║██╔══██╗██╔══╝╚════╝██╔══██║██║   ██║╚════██║   ██║   \x1b[0m\n",
    "\x1b[31m███████╗╚███╔███╔╝██║██║  ██║███████╗    ██║  ██║╚██████╔╝███████║   ██║   \x1b[0m\n",
    "\x1b[31m╚══════╝ ╚══╝╚══╝ ╚═╝╚═╝  ╚═╝╚══════╝    ╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝   \x1b[0m\n",
);

/// Static body of the `--help` screen (a plain string literal, so the JSON
/// braces in the EXAMPLES section stay literal).
const HELP_BODY: &str = "  \x1b[35m>> UNIVERSAL LOCAL HOST // FULL SPECTRUM <<\x1b[0m\n\n  universal local host — system stats · fs · exec · pty · kv · os\n\n\x1b[33m  USAGE:\x1b[0m zwire-host [MODE] [OPTIONS]\n\n\x1b[36m  ── MODES ─────────────────────────────────────────────────────\x1b[0m\n  zwire-host                     \x1b[32m//\x1b[0m native-messaging on stdio (Chrome default)\n  zwire-host serve               \x1b[32m//\x1b[0m run the NDJSON socket daemon\n  zwire-host call '<json>'       \x1b[32m//\x1b[0m send one request to the daemon, print reply\n  zwire-host call                \x1b[32m//\x1b[0m ...reading the request JSON from stdin\n  zwire-host call --stream '...' \x1b[32m//\x1b[0m keep printing frames (sysinfo/pty streams)\n  zwire-host version | help      \x1b[32m//\x1b[0m print version / this help\n\n\x1b[36m  ── OPTIONS ───────────────────────────────────────────────────\x1b[0m\n  -s, --socket <path>            \x1b[32m//\x1b[0m socket ($ZWIRE_HOST_SOCK / $XDG_RUNTIME_DIR / $TMPDIR)\n  -f, --stream                   \x1b[32m//\x1b[0m relay every reply frame instead of just the first\n      --tcp <addr>               \x1b[32m//\x1b[0m (serve) also listen for peers/remote clients on TCP\n      --token <tok>              \x1b[32m//\x1b[0m (serve) shared secret required of inbound TCP\n      --name <name>              \x1b[32m//\x1b[0m (serve) advertised peer name (default: hostname)\n      --peer <addr>              \x1b[32m//\x1b[0m (serve) dial a peer and keep it linked; repeatable\n  -h, --help                     \x1b[32m//\x1b[0m print this help\n  -V, --version                  \x1b[32m//\x1b[0m print version\n\n\x1b[36m  ── EXAMPLES ──────────────────────────────────────────────────\x1b[0m\n  zwire-host serve &\n  zwire-host call '{\"cmd\":\"hostinfo\"}'\n  zwire-host call '{\"cmd\":\"fs_walk\",\"path\":\"~/src\",\"ext\":\"rs\"}'\n  echo '{\"cmd\":\"exec\",\"program\":\"git\",\"args\":[\"status\"]}' | zwire-host call\n  zwire-host call --stream '{\"cmd\":\"sysinfo_start\"}'\n  zwire-host serve --tcp 0.0.0.0:7420 --token SECRET --peer other.local:7420\n";

/// Build the styled `--help` / `-h` screen in the MenkeTechnologies house
/// style (see `tp -h`): banner, a status box padded at runtime so its right
/// border never drifts as VERSION grows, cyan section rules, green `//`.
fn usage() -> String {
    const BOX_W: usize = 72;
    let status = format!(" STATUS: ONLINE  // SIGNAL: ████████░░ // v{VERSION}");
    let space = " ".repeat(BOX_W.saturating_sub(status.chars().count()));
    let rule = "─".repeat(BOX_W);
    format!(
        "\n{BANNER} \x1b[36m┌{rule}┐\x1b[0m\n \x1b[36m│\x1b[0m{status}{space}\x1b[36m│\x1b[0m\n \x1b[36m└{rule}┘\x1b[0m\n{HELP_BODY}\n\x1b[36m  ── SYSTEM ────────────────────────────────────────────────────\x1b[0m\n  \x1b[35mv{VERSION} \x1b[0m// \x1b[33m(c) MenkeTechnologies\x1b[0m\n  \x1b[35mOne pipe. One binary. The whole machine.\x1b[0m\n  \x1b[33m>>> JACK IN. ONE SOCKET. OWN YOUR MACHINE. <<<\x1b[0m\n \x1b[36m░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░\x1b[0m\n"
    )
}

/// Entry point: interpret `args` (everything after `argv[0]`) and run the chosen
/// transport. Any unrecognised first token falls through to native-messaging
/// mode, because Chrome launches the host with extension-origin arguments we
/// must ignore.
pub fn run(args: Vec<String>) {
    // Pull optional flags out of the arg list.
    let mut socket = default_socket();
    let mut follow = false;
    let mut tcp: Option<String> = None;
    let mut token: Option<String> = None;
    let mut name: Option<String> = None;
    let mut peers: Vec<String> = Vec::new();
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
            "--tcp" => tcp = it.next(),
            "--token" => token = it.next(),
            "--name" => name = it.next(),
            "--peer" => {
                if let Some(p) = it.next() {
                    peers.push(p);
                }
            }
            _ => positional.push(a),
        }
    }

    match positional.first().map(String::as_str) {
        Some("serve") => transport::serve(transport::ServeConfig {
            socket,
            tcp,
            token,
            name,
            peers,
        }),
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
            print!("{}", usage());
        }
        // `stdio`, no args, or Chrome's origin argument → native messaging.
        _ => transport::stdio(),
    }
}
