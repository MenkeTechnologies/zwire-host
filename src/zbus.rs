//! GUI Automation Bus endpoint — makes `zwire-host` reachable as `App::open("zwire")` from a stryke
//! script (see GUI_AUTOMATION_BUS.md). This is the `stryke → zwire-host → zwire` entrypoint: a stryke
//! script drives the whole host command surface (fs / exec / kv / jobs / procs / hooks / open /
//! clipboard / notify / hostinfo / scheme / ui, and `bus_pub` to reach the browser HUD for tabs /
//! windows / terminal), and the host in turn drives the browser.
//!
//! We speak the `zgui-bridge` NDJSON wire protocol NATIVELY here rather than depending on the
//! `zgui-bridge` crate: zwire is MIT and public, `zgui-bridge` is a private UNLICENSED crate, so a
//! dependency would break public builds and mix licenses. zwire-host already owns the local-IPC +
//! NDJSON framing (see `transport.rs` / `proto.rs`), so the protocol is a few frames on top of that.
//!
//! The bus is CROSS-PLATFORM: a Unix-domain socket at `$XDG_RUNTIME_DIR/zgui/zwire.sock` (else
//! `$TMPDIR/zgui`, else `/tmp/zgui`) on macOS/Linux, and the named pipe `\\.\pipe\zwire.sock` on
//! Windows. Both are served by a DEDICATED, detached `bus-daemon` singleton (see `ensure_daemon`);
//! the protocol dispatch below is identical on every platform. The address matches the bus client's
//! per-app path (`<app>.sock` → same leaf → `\\.\pipe\<app>.sock`), so client and host meet with no
//! registry.
//!
//! Frames (one JSON object per line):
//!
//! ```text
//!   in : {"t":"call","id":N,"verb":"<cmd>","args":{…}} | {"t":"get","id":N,"state":"<cmd>"}
//!        {"t":"verbs","id":N} | {"t":"sub","id":N,"event":"…"}
//!        {"t":"begin","id":N,"args":{…}} | {"t":"commit","id":N,"args":{"txn":T}}
//!        {"t":"abort","id":N,"args":{"txn":T}} | {"t":"undo","id":N,"args":{…}}
//!   out: {"t":"reply","id":N,"ok":true,"value":<host reply>} | {"t":"reply","id":N,"ok":false,"error":"…"}
//! ```
//!
//! PROTOCOL DRIFT IS THE STANDING HAZARD HERE. Because the frames above are hand-mirrored rather
//! than shared with the `zgui-bridge` crate, a frame added there and forgotten here degrades to the
//! `"unknown request kind"` fallthrough in [`serve_conn`](crate::zbus::serve_conn) — a clean error, never a hang, but still a
//! silent capability gap. `tests/wire_conformance.rs` pins the mirrored frame corpus so the gap
//! fails a build instead of a script. Every frame added to `zgui-bridge/src/proto.rs` must be added
//! to that corpus and to the match below in the same change.
//!
//! A `call`/`get` is translated to a host request `{"cmd":<verb>, …args}` and run through the REAL
//! [`session::Session::handle`] with a CAPTURING sink, so every host command works with zero
//! duplication.

