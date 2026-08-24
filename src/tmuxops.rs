//! REAL tmux, reachable from the browser — the `tmux_*` commands, backed by
//! [`ztmux_core`].
//!
//! The extension already ships a *tiling metaphor* named tmux (`ztmux-config.js`
//! tiles web panes and binds a prefix key). This module is the other thing: the
//! actual multiplexer running in the user's terminal, driven from a ⌘K row. A
//! palette row sends `{type:'zb-host', req:{cmd:'tmux_tree'}}`, the worker relays
//! it here, and the reply is the live session tree.
//!
//! ## Why the engine is a crate and not a `tmux` subprocess
//!
//! `ztmux-core` speaks tmux's client/server wire protocol (OpenBSD imsg framing,
//! protocol version 8) straight to the server socket — the same protocol the
//! `tmux` binary speaks, not control mode. Shelling out to `tmux` per palette
//! action would cost a fork+exec per keystroke-triggered row and would answer in
//! text that has to be re-parsed; the crate answers in `serde_json::Value` that
//! goes back over native messaging unchanged. The one place a subprocess is
//! unavoidable is starting a server when none is running (the wire protocol
//! cannot fork a server) — `ops::ensure_socket` owns that, and only
//! `tmux_snap_restore` reaches it.
//!
//! ## The command vocabulary is shared, not invented here
//!
//! Every name and argument below matches zterminal's tmux commands
//! (`zterminal/src/event.rs`), which drive the same crate from its own frontend.
//! One vocabulary means a script, a chain, or a person who learned `tmux_send`
//! in one app can name it in the other; a second spelling of the same call would
//! be a second thing to keep in sync for no gain.
//!
//! ## Reply shape
//!
//! Reads answer `{"ok":true,"result":<value>}` — uniform on purpose, so the
//! palette's `hostReq` callback reads `r.result` for every read rather than a
//! different key per command. `ok:true` means the CALL succeeded; whether a
//! server was running is the payload's own `running` flag (and `tmux_status`
//! answers that question directly). Writes answer `{"ok":<bool>}`, with an `err`
//! when the server refused.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use ztmux_core::{ops, snapshot, transport};

/// Where the browser's saved sessions live under a given state directory.
/// Split from [`snapshot_dir`] so the naming can be tested against a scratch
/// base instead of creating a directory in the developer's real zwire state.
fn snapshot_dir_in(base: &Path) -> PathBuf {
    base.join("tmux-snapshots")
}

/// Snapshot store for the browser's own saved sessions:
/// `<zwire state dir>/tmux-snapshots`. Parameterizing the directory is why
/// `snapshot` takes one — zterminal keeps its own set under its settings dir, and
/// the two never collide.
///
/// Deliberately does NOT create the directory: `snapshot::save` does that on the
/// write path (`snapshot.rs`), and creating it here would make `tmux_snap_list`
/// leave an artifact on disk — which would cost it its `pure` class in the
/// reversibility table and, with it, its place inside a transaction.
fn snapshot_dir() -> PathBuf {
    snapshot_dir_in(&crate::store::app_dir("zwire"))
}

fn s(req: &Value, key: &str) -> String {
    req[key].as_str().unwrap_or("").to_string()
}

fn b(req: &Value, key: &str, default: bool) -> bool {
    req[key].as_bool().unwrap_or(default)
}

