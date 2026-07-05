//! The capability router shared by every transport.
//!
//! A [`Session`] is the per-connection state: its open PTYs and its sysinfo
//! stream. [`Session::handle`] takes one decoded request plus the connection's
//! [`Out`] sink and does the work — reply for RPC, or set up a background stream.
//! Both the Chrome stdio loop and the socket daemon drive it the same way, so
//! every capability is reachable from every client with no per-transport code.
use crate::proto::{respond, send_msg, Out};
use crate::store;
use crate::{bus, exec, fsops, jobs, osops, peer, watch};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Capability tags advertised in the `hello` reply so a client can feature-test.
/// The set reflects which optional capabilities were compiled in.
pub fn caps() -> Vec<&'static str> {
    #[cfg_attr(not(any(feature = "sysinfo-caps", feature = "pty")), allow(unused_mut))]
    let mut c = vec![
        "hello",
        "kv",
        "fs",
        "exec",
        "jobs",
        "bus",
        "peer",
        "watch",
        "open",
        "clipboard",
        "notify",
        "hostinfo",
        "scheme",
        "ui",
    ];
    #[cfg(feature = "sysinfo-caps")]
    {
        c.push("sysinfo");
        c.push("procs");
    }
    #[cfg(feature = "pty")]
    c.push("pty");
    c
}

/// Per-connection state. Dropping it tears down every PTY, stops the sysinfo
/// stream, and drops the connection's bus subscription + peer link, so a client
/// disconnect never leaks a shell, a thread, a subscriber, or a stale link.
pub struct Session {
    #[cfg(feature = "pty")]
    ptys: HashMap<String, crate::pty::PtySession>,
    watchers: HashMap<String, watch::Watcher>,
    #[cfg(feature = "sysinfo-caps")]
    sysmon: Option<crate::sysmon::Monitor>,
    /// Bus subscriber handle, allocated lazily on the first `sub`.
    sub_id: Option<u64>,
    /// Peer link handle, set when an inbound peer completes `peer_hello`.
    peer_id: Option<u64>,
    /// Whether this connection may run privileged commands. Local clients are
    /// trusted; TCP clients must authenticate first when a token is configured.
    authed: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(id) = self.sub_id {
            bus::unregister(id);
        }
        if let Some(id) = self.peer_id {
            peer::unregister_link(id);
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    /// Fresh, trusted session (local socket / stdio).
    pub fn new() -> Self {
        Self::guarded(false)
    }

    /// Session for a connection that must authenticate before privileged use
    /// when `require_auth` is set (inbound TCP with a token configured).
    pub fn guarded(require_auth: bool) -> Self {
        Session {
            #[cfg(feature = "pty")]
            ptys: HashMap::new(),
            watchers: HashMap::new(),
            #[cfg(feature = "sysinfo-caps")]
            sysmon: None,
            sub_id: None,
            peer_id: None,
            authed: !require_auth,
        }
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
            // Notify local subscribers and every peer so all apps on all
            // machines keep their UI prefs in sync live.
            bus::publish("ui", &ui);
            peer::broadcast("ui", &ui);
            respond(out, msg, json!({"ok": true, "ui": ui}));
        } else if let Some(s) = msg["scheme"].as_str() {
            self.set_scheme(out, msg, s);
        } else {
            respond(out, msg, json!({"ok": false, "err": "empty"}));
        }
    }