use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::proto::Peer;
use crate::session::Session;

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
    "verbs",
    "watch_list",
    "watch_stop",
    "which",
    // BROWSER commands (Chromium HUD, forwarded via the zbus.action topic). Discovery only —
    // `call` accepts any `browser.<verb>`; the HUD worker's execZbCmd is the executor.
    // tabs
    "browser.newTab",
    "browser.openTab",
    "browser.closeTab",
    "browser.closeOthers",
    "browser.closeRight",
    "browser.closeLeft",
    "browser.closeDuplicates",
    "browser.reopenTab",
    "browser.duplicateTab",
    "browser.pinTab",
    "browser.unpinTab",
    "browser.muteTab",
    "browser.unmuteTab",
    "browser.muteOthers",
    "browser.discardTab",
    "browser.activate",
    "browser.open",
    // tab navigation / position
    "browser.nextTab",
    "browser.prevTab",
    "browser.firstTab",
    "browser.lastTab",
    "browser.gotoTab",
    "browser.moveTabLeft",
    "browser.moveTabRight",
    "browser.moveTabFirst",
    "browser.moveTabLast",
    "browser.tabToNewWindow",
    // page navigation (active tab)
    "browser.reload",
    "browser.reloadHard",
    "browser.reloadAll",
    "browser.goBack",
    "browser.goForward",
    "browser.home",
    // zoom
    "browser.zoomIn",
    "browser.zoomOut",
    "browser.zoomReset",
    // windows
    "browser.newWindow",
    "browser.incognitoWindow",
    "browser.closeWindow",
    "browser.minimizeWindow",
    "browser.maximizeWindow",
    "browser.fullscreenWindow",
    "browser.restoreWindow",
    "browser.nextWindow",
    "browser.prevWindow",
    "browser.mergeWindows",
    // window tiling / positioning (system.display work area)
    "browser.snapLeft",
    "browser.snapRight",
    "browser.snapTop",
    "browser.snapBottom",
    "browser.snapTopLeft",
    "browser.snapTopRight",
    "browser.snapBottomLeft",
    "browser.snapBottomRight",
    "browser.centerWindow",
    "browser.moveWindowNextDisplay",
    // bulk tab ops / capture / language
    "browser.muteAll",
    "browser.unmuteAll",
    "browser.pinAll",
    "browser.unpinAll",
    "browser.sortTabs",
    "browser.screenshot",
    "browser.detectLanguage",
    // tab groups
    "browser.groupTabs",
    "browser.ungroupTabs",
    "browser.collapseGroups",
    "browser.expandGroups",
    // downloads
    "browser.download",
    "browser.pauseDownload",
    "browser.resumeDownload",
    "browser.cancelDownload",
    "browser.openDownload",
    "browser.showDownload",
    "browser.retryDownload",
    "browser.clearDownloads",
    "browser.showDownloads",
    // browsing data
    "browser.clearCache",
    "browser.clearCookies",
    "browser.clearCacheAndCookies",
    "browser.clearAllData",
    "browser.clearPasswords",
    // reading list
    "browser.addReadingList",
    "browser.removeReadingList",
    // power
    "browser.keepAwake",
    "browser.keepDisplayAwake",
    "browser.allowSleep",
    // extension / app management (id param)
    "browser.enableExtension",
    "browser.disableExtension",
    "browser.uninstallExtension",
    "browser.launchApp",
    "browser.extensionOptions",
    // history / bookmarks / notifications
    "browser.clearHistory",
    "browser.deleteHistoryUrl",
    "browser.addHistoryUrl",
    "browser.bookmarkTab",
    "browser.bookmarkFolder",
    "browser.removeBookmark",
    "browser.notify",
    // terminal
    "browser.tmux",
    // transactional compensation (see `txn.rs`); `browser.undo` is executed by the HUD worker,
    // which owns the per-step pre-state journal.
    "browser.undo",
    "txn_begin",
    "txn_commit",
    "txn_abort",
];

