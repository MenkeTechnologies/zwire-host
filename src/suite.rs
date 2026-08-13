//! Suite bus CLIENT — the browser dialing the OTHER apps on the GUI Automation Bus.
//!
//! [`zbus`](crate::zbus) is the SERVER leg: it makes zwire reachable as `App::open("zwire")`, so a
//! stryke script (or another app) can drive the browser. This is the mirror leg — zwire reaching
//! OUT. A page trigger, a ⌘K command, or a pane pipeline can name a verb on a *different* running
//! MenkeTechnologies app and get its return value back into the browser.
//!
//! The wire is the same one `zbus` already serves, from the other end: newline-delimited JSON at
//! `$XDG_RUNTIME_DIR/zgui/<app>.sock` (else `$TMPDIR/zgui`, else `/tmp/zgui`) on Unix, and the named
//! pipe `\\.\pipe\<app>.sock` on Windows.
//!
//! ```text
//!   → {"t":"verbs","id":1}
//!   ← {"t":"reply","id":1,"ok":true,"value":{"app":"zcite","verbs":[…],"state":[…],"events":[…]}}
//!   → {"t":"call","id":2,"verb":"item.add","args":{"doi":"10.1000/x"}}
//!   ← {"t":"reply","id":2,"ok":true,"value":{…}}
//!   → {"t":"get","id":3,"state":"selection"}
//!   ← {"t":"reply","id":3,"ok":true,"value":[…]}
//! ```
//!
//! ## A socket file is not a running app
//!
//! Enumeration cannot be a directory listing. The socket directory accumulates entries from
//! processes that died without unlinking (and from every test that ever bound a scratch name), so a
//! listing reports apps that have not run for days. [`list`] therefore *dials* every candidate with
//! a short connect timeout and keeps only the ones that answer — liveness is proven, never assumed.
//!
//! ## Deliberately NOT a saga coordinator
//!
//! Cross-app rollback already has an owner: zgo runs an ordered multi-app transaction and unwinds
//! every enlisted participant on failure. Reimplementing that here would be a second, divergent
//! coordinator. zwire is a plain participant/caller: one verb, one app, one reply. A chain that
//! needs all-or-nothing across apps calls zgo's own `saga.plan` / `saga.run` verbs *through* this
//! client, which is why [`call`] is enough to reach it.
//!
//! For the same reason `suite_call` is classed `irreversible` in the `zbus` REV table: zwire's
//! journal holds zwire's own writes, and it cannot compensate a write that happened inside another
//! process. A self-reverting chain refuses the step up front rather than claiming an undo it cannot
//! perform.

use std::io::{BufRead, BufReader, Read, Write};
use std::time::Duration;

use serde_json::{json, Value};

/// Connect timeout per app when probing liveness. Enumeration dials every candidate, so this is
/// also the ceiling on how long a dead-but-present socket can slow the whole listing down.
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);
/// Read timeout for one request/response exchange. A verb may do real work (a search, a fetch), but
/// it must never wedge the browser's UI thread waiting on another app.
const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// Write timeout, so a peer that stopped draining its socket cannot block us in `write_all`.
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);
/// Ceiling on one reply line. A confused (or hostile) peer must not be able to stream until this
/// process runs out of memory.
const MAX_REPLY_BYTES: u64 = 8 * 1024 * 1024;

/// This app's own bus names — excluded from [`list`] so "the other apps" means the other apps.
/// `zwire-page` is the browser's own second endpoint ([`crate::page`]), served by whichever host
/// process the browser is attached to: listing it would report zwire twice under a name no script
/// should dial directly.
const SELF_APPS: &[&str] = &["zwire", crate::page::PAGE_APP];

/* ---------------------------------------------------------------- transport */

#[cfg(unix)]
mod platform {
    use super::{PROBE_TIMEOUT, READ_TIMEOUT, WRITE_TIMEOUT};
    use std::io;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::time::Duration;

