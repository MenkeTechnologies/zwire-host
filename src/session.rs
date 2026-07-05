//! The capability router shared by every transport.
//!
//! A [`Session`] is the per-connection state: its open PTYs and its sysinfo
//! stream. [`Session::handle`] takes one decoded request plus the connection's
//! [`Out`] sink and does the work — reply for RPC, or set up a background stream.
//! Both the Chrome stdio loop and the socket daemon drive it the same way, so
//! every capability is reachable from every client with no per-transport code.
use crate::proto::{respond, send_msg, Out};
use crate::store;
use crate::{exec, fsops, osops, pty, sysmon};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Capability tags advertised in the `hello` reply so a client can feature-test.
pub const CAPS: &[&str] = &[
    "hello",
    "kv",
    "fs",
    "exec",
    "sysinfo",
    "pty",
    "open",
    "clipboard",
    "notify",
    "hostinfo",
    "scheme",
    "ui",
];

/// Per-connection state. Dropping it tears down every PTY and stops the sysinfo
/// stream, so a client disconnect never leaks a shell or a thread.
#[derive(Default)]
pub struct Session {
    ptys: HashMap<String, pty::PtySession>,
    sysmon: Option<sysmon::Monitor>,
}

impl Session {
    /// Fresh session with no streams or terminals.
    pub fn new() -> Self {
        Self::default()
    }

    /// The session key for a PTY/stream request: its `id` string, or `""` for
    /// the legacy single-terminal default.
    fn key(req: &Value) -> String {
        req["id"].as_str().unwrap_or("").to_string()
    }

    /// The app namespace for state ops: `app` field, or `zwire` by default.
    fn app(req: &Value) -> String {
        req["app"].as_str().unwrap_or("zwire").to_string()
    }

    /// Handle one request. `out` is the connection's write sink; background
    /// capabilities (sysinfo, PTY output) keep writing to it after this returns.
    pub fn handle(&mut self, out: &Out, msg: &Value) {
        if let Some(cmd) = msg["cmd"].as_str() {
            self.handle_cmd(out, msg, cmd);
            return;
        }
        // Legacy commandless messages: {ui:{…}} and {scheme:"…"}.
        if !msg["ui"].is_null() {
            let ui = store::write_ui(&store::app_dir("zwire"), &msg["ui"]);
            respond(out, msg, json!({"ok": true, "ui": ui}));
        } else if let Some(s) = msg["scheme"].as_str() {
            self.set_scheme(out, msg, s);
        } else {
            respond(out, msg, json!({"ok": false, "err": "empty"}));
        }
    }

    fn handle_cmd(&mut self, out: &Out, msg: &Value, cmd: &str) {
        match cmd {
            "hello" | "ping" => respond(
                out,
                msg,
                json!({
                    "ok": true, "host": "zwire-host", "version": crate::VERSION,
                    "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
                    "pid": std::process::id(), "caps": CAPS,
                }),
            ),

            /* ---- namespaced key/value store ---- */
            "kv_get" => {
                let v = store::kv_get(&Self::app(msg), msg["key"].as_str().unwrap_or(""));
                respond(out, msg, json!({"ok": true, "value": v}));
            }
            "kv_set" => {
                let ok = store::kv_set(
                    &Self::app(msg),
                    msg["key"].as_str().unwrap_or(""),
                    &msg["value"],
                );
                respond(out, msg, json!({ "ok": ok }));
            }
            "kv_merge" => {
                let v = store::kv_merge(
                    &Self::app(msg),
                    msg["key"].as_str().unwrap_or(""),
                    &msg["value"],
                );
                respond(out, msg, json!({"ok": true, "value": v}));
            }
            "kv_del" => {
                let ok = store::kv_del(&Self::app(msg), msg["key"].as_str().unwrap_or(""));
                respond(out, msg, json!({ "ok": ok }));
            }
            "kv_keys" => {
                let keys = store::kv_keys(&Self::app(msg));
                respond(out, msg, json!({"ok": true, "keys": keys}));
            }

            /* ---- legacy zwire scheme + ui + get ---- */
            "get" => {
                let d = store::app_dir("zwire");
                respond(
                    out,
                    msg,
                    json!({"ok": true, "scheme": store::current_scheme(&d), "ui": store::current_ui(&d)}),
                );
            }

            /* ---- filesystem ---- */
            c if c.starts_with("fs_") => {
                let reply = fsops::handle(c, msg);
                respond(out, msg, reply);
            }

            /* ---- subprocess ---- */
            "exec" => respond(out, msg, exec::run(msg)),

            /* ---- os integration ---- */
            "open" | "clipboard_get" | "clipboard_set" | "notify" | "hostinfo" => {
                respond(out, msg, osops::handle(cmd, msg));
            }

            /* ---- live system stats ---- */
            "sysinfo_start" => {
                let interval = msg["interval_ms"].as_u64().unwrap_or(2000);
                self.sysmon = Some(sysmon::Monitor::start(out, interval, Self::key(msg)));
                respond(out, msg, json!({"ok": true, "streaming": true}));
            }
            "sysinfo_stop" => {
                self.sysmon = None;
                respond(out, msg, json!({"ok": true, "streaming": false}));
            }
            "sysinfo_once" => {
                use sysinfo::{Networks, System};
                let mut sys = System::new();
                sys.refresh_cpu_usage();
                sys.refresh_memory();
                let nets = Networks::new_with_refreshed_list();
                respond(
                    out,
                    msg,
                    json!({"ok": true, "sys": sysmon::snapshot(1.0, &nets, &sys)}),
                );
            }

            /* ---- multiplexed PTY terminals ---- */
            "pty_spawn" => {
                let key = Self::key(msg);
                match pty::PtySession::spawn(out, msg, key.clone()) {
                    Some(s) => {
                        self.ptys.insert(key, s);
                        respond(out, msg, json!({"ok": true, "spawned": true}));
                    }
                    None => respond(out, msg, json!({"ok": false, "err": "pty_spawn_failed"})),
                }
            }
            "pty_write" => {
                if let Some(s) = self.ptys.get_mut(&Self::key(msg)) {
                    s.write(msg);
                }
            }
            "pty_resize" => {
                if let Some(s) = self.ptys.get(&Self::key(msg)) {
                    s.resize(msg);
                }
            }
            "pty_kill" => {
                // Dropping the session kills the child; its reader thread emits
                // the `exit` frame.
                self.ptys.remove(&Self::key(msg));
            }

            _ => {
                let _ = send_msg(out, &json!({"ok": false, "err": "unknown_cmd", "cmd": cmd}));
            }
        }
    }

    fn set_scheme(&self, out: &Out, msg: &Value, s: &str) {
        if store::SCHEMES.contains(&s) {
            store::write_scheme(&store::app_dir("zwire"), s);
            respond(out, msg, json!({"ok": true, "scheme": s}));
        } else {
            respond(out, msg, json!({"ok": false, "err": "bad_scheme"}));
        }
    }
}