    fn handle_cmd(&mut self, out: &Out, msg: &Value, cmd: &str) {
        // Untrusted (TCP) connections may only authenticate or do harmless
        // discovery until they present the token.
        if !self.authed && !matches!(cmd, "auth" | "peer_hello" | "hello" | "ping") {
            respond(out, msg, json!({"ok": false, "err": "unauthorized"}));
            return;
        }
        match cmd {
            "hello" | "ping" => respond(
                out,
                msg,
                json!({
                    "ok": true, "host": "zwire-host", "version": crate::VERSION,
                    "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
                    "pid": std::process::id(), "caps": caps(),
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

            /* ---- streaming file observers ---- */
            "fs_watch" | "fs_tail" => {
                let key = Self::key(msg);
                let watcher = if cmd == "fs_watch" {
                    watch::Watcher::fs_watch(out, msg, key.clone())
                } else {
                    watch::Watcher::fs_tail(out, msg, key.clone())
                };
                self.watchers.insert(key, watcher);
                respond(out, msg, json!({"ok": true, "watching": true}));
            }
            "watch_stop" => {
                self.watchers.remove(&Self::key(msg));
                respond(out, msg, json!({"ok": true}));
            }
            "watch_list" => {
                let mut ids: Vec<&String> = self.watchers.keys().collect();
                ids.sort();
                respond(out, msg, json!({"ok": true, "watchers": ids}));
            }

            /* ---- filesystem ---- */
            c if c.starts_with("fs_") => {
                let reply = fsops::handle(c, msg);
                respond(out, msg, reply);
            }

            /* ---- subprocess ---- */
            "exec" => respond(out, msg, exec::run(msg)),

            /* ---- background jobs ---- */
            c if c.starts_with("job_") => {
                let reply = jobs::handle(c, msg);
                respond(out, msg, reply);
            }

            /* ---- process tools ---- */
            #[cfg(feature = "sysinfo-caps")]
            "ps" | "kill" | "which" => respond(out, msg, crate::procs::handle(cmd, msg)),

            /* ---- pub/sub event bus ---- */
            "sub" => {
                let id = *self.sub_id.get_or_insert_with(|| bus::register(out));
                if let Some(topic) = msg["topic"].as_str() {
                    bus::subscribe(id, topic);
                    respond(out, msg, json!({"ok": true, "topic": topic}));
                } else {
                    respond(out, msg, json!({"ok": false, "err": "no_topic"}));
                }
            }
            "unsub" => {
                if let (Some(id), Some(topic)) = (self.sub_id, msg["topic"].as_str()) {
                    bus::unsubscribe(id, topic);
                }
                respond(out, msg, json!({"ok": true}));
            }
            "pub" => match msg["topic"].as_str() {
                Some(topic) => {
                    let delivered = bus::publish(topic, &msg["data"]);
                    let forwarded = peer::broadcast(topic, &msg["data"]);
                    respond(
                        out,
                        msg,
                        json!({"ok": true, "delivered": delivered, "forwarded": forwarded}),
                    );
                }
                None => respond(out, msg, json!({"ok": false, "err": "no_topic"})),
            },

            /* ---- host-to-host peering ---- */
            "auth" => {
                if peer::token_ok(msg["token"].as_str()) {
                    self.authed = true;
                    respond(out, msg, json!({"ok": true}));
                } else {
                    respond(out, msg, json!({"ok": false, "err": "unauthorized"}));
                }
            }
            "peer_hello" => {
                if peer::token_ok(msg["token"].as_str()) {
                    self.authed = true;
                    let name = msg["name"].as_str().unwrap_or("peer");
                    self.peer_id = Some(peer::register_link(out, name));
                    respond(out, msg, json!({"ok": true, "name": peer::local_name()}));
                } else {
                    respond(out, msg, json!({"ok": false, "err": "unauthorized"}));
                }
            }
            // A federated event from a peer: deliver locally only (never
            // re-forward) so events can't loop around the mesh.
            "peer_pub" => {
                if let Some(topic) = msg["topic"].as_str() {
                    bus::publish(topic, &msg["data"]);
                }
            }
            "peers" => respond(
                out,
                msg,
                json!({"ok": true, "self": peer::local_name(), "peers": peer::peer_names()}),
            ),
            "peer_connect" => match msg["addr"].as_str() {
                Some(addr) => {
                    peer::dial(addr.to_string());
                    respond(out, msg, json!({"ok": true, "dialing": addr}));
                }
                None => respond(out, msg, json!({"ok": false, "err": "no_addr"})),
            },
            "remote" => match msg["peer"].as_str() {
                Some(addr) => match peer::remote(addr, &msg["request"]) {
                    Ok(reply) => {
                        respond(out, msg, json!({"ok": true, "peer": addr, "reply": reply}))
                    }
                    Err(e) => respond(out, msg, json!({"ok": false, "err": e})),
                },
                None => respond(out, msg, json!({"ok": false, "err": "no_peer"})),
            },

            /* ---- os integration ---- */
            "open" | "clipboard_get" | "clipboard_set" | "notify" | "hostinfo" => {
                respond(out, msg, osops::handle(cmd, msg));
            }

            /* ---- live system stats ---- */
            #[cfg(feature = "sysinfo-caps")]
            "sysinfo_start" => {
                let interval = msg["interval_ms"].as_u64().unwrap_or(2000);
                self.sysmon = Some(crate::sysmon::Monitor::start(out, interval, Self::key(msg)));
                respond(out, msg, json!({"ok": true, "streaming": true}));
            }
            #[cfg(feature = "sysinfo-caps")]
            "sysinfo_stop" => {
                self.sysmon = None;
                respond(out, msg, json!({"ok": true, "streaming": false}));
            }
            #[cfg(feature = "sysinfo-caps")]
            "sysinfo_once" => {
                use sysinfo::{Networks, System};
                let mut sys = System::new();
                sys.refresh_cpu_usage();
                sys.refresh_memory();
                let nets = Networks::new_with_refreshed_list();
                respond(
                    out,
                    msg,
                    json!({"ok": true, "sys": crate::sysmon::snapshot(1.0, &nets, &sys)}),
                );
            }

            /* ---- multiplexed PTY terminals ---- */
            #[cfg(feature = "pty")]
            "pty_spawn" => {
                let key = Self::key(msg);
                match crate::pty::PtySession::spawn(out, msg, key.clone()) {
                    Some(s) => {
                        self.ptys.insert(key, s);
                        respond(out, msg, json!({"ok": true, "spawned": true}));
                    }
                    None => respond(out, msg, json!({"ok": false, "err": "pty_spawn_failed"})),
                }
            }
            #[cfg(feature = "pty")]
            "pty_write" => {
                if let Some(s) = self.ptys.get_mut(&Self::key(msg)) {
                    s.write(msg);
                }
            }
            #[cfg(feature = "pty")]
            "pty_resize" => {
                if let Some(s) = self.ptys.get(&Self::key(msg)) {
                    s.resize(msg);
                }
            }
            #[cfg(feature = "pty")]
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
            // Push the change to every local subscriber and every peer for live
            // cross-app, cross-machine theme sync.
            let data = json!({ "scheme": s });
            bus::publish("scheme", &data);
            peer::broadcast("scheme", &data);
            respond(out, msg, json!({"ok": true, "scheme": s}));
        } else {
            respond(out, msg, json!({"ok": false, "err": "bad_scheme"}));
        }
    }
}