/// The reversibility class of a verb, as advertised on `surface()` and enforced at call time while
/// a transaction is open.
///
/// * `"inverse"` — a compensation exists that restores the prior observable state. Journaled on
///   call, replayed in reverse order on abort.
/// * `"pure"` — reads only. Runs inside a transaction, is not journaled, is not compensated.
/// * `"irreversible"` — the DEFAULT for anything absent from [`REV`]. Rejected at call time while a
///   transaction is open, so an un-undoable step fails fast at the top of a chain instead of
///   stranding it half-compensated at abort time.
///
/// Only `browser.*` verbs can be `"inverse"`: the HUD service worker captures each one's pre-state
/// at execution time and replays the inverse (`browser.undo`). No host-side command has a
/// compensation, so every host WRITE is `"irreversible"` here rather than optimistically classed.
/// [`crate::txn`] journals a step's verb and args and nothing else — there is no pre-state anywhere
/// on this side — so `kv_set`, `clipboard_set`, `fs_write` and friends stay irreversible no matter
/// how obvious their "inverse" reads on paper.
///
/// WHAT MAKES A `browser.*` VERB `"inverse"` is narrower than "the browser could undo it". The HUD
/// journal (`zwire/extensions/hud-internal/zjournal.js`) captures effects by OBSERVING chrome
/// events, and it listens to exactly eight: `tabs.onCreated`, `onRemoved`, `onMoved`, `onDetached`,
/// `onUpdated` (pinned / muted / url only), `onActivated`, `onZoomChange`, and `windows.onCreated`.
/// It can replay exactly eight inverse ops: reopen, close, closeWindow, move, flags, activate,
/// navigate, zoom. A verb whose effect falls OUTSIDE that vocabulary — a window state or bounds
/// change, a tab group, a download, a bookmark, a reading-list entry, browsing data, an extension
/// toggle — journals NOTHING, so classing it `"inverse"` would produce an abort that reports a
/// clean revert having compensated nothing. That failure is silent, which is why the bar here is
/// "the journal observes and replays every effect this verb produces", not "an undo is imaginable".
///
/// The verbs deliberately left `"irreversible"`, each with the reason, are enumerated in
/// `tests/rev_coverage.rs`. That ledger plus this table must together cover [`SURFACE_VERBS`], so a
/// verb added to the surface fails the suite until someone classifies it.
const REV: &[(&str, &str)] = &[
    /* ---- host reads: safe inside a transaction, nothing to unwind ---- */
    ("clipboard_get", "pure"),
    ("fs_list", "pure"),
    ("fs_read", "pure"),
    ("fs_stat", "pure"),
    ("fs_walk", "pure"),
    ("get", "pure"),
    ("hello", "pure"),
    ("hooks_events", "pure"),
    ("hooks_get_script", "pure"),
    ("hooks_list", "pure"),
    ("hooks_script_path", "pure"),
    ("hostinfo", "pure"),
    ("hostlog", "pure"),
    ("job_list", "pure"),
    ("job_poll", "pure"),
    ("job_result", "pure"),
    ("kv_get", "pure"),
    ("kv_keys", "pure"),
    ("peer", "pure"),
    ("peers", "pure"),
    ("ping", "pure"),
    ("ps", "pure"),
    ("sysinfo_once", "pure"),
    ("verbs", "pure"),
    ("watch_list", "pure"),
    ("which", "pure"),
    /* ---- browser tabs: worker journals {id,url,index,windowId,pinned,muted} ---- */
    ("browser.closeTab", "inverse"),
    ("browser.closeOthers", "inverse"),
    ("browser.closeRight", "inverse"),
    ("browser.closeLeft", "inverse"),
    ("browser.closeDuplicates", "inverse"),
    /* ---- browser tab creation: worker journals the created id ---- */
    ("browser.newTab", "inverse"),
    ("browser.openTab", "inverse"),
    ("browser.duplicateTab", "inverse"),
    ("browser.newWindow", "inverse"),
    // `management.get` is a read; the only effect is the one `tabs.create` for the options page,
    // which the worker journals as a `close` exactly like `openTab`.
    ("browser.extensionOptions", "inverse"),
    /* ---- browser tab flags: worker journals the prior flag per affected tab ---- */
    ("browser.pinTab", "inverse"),
    ("browser.unpinTab", "inverse"),
    ("browser.muteTab", "inverse"),
    ("browser.unmuteTab", "inverse"),
    ("browser.muteOthers", "inverse"),
    // Bulk mute is `muteOthers` over the whole window: one `mutedInfo` update per tab whose flag
    // actually changed, journaled as that tab's prior `muted`. Muting never reorders the strip, so
    // there are no `move` ops to interleave and the replay is order-independent. (Bulk PIN is not
    // here — pinning moves tabs, see the ledger in `tests/rev_coverage.rs`.)
    ("browser.muteAll", "inverse"),
    ("browser.unmuteAll", "inverse"),
    /* ---- browser tab position: worker journals the prior index ---- */
    ("browser.moveTabLeft", "inverse"),
    ("browser.moveTabRight", "inverse"),
    ("browser.moveTabFirst", "inverse"),
    ("browser.moveTabLast", "inverse"),
    ("browser.tabToNewWindow", "inverse"),
    // One `tabs.move` per unpinned tab, each journaled from `onMoved.fromIndex` (authoritative
    // pre-state, not re-derived). A single-element move is inverted by moving that element back to
    // its recorded index — the other tabs keep their relative order — so replaying the moves
    // newest-first restores the exact prior permutation.
    ("browser.sortTabs", "inverse"),
    /* ---- browser selection: worker journals the prior active tab ---- */
    ("browser.activate", "inverse"),
    ("browser.firstTab", "inverse"),
    ("browser.lastTab", "inverse"),
    ("browser.gotoTab", "inverse"),
    ("browser.nextTab", "inverse"),
    ("browser.prevTab", "inverse"),
    /* ---- browser page: worker journals the prior url / zoom factor ---- */
    ("browser.open", "inverse"),
    ("browser.zoomIn", "inverse"),
    ("browser.zoomOut", "inverse"),
    ("browser.zoomReset", "inverse"),
    ("browser.goBack", "inverse"),
    ("browser.goForward", "inverse"),
    // `home` is `open` with a fixed target: one `tabs.update({url})` on the active tab, journaled
    // as `navigate` back to the url it was showing.
    ("browser.home", "inverse"),
    /* ---- browser reads / idempotent refreshes ---- */
    ("browser.reload", "pure"),
    ("browser.reloadHard", "pure"),
    ("browser.reloadAll", "pure"),
    /* ---- transaction control itself: never journaled, never compensated ---- */
    ("txn_begin", "pure"),
    ("txn_commit", "pure"),
    ("txn_abort", "pure"),
    ("browser.undo", "pure"),
];