/// A JSON string array as `Vec<String>` — the `panes` selection and the raw
/// `args` argv both arrive this way.
fn strings(req: &Value, key: &str) -> Vec<String> {
    req[key]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn read(v: Value) -> Value {
    json!({ "ok": true, "result": v })
}

/// A write that the server can refuse. `false` carries a reason rather than a
/// bare `ok:false`, because from the palette the two indistinguishable causes —
/// no server at all, and a server that rejected the argument — need different
/// fixes from the user.
fn wrote(ok: bool) -> Value {
    if ok {
        json!({ "ok": true })
    } else {
        json!({"ok": false, "err": "tmux refused the write (no server running, or the target does not exist)"})
    }
}

fn result(r: Result<(), String>) -> Value {
    match r {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({"ok": false, "err": e}),
    }
}

/// Dispatch a `tmux_*` command. `id` stamping is handled by the caller.
pub fn handle(cmd: &str, req: &Value) -> Value {
    match cmd {
        /* ---- liveness ---- */
        "tmux_status" => status(),

        /* ---- reads: the whole server as JSON ---- */
        "tmux_tree" => read(ops::tree()),
        "tmux_sessions" => read(ops::sessions_summary()),
        "tmux_panes" => read(ops::panes()),
        "tmux_search" => read(ops::search_corpus()),
        "tmux_broadcast_list" => read(ops::broadcast_list()),
        "tmux_options" => read(ops::options()),
        "tmux_buffers" => read(ops::buffers()),
        "tmux_buffer" => read(ops::buffer(&s(req, "name"))),
        "tmux_keys" => read(ops::keys()),
        "tmux_export_state" => read(ops::export_state()),

        /* ---- a pane's visible text ---- */
        "tmux_capture" => match transport::socket_path() {
            Some(sock) => read(Value::String(ops::capture(&sock, &s(req, "pane")))),
            None => json!({"ok": false, "err": "no tmux server running"}),
        },

        /* ---- writes: drive the terminal ---- */
        "tmux_focus" => {
            let session = s(req, "session");
            if session.is_empty() {
                return json!({"ok": false, "err": "no session named"});
            }
            ops::focus(&session, &s(req, "window"), &s(req, "pane"));
            json!({ "ok": true })
        }
        "tmux_send" => {
            let panes = strings(req, "panes");
            if panes.is_empty() {
                return json!({"ok": false, "err": "no panes selected"});
            }
            ops::send_keys(&panes, &s(req, "text"), b(req, "enter", true));
            json!({"ok": true, "panes": panes.len()})
        }
        "tmux_sync" => {
            let window = s(req, "window");
            if window.is_empty() {
                return json!({"ok": false, "err": "no window named"});
            }
            ops::set_sync(&window, b(req, "on", false));
            json!({ "ok": true })
        }
        "tmux_set_option" => wrote(ops::set_option(
            &s(req, "scope"),
            &s(req, "name"),
            &s(req, "value"),
        )),
        "tmux_set_buffer" => wrote(ops::set_buffer(&s(req, "name"), &s(req, "content"))),
        "tmux_delete_buffer" => wrote(ops::delete_buffer(&s(req, "name"))),
        "tmux_paste_buffer" => wrote(ops::paste_buffer(&s(req, "name"), &s(req, "pane"))),
        "tmux_set_key" => wrote(ops::set_key(
            &s(req, "table"),
            &s(req, "key"),
            b(req, "repeat", false),
            &s(req, "command"),
        )),
        "tmux_unbind_key" => wrote(ops::unbind_key(&s(req, "table"), &s(req, "key"))),
        "tmux_import_state" => wrote(ops::import_state(&req["state"])),

        /* ---- raw tmux command lines ----
        `tmux_run` is fire-and-forget (the crate's own `run`, used for the
        one-key window/pane verbs); `tmux_command` waits and returns the
        server's exit code and output, which is what a chain step needs to
        decide whether it worked. */
        "tmux_run" => {
            let args = strings(req, "args");
            if args.is_empty() {
                return json!({"ok": false, "err": "no args"});
            }
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            ops::run(&argv);
            json!({ "ok": true })
        }
        "tmux_command" => {
            let args = strings(req, "args");
            if args.is_empty() {
                return json!({"ok": false, "err": "no args"});
            }
            let Some(sock) = transport::socket_path() else {
                return json!({"ok": false, "err": "no tmux server running"});
            };
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            match transport::command(&sock, &argv) {
                Ok(o) => {
                    json!({"ok": true, "code": o.exit, "stdout": o.stdout, "stderr": o.stderr})
                }
                Err(e) => json!({"ok": false, "err": e.to_string()}),
            }
        }

        /* ---- saved sessions (a tmux-resurrect replacement, native) ---- */
        "tmux_snap_list" => read(snapshot::list(&snapshot_dir())),
        "tmux_snap_detail" => read(snapshot::detail(&snapshot_dir(), &s(req, "name"))),
        "tmux_snap_save" => result(snapshot::save(
            &snapshot_dir(),
            &s(req, "name"),
            b(req, "contents", false),
        )),
        "tmux_snap_restore" => {
            let processes = req["processes"].as_str();
            result(snapshot::restore(
                &snapshot_dir(),
                &s(req, "name"),
                b(req, "relaunch", false),
                processes,
            ))
        }
        "tmux_snap_rename" => result(snapshot::rename(
            &snapshot_dir(),
            &s(req, "name"),
            &s(req, "new_name"),
        )),
        "tmux_snap_delete" => {
            snapshot::delete(&snapshot_dir(), &s(req, "name"));
            json!({ "ok": true })
        }

        _ => json!({"ok": false, "err": "unknown_cmd"}),
    }
}

/// Is there a server to talk to, and what would we start if there weren't?
///
/// The palette asks this before it publishes tmux rows: with no server, a row
/// that focuses a pane has nothing to focus, and the honest answer is one row
/// that says so rather than a dozen that fail on click.
fn status() -> Value {
    let sock = transport::socket_path();
    json!({
        "ok": true,
        "running": sock.is_some(),
        "socket": sock.as_ref().map(|p| p.display().to_string()),
        "attached": ops::clients_attached(),
        "bin": ops::tmux_bin().map(|p| p.display().to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every command answers a JSON object with an `ok` boolean — including on a
    /// machine with no tmux server, which is what CI is. A panic or a missing
    /// `ok` here would reach the browser as a dead ⌘K row.
    #[test]
    fn every_command_answers_ok_shaped_json_without_a_server() {
        let reqs = [
            json!({"cmd": "tmux_status"}),
            json!({"cmd": "tmux_tree"}),
            json!({"cmd": "tmux_sessions"}),
            json!({"cmd": "tmux_panes"}),
            json!({"cmd": "tmux_search"}),
            json!({"cmd": "tmux_broadcast_list"}),
            json!({"cmd": "tmux_options"}),
            json!({"cmd": "tmux_buffers"}),
            json!({"cmd": "tmux_buffer", "name": "buffer0"}),
            json!({"cmd": "tmux_keys"}),
            json!({"cmd": "tmux_export_state"}),
            json!({"cmd": "tmux_capture", "pane": "%0"}),
            json!({"cmd": "tmux_focus", "session": "s", "window": "0", "pane": "%0"}),
            json!({"cmd": "tmux_send", "panes": ["%0"], "text": "ls", "enter": true}),
            json!({"cmd": "tmux_sync", "window": "s:0", "on": true}),
            json!({"cmd": "tmux_set_option", "scope": "server", "name": "escape-time", "value": "0"}),
            json!({"cmd": "tmux_set_buffer", "name": "b", "content": "x"}),
            json!({"cmd": "tmux_delete_buffer", "name": "b"}),
            json!({"cmd": "tmux_paste_buffer", "name": "b", "pane": "%0"}),
            json!({"cmd": "tmux_set_key", "table": "prefix", "key": "F", "command": "next-window"}),
            json!({"cmd": "tmux_unbind_key", "table": "prefix", "key": "F"}),
            json!({"cmd": "tmux_import_state", "state": {}}),
            json!({"cmd": "tmux_run", "args": ["new-window"]}),
            json!({"cmd": "tmux_command", "args": ["list-sessions"]}),
        ];
        for req in reqs {
            let cmd = req["cmd"].as_str().unwrap();
            let reply = handle(cmd, &req);
            assert!(
                reply["ok"].is_boolean(),
                "{cmd} answered without an ok flag: {reply}"
            );
        }
    }

    /// A read that reaches no server still succeeds as a CALL and reports the
    /// absence in its payload — the distinction the palette needs to tell "tmux
    /// is not running" apart from "the host is broken".
    #[test]
    fn a_read_with_no_server_is_a_successful_call_reporting_running_false() {
        if transport::socket_path().is_some() {
            return; // a real server is running on this machine; the negative case cannot be observed
        }
        let tree = handle("tmux_tree", &json!({}));
        assert_eq!(tree["ok"], json!(true));
        assert_eq!(tree["result"]["running"], json!(false));
        assert_eq!(handle("tmux_status", &json!({}))["running"], json!(false));
    }

    /// Selection-less writes are refused here rather than forwarded. `send_keys`
    /// with an empty pane list would be a silent no-op that still answers ok, and
    /// a chain step cannot tell that apart from a delivery.
    #[test]
    fn writes_without_a_target_are_refused_with_a_reason() {
        for req in [
            json!({"cmd": "tmux_send", "text": "ls"}),
            json!({"cmd": "tmux_focus"}),
            json!({"cmd": "tmux_sync", "on": true}),
            json!({"cmd": "tmux_run"}),
            json!({"cmd": "tmux_command"}),
        ] {
            let cmd = req["cmd"].as_str().unwrap();
            let reply = handle(cmd, &req);
            assert_eq!(reply["ok"], json!(false), "{cmd} should refuse: {reply}");
            assert!(
                reply["err"].as_str().is_some_and(|e| !e.is_empty()),
                "{cmd} refused without a reason: {reply}"
            );
        }
    }

    /// The browser's snapshots hang off zwire's own state dir, never zterminal's
    /// — the two apps save independent sets through the same crate, and a shared
    /// directory would have one app's `tmux_snap_list` reporting the other's
    /// saves as its own.
    #[test]
    fn snapshots_hang_off_the_state_dir_they_are_given() {
        let base = std::env::temp_dir().join("zwh-tmux-snap-base");
        assert_eq!(snapshot_dir_in(&base), base.join("tmux-snapshots"));
    }
}
