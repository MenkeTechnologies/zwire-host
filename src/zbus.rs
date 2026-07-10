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

/// The command surface advertised on `App::open("zwire")->verbs()`. Comprehensive — every host
/// command `session::handle_cmd` (+ the fs/jobs/os/procs sub-handlers) accepts, plus the
/// `browser.*` verbs the Chromium HUD executes. Discovery only; `call` accepts any command.
const SURFACE_VERBS: &[&str] = &[
    "clipboard_get",
    "clipboard_set",
    "exec",
    "fs_append",
    "fs_list",
    "fs_mkdir",
    "fs_read",
    "fs_rm",
    "fs_stat",
    "fs_tail",
    "fs_walk",
    "fs_watch",
    "fs_write",
    "get",
    "hello",
    "hook_fire",
    "hooks_delete",
    "hooks_events",
    "hooks_get_script",
    "hooks_list",
    "hooks_save",
    "hooks_script_path",
    "hooks_set_enabled",
    "hooks_set_script",
    "hooks_test_run",
    "hostinfo",
    "hostlog",
    "job_list",
    "job_poll",
    "job_result",
    "job_start",
    "kill",
    "kv_del",
    "kv_get",
    "kv_keys",
    "kv_merge",
    "kv_set",
    "meter_stream",
    "notify",
    "open",
    "peer",
    "peer_connect",
    "peers",
    "ping",
    "ps",
    "pty_kill",
    "pty_resize",
    "pty_spawn",
    "pty_write",
    "pub",
    "stryke_lsp_send",
    "stryke_lsp_start",
    "stryke_lsp_stop",
    "stryke_run",
    "sub",
    "sysinfo_once",
    "sysinfo_start",
    "sysinfo_stop",
    "unsub",
    "watch_list",
    "watch_stop",
    "which",
    // BROWSER commands (Chromium HUD, forwarded via the zbus.action topic).
    "browser.newTab",
    "browser.newWindow",
    "browser.closeTab",
    "browser.closeOthers",
    "browser.reopenTab",
    "browser.duplicateTab",
    "browser.pinTab",
    "browser.muteTab",
    "browser.nextTab",
    "browser.prevTab",
    "browser.activate",
    "browser.open",
    "browser.openTab",
    "browser.tmux",
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
///
/// `browser.<action>` verbs are BROWSER commands the Chromium HUD executes (tabs / windows / etc.),
/// not host commands — they are FORWARDED to the HUD by publishing `{a:<action>, …args}` on the
/// `zbus.action` topic. background.js (subscribed on its persistent native port) writes that to
/// `zb_cmd` storage, which drives the existing action pipeline. Fire-and-forget (returns delivery
/// count, not the browser result). Works when this process owns the zgui socket AND holds the HUD's
/// subscription — the long-lived sysinfo host, which does both in practice.
fn run_command(verb: &str, args: &Value) -> Value {
    if let Some(action) = verb.strip_prefix("browser.") {
        let mut data = serde_json::Map::new();
        data.insert("a".into(), json!(action));
        if let Some(o) = args.as_object() {
            for (k, v) in o {
                if k != "cmd" && k != "a" {
                    data.insert(k.clone(), v.clone());
                }
            }
        }
        let payload = Value::Object(data);
        // Same-process fast path.
        let delivered = crate::bus::publish("zbus.action", &payload);
        let forwarded = crate::peer::broadcast("zbus.action", &payload);
        // Cross-process delivery: the HUD's host process is usually NOT the one that owns the zgui
        // socket (a separate `serve` daemon does), so pub/sub alone reaches no subscriber there.
        // Stamp the action into the file-backed KV with a monotonic nonce; background.js polls it on
        // its sysinfo stream and runs any action it hasn't seen. `App::open("zwire")->call("browser.*")`
        // then works regardless of which host answered the socket.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut kv = payload.as_object().cloned().unwrap_or_default();
        kv.insert("_n".into(), json!(nonce));
        crate::store::kv_set("zwire", "__zbus_action", &Value::Object(kv));
        return json!({ "ok": true, "action": action, "delivered": delivered, "forwarded": forwarded, "queued": true });
    }
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

/// Open the `App::open("zwire")` bus socket and serve it in the background. Called once at startup
/// from every run mode so the bus is up whenever zwire-host is running (including the short-lived
/// hosts a `stryke_run` spawns).
///
/// The BIND is SYNCHRONOUS — by the time this returns, the socket exists and is listening, so a
/// script that runs immediately afterward (e.g. `App::open("zwire")` inside a one-shot `stryke -E`
/// spawned by this very host) never races an unbound socket. Only the accept loop is backgrounded.
/// Best-effort: if another zwire-host already holds a LIVE socket we adopt it (don't rebind); a stale
/// socket from an exited host (connect → refused) is removed and rebound here.
pub fn start() {
    let dir = socket_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let sock = dir.join("zwire.sock");
    // A live listener means another instance owns the bus — leave it be (the child connects to it).
    if UnixStream::connect(&sock).is_ok() {
        return;
    }
    // Otherwise the file (if any) is stale: clear it and bind ourselves, synchronously.
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(_) => return,
    };
    let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
    std::thread::Builder::new()
        .name("zwire-zbus".into())
        .spawn(move || {
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
