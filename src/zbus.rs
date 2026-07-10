//! GUI Automation Bus endpoint — makes `zwire-host` reachable as `App::open("zwire")` from a stryke
//! script (see GUI_AUTOMATION_BUS.md). This is the `stryke → zwire-host → zwire` entrypoint: a stryke
//! script drives the whole host command surface (fs / exec / kv / jobs / procs / hooks / open /
//! clipboard / notify / hostinfo / scheme / ui, and `bus_pub` to reach the browser HUD for tabs /
//! windows / terminal), and the host in turn drives the browser.
//!
//! We speak the `zgui-bridge` NDJSON wire protocol NATIVELY here rather than depending on the
//! `zgui-bridge` crate: zwire is MIT and public, `zgui-bridge` is a private UNLICENSED crate, so a
//! dependency would break public builds and mix licenses. zwire-host already owns Unix-socket + NDJSON
//! framing (see `transport.rs` / `proto.rs`), so the protocol is a few frames on top of that.
//!
//! Frames (one JSON object per line):
//!   in : {"t":"call","id":N,"verb":"<cmd>","args":{…}} | {"t":"get","id":N,"state":"<cmd>"}
//!        {"t":"verbs","id":N} | {"t":"sub","id":N,"event":"…"}
//!   out: {"t":"reply","id":N,"ok":true,"value":<host reply>} | {"t":"reply","id":N,"ok":false,"error":"…"}
//!
//! A `call`/`get` is translated to a host request `{"cmd":<verb>, …args}` and run through the REAL
//! [`session::Session::handle`] with a CAPTURING sink, so every host command works with zero
//! duplication. The socket lives at `$XDG_RUNTIME_DIR/zgui/zwire.sock` (else `$TMPDIR/zgui`, else
//! `/tmp/zgui`), matching the bus client's `socket_path`.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::proto::Peer;
use crate::session::Session;

/// The bus socket directory, matching the `zgui-bridge` client: `$XDG_RUNTIME_DIR/zgui`, else
/// `$TMPDIR/zgui`, else `/tmp/zgui`.
fn socket_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TMPDIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("zgui")
}

/// The host commands advertised on the automation surface (`App::open("zwire")->verbs()`). Discovery
/// only — `call` accepts any command `session::handle` understands, whether listed here or not.
const SURFACE_VERBS: &[&str] = &[
    "hello",
    "hostinfo",
    // key/value store (namespaced by `app`)
    "kv_get",
    "kv_set",
    "kv_merge",
    "kv_del",
    "kv_keys",
    // filesystem
    "fs_walk",
    "fs_read",
    "fs_write",
    "fs_stat",
    "fs_list",
    // process / exec
    "exec",
    "ps",
    "kill",
    "which",
    // os integration
    "open",
    "clipboard",
    "notify",
    // theme
    "scheme",
    "ui",
    // hooks
    "hooks_list",
    "hooks_events",
    "hooks_save",
    "hooks_delete",
    "hooks_set_enabled",
    "hook_fire",
    // pub/sub — reach the browser HUD (tabs / windows / terminal open-close are HUD commands the
    // browser executes when it receives the published event).
    "bus_pub",
    "bus_sub",
];

/// A `std::io::Write` sink that captures everything written into a shared buffer, so we can run a real
/// `Session` against an in-memory "connection" and read back the reply it emits via `respond`.
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Run one host command through a real (authed, local) session with a capturing sink and return the
/// reply object it emitted. Streaming commands (sysinfo/pty/job) return only their first frame.
fn run_command(verb: &str, args: &Value) -> Value {
    // Build the host request `{"cmd":verb, …args, "id":1}`.
    let mut obj = serde_json::Map::new();
    obj.insert("cmd".into(), json!(verb));
    if let Some(a) = args.as_object() {
        for (k, v) in a {
            if k != "cmd" {
                obj.insert(k.clone(), v.clone());
            }
        }
    }
    obj.insert("id".into(), json!(1));
    let msg = Value::Object(obj);

    let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let out = Peer::ndjson(Box::new(Capture(buf.clone())));
    let mut sess = Session::new();
    sess.handle(&out, &msg);

    // The sink holds NDJSON frames; the command's reply is the first line.
    let bytes = buf.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            return v;
        }
    }
    json!({ "ok": false, "err": "no reply from host session" })
}

/// The automation surface — every host command (for discovery) plus a couple of state queries.
fn surface() -> Value {
    let verbs: Vec<Value> = SURFACE_VERBS
        .iter()
        .map(|c| json!({ "id": *c, "label": *c }))
        .collect();
    json!({
        "app": "zwire",
        "verbs": verbs,
        "state": [
            json!({ "id": "hostinfo", "label": "Host info" }),
            json!({ "id": "scheme", "label": "Color scheme" }),
        ],
        "events": [],
    })
}

/// Frame + write one zgui-bridge reply on `w`.
fn reply(w: &mut UnixStream, id: u64, ok: bool, value: Value, error: Option<String>) {
    let mut r = serde_json::Map::new();
    r.insert("t".into(), json!("reply"));
    r.insert("id".into(), json!(id));
    r.insert("ok".into(), json!(ok));
    if ok {
        r.insert("value".into(), value);
    } else if let Some(e) = error {
        r.insert("error".into(), json!(e));
    }
    let mut line = serde_json::to_vec(&Value::Object(r)).unwrap_or_default();
    line.push(b'\n');
    let _ = w.write_all(&line);
    let _ = w.flush();
}

/// Serve one accepted bus connection: read zgui-bridge request frames, dispatch, reply.
fn handle_conn(stream: UnixStream) {
    let mut w = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = req.get("id").and_then(Value::as_u64).unwrap_or(0);
        match req.get("t").and_then(Value::as_str) {
            Some("call") => {
                let verb = req.get("verb").and_then(Value::as_str).unwrap_or("");
                let args = req.get("args").cloned().unwrap_or_else(|| json!({}));
                reply(&mut w, id, true, run_command(verb, &args), None);
            }
            Some("get") => {
                let state = req.get("state").and_then(Value::as_str).unwrap_or("");
                reply(&mut w, id, true, run_command(state, &json!({})), None);
            }
            Some("verbs") => reply(&mut w, id, true, surface(), None),
            // Event subscriptions are not bridged yet (host pub/sub is process-global and not
            // request/reply). Acknowledge so the client doesn't hang.
            Some("sub") => reply(&mut w, id, true, Value::Null, None),
            _ => reply(&mut w, id, false, Value::Null, Some("unknown request kind".into())),
        }
    }
}

/// Open the `App::open("zwire")` bus socket and serve it in the background. Best-effort: if another
/// zwire-host instance already owns the socket, we simply don't serve (first instance wins). Called
/// once at startup from every run mode so the bus is up whenever zwire-host is running.
pub fn start() {
    std::thread::Builder::new()
        .name("zwire-zbus".into())
        .spawn(|| {
            let dir = socket_dir();
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
            let sock = dir.join("zwire.sock");
            // Clear a stale socket from a crashed prior run, then bind. A live peer means another
            // instance owns it — leave it alone.
            if UnixStream::connect(&sock).is_ok() {
                return;
            }
            let _ = std::fs::remove_file(&sock);
            let listener = match UnixListener::bind(&sock) {
                Ok(l) => l,
                Err(_) => return,
            };
            let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => {
                        std::thread::spawn(move || handle_conn(stream));
                    }
                    Err(_) => break,
                }
            }
        })
        .ok();
}