/// The reversibility class of `verb` — `"inverse"`, `"pure"`, or `"irreversible"` (the default for
/// anything `REV` does not name, including every unknown verb).
pub fn rev(verb: &str) -> &'static str {
    REV.iter()
        .find(|(id, _)| *id == verb)
        .map(|(_, class)| *class)
        .unwrap_or("irreversible")
}

/// A `std::io::Write` sink that captures everything written into a shared buffer, so we can run a real
/// `Session` against an in-memory "connection" and read back the reply it emits via `respond`.
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(b);
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
/// A monotonic, unique stamp for one forwarded browser action.
///
/// EVERY delivery path carries the same `_n` for a given action — the `zbus.action` publish, the peer
/// broadcast, and the file-backed kv the `stryke_run` reply piggybacks. That is what lets the HUD
/// worker accept all of them and still execute the action exactly once, instead of the old choice
/// between a duplicate (two paths deliver) and a drop (only one path is wired).
///
/// Seeded from the wall clock so it keeps rising across a host restart, then advanced by one per
/// action so a tight chain issued inside a single millisecond still gets distinct stamps — a
/// collision there would silently DROP a step of the chain, which is worse than a duplicate.
fn action_nonce() -> u64 {
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    NONCE
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            Some(cur.max(ms) + 1)
        })
        .map_or(ms, |prev| prev.max(ms) + 1)
}

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
        let nonce = action_nonce();
        data.insert("_n".into(), json!(nonce));
        let payload = Value::Object(data);
        // Same-process fast path: the HUD worker holds a `zbus.action` subscription on its persistent
        // native port, so a chain of N actions arrives as N frames in order — the kv path below can
        // only ever hold the LAST one.
        let delivered = crate::bus::publish("zbus.action", &payload);
        let forwarded = crate::peer::broadcast("zbus.action", &payload);
        // Cross-process delivery: the HUD's host process is usually NOT the one that owns the zgui
        // socket (a separate `serve` daemon does), so pub/sub alone reaches no subscriber there.
        // Stamp the action into the file-backed KV; the `stryke_run` reply piggybacks it (session.rs)
        // so `App::open("zwire")->call("browser.*")` works regardless of which host answered the
        // socket. Same `_n` as the publish, so a doubly-delivered action still runs once.
        crate::store::kv_set("zwire", "__zbus_action", &payload);
        return json!({ "ok": true, "action": action, "nonce": nonce, "delivered": delivered, "forwarded": forwarded, "queued": true });
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