    /// The bus socket directory, matching `zbus`'s server side and the `zgui-bridge` client:
    /// `$XDG_RUNTIME_DIR/zgui`, else `$TMPDIR/zgui`, else `/tmp/zgui`.
    pub fn socket_dir() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("TMPDIR").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("zgui")
    }

    /// Bus names with a socket ENTRY present. Candidates only — presence proves nothing, so every
    /// caller passes these through a dial probe.
    pub fn candidates() -> Vec<String> {
        let mut names: Vec<String> = match std::fs::read_dir(socket_dir()) {
            Ok(rd) => rd
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.strip_suffix(".sock").map(str::to_string)
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        names.dedup();
        names
    }

    /// A connected, timeout-armed duplex stream to `app`, or the OS error.
    pub fn dial(app: &str, connect_timeout: Duration) -> io::Result<UnixStream> {
        let path = socket_dir().join(format!("{app}.sock"));
        // `UnixStream::connect_timeout` needs a `SocketAddr`; building one from a path is nightly-only
        // on some targets, so connect directly. A local socket either has a listener queued and
        // returns immediately, or fails immediately with ECONNREFUSED/ENOENT — the timeout below
        // still bounds every byte we then read or write.
        let _ = connect_timeout;
        let s = UnixStream::connect(path)?;
        s.set_read_timeout(Some(READ_TIMEOUT))?;
        s.set_write_timeout(Some(WRITE_TIMEOUT))?;
        Ok(s)
    }

    /// Cheap liveness probe: can we connect at all?
    pub fn alive(app: &str) -> bool {
        match dial(app, PROBE_TIMEOUT) {
            Ok(s) => {
                let _ = s.shutdown(std::net::Shutdown::Both);
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{READ_TIMEOUT, WRITE_TIMEOUT};
    use interprocess::local_socket::traits::Stream as _;
    use interprocess::local_socket::{prelude::*, GenericFilePath, Stream};
    use std::io;
    use std::path::PathBuf;
    use std::time::Duration;

    /// Nominal base — on Windows only the leaf matters, because the endpoint is a named pipe.
    pub fn socket_dir() -> PathBuf {
        PathBuf::from(r"\\.\pipe")
    }

    fn pipe_name(app: &str) -> io::Result<interprocess::local_socket::Name<'static>> {
        format!(r"\\.\pipe\{app}.sock")
            .to_fs_name::<GenericFilePath>()
            .map(|n| n.into_owned())
    }

    /// Named pipes live in a kernel object namespace exposed as `\\.\pipe\`, which `read_dir`
    /// enumerates. If that ever fails there is no fallback enumeration — a caller can still name an
    /// app explicitly, which is why `call`/`verbs` never depend on this.
    pub fn candidates() -> Vec<String> {
        let mut names: Vec<String> = match std::fs::read_dir(r"\\.\pipe\") {
            Ok(rd) => rd
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    name.strip_suffix(".sock").map(str::to_string)
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        names.dedup();
        names
    }

    /// A connected duplex stream to `app`, or the OS error.
    pub fn dial(app: &str, _connect_timeout: Duration) -> io::Result<Stream> {
        let _ = (READ_TIMEOUT, WRITE_TIMEOUT); // no per-handle timeout API on this transport
        Stream::connect(pipe_name(app)?)
    }

    /// Cheap liveness probe: can we connect at all? A named pipe is refcounted by the kernel, so a
    /// successful connect means a real owner exists — there is no stale path to mistake for one.
    pub fn alive(app: &str) -> bool {
        dial(app, super::PROBE_TIMEOUT).is_ok()
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::io;
    use std::path::PathBuf;
    use std::time::Duration;

    pub fn socket_dir() -> PathBuf {
        PathBuf::from("/tmp/zgui")
    }
    pub fn candidates() -> Vec<String> {
        Vec::new()
    }
    pub fn dial(_app: &str, _t: Duration) -> io::Result<std::net::TcpStream> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no local IPC transport on this platform",
        ))
    }
    pub fn alive(_app: &str) -> bool {
        false
    }
}

/* ------------------------------------------------------------- one exchange */

/// Send one request frame to `app` and return the peer's `value` (or its error string).
///
/// One connection per exchange: the bridge journals a transaction against a *held* connection, so a
/// long-lived shared connection here would silently enlist unrelated calls in whatever transaction
/// a previous caller left open. A short-lived dial has no such shared state, and dialing a local
/// socket is a few microseconds.
fn exchange(app: &str, req: Value) -> Result<Value, String> {
    if app.is_empty() {
        return Err("no app named".into());
    }
    // The bus name becomes a filename; a caller-supplied `../…` would otherwise dial outside the
    // socket directory entirely.
    if app.contains('/') || app.contains('\\') || app.contains("..") {
        return Err(format!("invalid app name: {app}"));
    }
    let stream = platform::dial(app, PROBE_TIMEOUT)
        .map_err(|e| format!("{app}: not running on the bus ({e})"))?;

    #[cfg(unix)]
    let (r, mut w) = {
        let r = stream
            .try_clone()
            .map_err(|e| format!("{app}: clone socket: {e}"))?;
        (r, stream)
    };
    #[cfg(windows)]
    let (r, mut w) = {
        use interprocess::local_socket::traits::Stream as _;
        stream.split()
    };
    #[cfg(not(any(unix, windows)))]
    let (r, mut w) = (stream.try_clone().map_err(|e| e.to_string())?, stream);

    let mut line = serde_json::to_string(&req).map_err(|e| e.to_string())?;
    line.push('\n');
    w.write_all(line.as_bytes())
        .map_err(|e| format!("{app}: write: {e}"))?;
    w.flush().map_err(|e| format!("{app}: flush: {e}"))?;

    let mut reader = BufReader::new(r.take(MAX_REPLY_BYTES));
    let mut buf = String::new();
    let n = reader
        .read_line(&mut buf)
        .map_err(|e| format!("{app}: read: {e}"))?;
    if n == 0 {
        return Err(format!("{app}: closed without replying"));
    }
    let reply: Value =
        serde_json::from_str(buf.trim()).map_err(|e| format!("{app}: bad reply frame: {e}"))?;
    if reply["ok"] == json!(true) {
        Ok(reply.get("value").cloned().unwrap_or(Value::Null))
    } else {
        Err(reply["error"]
            .as_str()
            .or_else(|| reply["err"].as_str())
            .unwrap_or("call failed")
            .to_string())
    }
}

/* -------------------------------------------------------------- public API */

/// Running apps on the bus, excluding zwire itself, each proven live by an actual dial.
///
/// Returns `{ok, apps: [name], probed: N}` — `probed` is how many socket entries existed, so a
/// caller can see the difference between "nothing is running" and "nothing is even installed".
pub fn list() -> Value {
    let cands = platform::candidates();
    let probed = cands.len();
    let apps: Vec<String> = cands
        .into_iter()
        .filter(|a| !SELF_APPS.contains(&a.as_str()))
        .filter(|a| platform::alive(a))
        .collect();
    json!({ "ok": true, "apps": apps, "probed": probed })
}

/// The typed automation surface of `app` — its verbs (with reversibility classes, where the peer
/// publishes them), state queries, and events.
pub fn verbs(app: &str) -> Result<Value, String> {
    exchange(app, json!({ "t": "verbs", "id": 1 }))
}

/// Invoke one verb on `app` and return its value.
pub fn call(app: &str, verb: &str, args: Value) -> Result<Value, String> {
    if verb.is_empty() {
        return Err("no verb named".into());
    }
    let args = if args.is_null() { json!({}) } else { args };
    exchange(
        app,
        json!({ "t": "call", "id": 1, "verb": verb, "args": args }),
    )
}

/// Read one state query from `app`.
pub fn get(app: &str, state: &str) -> Result<Value, String> {
    if state.is_empty() {
        return Err("no state named".into());
    }
    exchange(app, json!({ "t": "get", "id": 1, "state": state }))
}

/// Dispatch a `suite_*` host command. Returns `None` when `cmd` is not one of ours, so the caller's
/// match arm stays a single line and unknown commands keep falling through to the normal error.
pub fn command(cmd: &str, msg: &Value) -> Option<Value> {
    let app = msg["app"].as_str().unwrap_or("");
    let wrap = |r: Result<Value, String>| match r {
        Ok(v) => json!({ "ok": true, "result": v }),
        Err(e) => json!({ "ok": false, "err": e }),
    };
    match cmd {
        "suite_list" => Some(list()),
        "suite_verbs" => Some(wrap(verbs(app))),
        "suite_get" => Some(wrap(get(app, msg["state"].as_str().unwrap_or("")))),
        "suite_call" => Some(wrap(call(
            app,
            msg["verb"].as_str().unwrap_or(""),
            msg.get("args").cloned().unwrap_or(Value::Null),
        ))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_names_that_escape_the_socket_directory_are_refused() {
        // A bus name is interpolated into a filename. Without this guard a trigger could name
        // "../../tmp/evil" and dial an arbitrary socket on the machine.
        for bad in ["../zcite", "a/b", "..", r"a\b"] {
            let err = exchange(bad, json!({"t":"verbs","id":1})).unwrap_err();
            assert!(
                err.contains("invalid app name"),
                "{bad} should be refused, got: {err}"
            );
        }
    }

    #[test]
    fn empty_app_verb_and_state_are_refused_before_dialling() {
        assert!(exchange("", json!({}))
            .unwrap_err()
            .contains("no app named"));
        assert!(call("zcite", "", Value::Null)
            .unwrap_err()
            .contains("no verb named"));
        assert!(get("zcite", "").unwrap_err().contains("no state named"));
    }

    #[test]
    fn command_returns_none_for_foreign_commands() {
        assert!(command("fs_read", &json!({})).is_none());
        assert!(command("suite_list", &json!({})).is_some());
    }

    #[test]
    fn calling_an_app_that_is_not_running_reports_it_rather_than_hanging() {
        let r = call("zzz-no-such-app-on-the-bus", "noop", json!({}));
        let err = r.unwrap_err();
        assert!(
            err.contains("not running on the bus"),
            "unexpected error: {err}"
        );
    }
}