/// Dispatch one bus `call`: the reversibility gate, the journal stamp, and the command itself.
///
/// Shared by the NDJSON `{"t":"call"}` frame ([`serve_conn`]) and the plain `{"cmd":"call"}` host
/// command ([`crate::session::Session`]), because a transaction is only coherent when every step in
/// it reaches the SAME journal. The journal is process-global, so a caller that opens a transaction
/// with one connection and calls verbs on another process journals nothing and compensates nothing
/// on abort — an aborted chain that silently unwinds zero steps. Routing both entrypoints through
/// here is what lets the Chromium HUD (native messaging) and a stryke script (the socket) share one
/// transaction.
///
/// `Err` is the refusal of an `irreversible` verb while a transaction is open — the class check
/// happens BEFORE the command runs, so a chain fails at its first un-undoable step rather than
/// stranding itself half-compensated at abort time.
pub fn call_verb(verb: &str, mut args: Value, txn: Option<u64>) -> Result<Value, String> {
    // Outside a transaction this is exactly the pre-transaction path: no class check, no journal.
    if crate::txn::any_open() && txn.is_none_or(crate::txn::is_open) {
        match rev(verb) {
            "irreversible" => return Err(format!("verb not reversible: {verb}")),
            // `pure` changes nothing, so there is nothing to compensate.
            //
            // The journal here holds the ORDER; the HUD service worker holds the PRE-STATE, which
            // it can only file under a key both sides agree on. Stamping the entry's `(txn, seq)`
            // onto the forwarded action IS that key: `run_command` copies every arg into the
            // `zbus.action` payload, so the worker sees `_seq` and journals what the action changed
            // under it. Without the stamp the worker cannot tell a transacted action from an
            // ordinary one, and `browser.undo` has nothing to look up.
            "inverse" => {
                if let Some(seq) = crate::txn::record(txn, verb, &args) {
                    if let Some(o) = args.as_object_mut() {
                        o.insert("_txn".into(), json!(txn));
                        o.insert("_seq".into(), json!(seq));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(run_command(verb, &args))
}

/// Transaction control (`txn_begin` / `txn_commit` / `txn_abort`), dispatched from
/// [`crate::session::Session`] so the frames and the plain host commands share one implementation.
///
/// `txn_abort` is the one that does work: it drains the journal in reverse `seq` order and forwards
/// a SINGLE `browser.undo` carrying the reversed step list. The HUD worker holds the per-step
/// pre-state (a `browser.*` forward call returns a delivery count, not a browser result — see
/// `run_command`), so the host contributes the ORDER and the worker contributes the inverses. The
/// `txn-aborted` lifecycle event fires afterwards so a hook can observe the compensation.
pub fn txn_command(cmd: &str, args: &Value) -> Value {
    match cmd {
        "txn_begin" => crate::txn::begin(args),
        "txn_commit" => crate::txn::commit(args),
        "txn_abort" => {
            let txn = match args.get("txn").and_then(Value::as_u64) {
                Some(t) => t,
                None => return json!({ "ok": false, "err": "no txn" }),
            };
            // Removed from the journal before anything is compensated, so a concurrent second
            // abort of the same transaction unwinds nothing rather than unwinding twice.
            let entries = crate::txn::take_reversed(txn);
            let steps: Vec<Value> = entries
                .iter()
                .map(|e| json!({ "seq": e.seq, "verb": e.verb, "args": e.args }))
                .collect();
            let undo = if steps.is_empty() {
                Value::Null
            } else {
                run_command("browser.undo", &json!({ "txn": txn, "steps": steps }))
            };
            crate::hooks::fire("txn-aborted", json!({ "txn": txn, "steps": steps.len() }));
            json!({
                "ok": true,
                "txn": txn,
                "steps": steps.len(),
                "aborted": true,
                "undo": undo,
            })
        }
        _ => json!({ "ok": false, "err": "unknown_cmd", "cmd": cmd }),
    }
}

/// The automation surface — every host command (for discovery) plus a couple of state queries.
/// Each verb carries its `rev` class (see [`rev`]) so a script can tell, before it builds a chain,
/// which steps are safe to put inside `App::txn`.
///
/// Reachable as the `{"t":"verbs"}` bus frame AND as the plain `{"cmd":"verbs"}` host command, so
/// the Chromium HUD's trigger editor classifies a chain from THIS table instead of mirroring [`REV`]
/// in JavaScript — a mirror drifts the moment a verb is added here, and a drifted mirror tells the
/// author a chain is revertible when the host will refuse it.
pub fn surface() -> Value {
    let verbs: Vec<Value> = SURFACE_VERBS
        .iter()
        .map(|c| json!({ "id": *c, "label": *c, "rev": rev(c) }))
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

/// The argument object of a transaction frame.
///
/// `zgui-bridge` carries `txn` (and an undo's `verb`/`args`/`result`) at the TOP level of the frame,
/// while a `call` nests everything under `args`. Accept either shape so a client written against
/// the crate's frames works unchanged here — the hand-mirroring is exactly where the two drift.
fn args_of(req: &Value) -> Value {
    let mut o = req
        .get("args")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for k in ["txn", "verb", "seq", "result"] {
        if let Some(v) = req.get(k) {
            o.entry(k).or_insert_with(|| v.clone());
        }
    }
    Value::Object(o)
}

/// Frame + write one zgui-bridge reply on any writer.
fn reply<W: Write>(w: &mut W, id: u64, ok: bool, value: Value, error: Option<String>) {
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

/// Serve one accepted bus connection given its read + write halves: read zgui-bridge request frames,
/// dispatch, reply. Platform-neutral — the per-platform accept loop supplies the two halves.
pub fn serve_conn<R: BufRead, W: Write>(reader: R, mut w: W) {
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
                let txn = req.get("txn").and_then(Value::as_u64);
                match call_verb(verb, args, txn) {
                    Ok(v) => reply(&mut w, id, true, v, None),
                    Err(e) => reply(&mut w, id, false, Value::Null, Some(e)),
                }
            }
            Some("get") => {
                let state = req.get("state").and_then(Value::as_str).unwrap_or("");
                reply(&mut w, id, true, run_command(state, &json!({})), None);
            }
            Some("verbs") => reply(&mut w, id, true, surface(), None),
            // Event subscriptions are not bridged yet (host pub/sub is process-global and not
            // request/reply). Acknowledge so the client doesn't hang.
            Some("sub") => reply(&mut w, id, true, Value::Null, None),
            // Transactional compensation. `begin`/`commit`/`abort` are host commands so an
            // in-process caller reaches the same journal; `undo` is a browser action, executed by
            // the HUD worker that holds the pre-state.
            Some("begin") => reply(
                &mut w,
                id,
                true,
                run_command("txn_begin", &args_of(&req)),
                None,
            ),
            Some("commit") => reply(
                &mut w,
                id,
                true,
                run_command("txn_commit", &args_of(&req)),
                None,
            ),
            Some("abort") => reply(
                &mut w,
                id,
                true,
                run_command("txn_abort", &args_of(&req)),
                None,
            ),
            Some("undo") => reply(
                &mut w,
                id,
                true,
                run_command("browser.undo", &args_of(&req)),
                None,
            ),
            _ => reply(
                &mut w,
                id,
                false,
                Value::Null,
                Some("unknown request kind".into()),
            ),
        }
    }
}

/* ---- Unix domain socket (macOS / Linux) ---- */
#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    /// The bus socket directory, matching the `zgui-bridge` client: `$XDG_RUNTIME_DIR/zgui`, else
    /// `$TMPDIR/zgui`, else `/tmp/zgui`.
    fn socket_dir() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("TMPDIR").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("zgui")
    }

    /// Serve one accepted connection: clone the stream for the writer, BufRead the reader.
    fn handle_conn(stream: UnixStream) {
        let w = match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        };
        serve_conn(io::BufReader::new(stream), w);
    }

    /// Is a live listener currently accepting on the bus socket?
    fn bus_live(sock: &std::path::Path) -> bool {
        UnixStream::connect(sock).is_ok()
    }

    /// Try to become the bus owner: under an advisory `flock`, if nobody is already listening, bind a
    /// private temp path and atomically `rename` it over `zwire.sock`. Returns the bound listener if WE
    /// took ownership, or `None` if another owner is already live (adopt) or the bind failed.
    ///
    /// The flock SERIALIZES the check-and-bind across every process so two starters can't both bind (the
    /// loser's `remove_file` would otherwise unlink the winner's live socket — last writer wins the path).
    /// The `rename` makes the replace ATOMIC: no window where the path is missing (ENOENT) or points at a
    /// dead socket, and the listener survives the rename on macOS/Linux (verified). Lock auto-releases on
    /// return (drop of `_lock`) or process exit.
    fn bind_bus(dir: &std::path::Path, sock: &std::path::Path) -> Option<UnixListener> {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.join("zwire.sock.lock"))
            .ok();
        if let Some(l) = &lock {
            let _ = l.lock(); // blocks until we hold the exclusive lock
        }
        // Held under the lock, this check is authoritative: nobody can rebind between here and our bind.
        if bus_live(sock) {
            return None;
        }
        let tmp = dir.join(format!("zwire.sock.{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let listener = UnixListener::bind(&tmp).ok()?;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        if std::fs::rename(&tmp, sock).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
        Some(listener)
        // `lock` drops here → flock released. (Ownership of the socket is now the listener's, not the lock's.)
    }

    /// Accept bus connections forever, one thread per connection. Blocks the calling thread.
    fn accept_loop(listener: UnixListener) {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    std::thread::spawn(move || handle_conn(stream));
                }
                Err(_) => break,
            }
        }
    }

    /// Ensure the `App::open("zwire")` bus has a GUARANTEED long-lived owner, then return once it is
    /// reachable. Called by every run mode at startup. See the crate-level `ensure_daemon` doc
    /// for the ownership rationale (the os-error-61 fix).
    pub fn ensure_daemon() {
        let dir = socket_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        let sock = dir.join("zwire.sock");
        if bus_live(&sock) {
            return;
        }
        // Hermetic-test seam: when set, do the live-check but never auto-spawn, so `cargo test` (whose
        // host-spawning helpers set this) never leaves a detached daemon on the developer's real bus.
        // Production never sets it; the `bus-daemon` arm ignores it (it binds directly, not via here).
        if std::env::var_os("ZWIRE_BUS_NO_DAEMON").is_some() {
            return;
        }
        // Spawn the detached singleton daemon. `process_group(0)` puts it in its own group so it survives
        // this host's exit and any signal sent to the host's group; null stdio fully detaches it.
        if let Ok(exe) = std::env::current_exe() {
            let _ = Command::new(exe)
                .arg("bus-daemon")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn();
        }
        // Wait (bounded) until the daemon has bound so immediate callers don't race the socket.
        for _ in 0..200 {
            if bus_live(&sock) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Entry point for the internal `bus-daemon` subcommand: become the bus owner and serve forever. If
    /// another daemon already owns the bus (lost the `flock` race), exit immediately so only ONE lingers.
    pub fn run_daemon() -> ! {
        let dir = socket_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        let sock = dir.join("zwire.sock");
        match bind_bus(&dir, &sock) {
            Some(listener) => {
                accept_loop(listener); // blocks until the listener errors
                std::process::exit(0);
            }
            None => std::process::exit(0), // another daemon owns it, or bind failed
        }
    }
}

/* ---- named pipe (Windows) ---- */
#[cfg(windows)]
mod platform {
    use super::*;
    use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions, Stream};
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    /// `DETACHED_PROCESS`: the daemon gets no console, so it fully outlives this host (the analog of
    /// Unix `process_group(0)` + null stdio). `CREATE_NEW_PROCESS_GROUP`: it ignores CTRL_C/CTRL_BREAK
    /// aimed at this host's group. (Win32 `CreateProcess` dwCreationFlags.)
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    /// The bus pipe leaf name. `to_ns_name::<GenericNamespaced>()` maps `zwire.sock` → the pipe
    /// `\\.\pipe\zwire.sock`, matching the client's per-app leaf (`<app>.sock`). There is no
    /// filesystem directory on Windows — the pipe is a named kernel object, not a path.
    const PIPE_LEAF: &str = "zwire.sock";

    fn pipe_name() -> io::Result<interprocess::local_socket::Name<'static>> {
        PIPE_LEAF.to_ns_name::<GenericNamespaced>()
    }

    /// Serve one accepted connection: split the pipe stream into recv/send halves.
    fn handle_conn(stream: Stream) {
        let (recv, send) = stream.split();
        serve_conn(io::BufReader::new(recv), send);
    }

    /// Is a live listener currently accepting on the bus pipe? A successful connect means an owner
    /// exists (the pipe object is refcounted by the kernel — no stale path to mistake for a live one).
    fn bus_live() -> bool {
        match pipe_name() {
            Ok(name) => Stream::connect(name).is_ok(),
            Err(_) => false,
        }
    }

    /// Accept bus connections forever, one thread per connection. Blocks the calling thread.
    fn accept_loop(listener: interprocess::local_socket::Listener) {
        while let Ok(stream) = listener.accept() {
            std::thread::spawn(move || handle_conn(stream));
        }
    }

    /// Ensure the `App::open("zwire")` bus has a GUARANTEED long-lived owner, then return once it is
    /// reachable. See the crate-level `ensure_daemon` doc for the ownership rationale.
    pub fn ensure_daemon() {
        if bus_live() {
            return;
        }
        // Hermetic-test seam, mirroring the Unix arm: never auto-spawn a detached daemon under test.
        if std::env::var_os("ZWIRE_BUS_NO_DAEMON").is_some() {
            return;
        }
        // Spawn the detached singleton daemon. DETACHED_PROCESS + null stdio + a fresh process group
        // fully sever it from this host, so it outlives us (the analog of the Unix `process_group(0)`).
        if let Ok(exe) = std::env::current_exe() {
            let _ = Command::new(exe)
                .arg("bus-daemon")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
                .spawn();
        }
        // Wait (bounded) until the daemon has bound so immediate callers don't race the pipe.
        for _ in 0..200 {
            if bus_live() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Entry point for the internal `bus-daemon` subcommand: become the bus owner and serve forever.
    ///
    /// The singleton guarantee is the OS itself: interprocess creates the first pipe instance with
    /// `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a SECOND daemon's `create_sync()` fails with
    /// `ERROR_ACCESS_DENIED` while the first owner is live — the direct analog of the Unix `flock`.
    /// The loser exits immediately so only ONE daemon lingers.
    pub fn run_daemon() -> ! {
        let name = match pipe_name() {
            Ok(n) => n,
            Err(_) => std::process::exit(0),
        };
        match ListenerOptions::new().name(name).create_sync() {
            Ok(listener) => {
                accept_loop(listener); // blocks until the listener errors
                std::process::exit(0);
            }
            // `ERROR_ACCESS_DENIED` here = another daemon already owns the pipe (FIRST_PIPE_INSTANCE).
            Err(_) => std::process::exit(0),
        }
    }
}

/* ---- platforms with neither Unix sockets nor named pipes ---- */
#[cfg(not(any(unix, windows)))]
mod platform {
    /// No-op: the automation bus daemon is not available on this platform.
    pub fn ensure_daemon() {}
    /// No-op entry point for the internal `bus-daemon` subcommand.
    pub fn run_daemon() -> ! {
        std::process::exit(0);
    }
}

/// Ensure the `App::open("zwire")` bus has a GUARANTEED long-lived owner, then return once it is
/// reachable. Called by every run mode at startup.
///
/// The bus is NOT owned by whichever host happens to run first — that was the intermittent
/// `Connection refused (os error 61)` bug: Chrome spawns a short-lived host per `sendNativeMessage`,
/// and one of those would bind the bus, get adopted by the persistent `connectNative` hosts, then
/// exit — leaving a stale socket that nobody re-bound. Instead, ownership belongs to a DEDICATED,
/// detached `bus-daemon` singleton (the `ssh-agent`/`tmux` model): its only job is to hold the socket,
/// so it never exits early. Any host merely spawns it if the bus isn't already live; the singleton
/// guard (Unix `flock`, Windows `FILE_FLAG_FIRST_PIPE_INSTANCE`) makes extra spawns adopt-and-exit.
///
/// SYNCHRONOUS: after spawning we spin (bounded ~2s) until the daemon has bound, so a script that
/// `App::open`s immediately afterward (e.g. a `stryke -E` hook this host spawns) never races an
/// unbound endpoint.
pub use platform::ensure_daemon;

/// Entry point for the internal `bus-daemon` subcommand: become the bus owner and serve forever.
pub use platform::run_daemon;
