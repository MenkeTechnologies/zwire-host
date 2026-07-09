//! Lifecycle scripting hooks, executed via the standalone `stryke` interpreter.
//!
//! A hook binds a lifecycle *event* (e.g. `scheme-changed`) to a stryke script.
//! When the event fires, [`fire`] spawns the script (see [`crate::stryke_runner`]),
//! pipes the event envelope `{ event, payload }` as JSON on stdin, and reads a
//! `{ "actions": [ ... ] }` object back on stdout. Recognized actions are
//! validated and dispatched against the host; unknown actions are ignored.
//!
//! Storage: `~/.zwire/hooks/hooks.json` holds metadata, and each hook's body
//! lives next to it as `<id>.st`.
//!
//! Ported from the Audio-Haxor engine (`src-tauri/src/hooks.rs`). The model,
//! manifest format, id/slug scheme, starter-script scaffolding, and stdout
//! action-dispatch logic are carried over verbatim. Re-hosted onto zwire-host's
//! substrate: Tauri commands become plain functions returning `serde_json::Value`
//! (dispatched from `session.rs`), `app.emit` becomes `bus::publish`, and the
//! app-specific action verbs (tag/favorite/trash tied to the Audio-Haxor DB) are
//! replaced by zwire-host's own capabilities — notify/open/exec/pub — dispatched
//! to the existing `osops`/`exec`/`bus` modules rather than reimplemented here.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::stryke_runner::run_script;
use crate::{bus, exec, osops, store};

const DEFAULT_TIMEOUT_MS: u64 = 10_000;

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// One configured hook. The script body lives in a sibling `<id>.st` file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hook {
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// Lifecycle event name this hook listens to.
    pub event: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

// ── paths / persistence ──

/// The hooks config directory (`~/.zwire/hooks`), created on first use. zwire-host
/// has no long-lived per-app state object like Audio-Haxor's `HooksState`, so the
/// small manifest is read from disk per operation instead of RAM-cached.
fn hooks_dir() -> PathBuf {
    let d = store::app_dir("zwire").join("hooks");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("hooks.json")
}

fn script_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.st"))
}

fn load_manifest(dir: &Path) -> Vec<Hook> {
    let p = manifest_path(dir);
    match std::fs::read_to_string(&p) {
        Ok(txt) => serde_json::from_str(&txt).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn save_manifest(dir: &Path, hooks: &[Hook]) -> Result<(), String> {
    let txt = serde_json::to_string_pretty(hooks).map_err(|e| e.to_string())?;
    std::fs::write(manifest_path(dir), txt).map_err(|e| e.to_string())
}

// ── firing ──

/// Fire all enabled hooks bound to `event`, running each script to completion
/// before returning. The hot path (no matching hook) is one small manifest read.
///
/// Runs SYNCHRONOUSLY on purpose: the browser fires events via one-shot
/// `sendNativeMessage`, so Chrome closes the port (and can tear down the host's
/// whole process group) the instant it reads the reply. A background thread —
/// and the `stryke` child it spawned — would be killed before the script ran.
/// Blocking here means the reply is sent only after the hooks have actually run,
/// so the host (and its children) stay alive for the duration.
pub fn fire(event: &str, payload: Value) {
    let dir = hooks_dir();
    let matching: Vec<Hook> = load_manifest(&dir)
        .into_iter()
        .filter(|h| h.enabled && h.event == event)
        .collect();
    if matching.is_empty() {
        return;
    }
    let envelope = json!({ "event": event, "payload": payload }).to_string();
    for h in matching {
        let sp = script_path(&dir, &h.id);
        if !sp.is_file() {
            continue;
        }
        match run_script(&sp, event, &envelope, Duration::from_millis(h.timeout_ms)) {
            Ok(out) => {
                let done = dispatch_stdout(&out.stdout);
                bus::publish(
                    "hook-result",
                    &json!({
                        "id": h.id,
                        "event": event,
                        "ok": !out.timed_out,
                        "timedOut": out.timed_out,
                        "code": out.code,
                        "actions": done,
                        "stderr": truncate(&out.stderr, 4000),
                    }),
                );
            }
            Err(e) => {
                bus::publish(
                    "hook-result",
                    &json!({ "id": h.id, "event": event, "ok": false, "error": e }),
                );
            }
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

// ── actions ──

/// The action verbs a hook script may emit. Externally tagged: each array element
/// looks like `{ "notify": { "title": "..", "body": ".." } }`. Where Audio-Haxor's
/// verbs drove its media DB (tag/favorite/trash), zwire's map onto the host's own
/// capabilities so the dispatch reuses existing `osops`/`exec`/`bus` code.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    /// Desktop notification (via `osops::notify`).
    Notify {
        title: String,
        #[serde(default)]
        body: String,
    },
    /// Open a path/URL with the OS default handler (via `osops::open`).
    Open {
        target: String,
    },
    /// Run a subprocess (via `exec::run`).
    Exec {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// Publish an event on the host pub/sub bus (via `bus::publish`).
    Pub {
        topic: String,
        #[serde(default)]
        data: Value,
    },
}

/// Parse the script's stdout and dispatch every recognized action. Tolerates a
/// trailing JSON line among other output. Returns the count dispatched.
fn dispatch_stdout(stdout: &str) -> usize {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return 0;
    }
    let parsed = serde_json::from_str::<Value>(trimmed)
        .ok()
        .filter(|v| v.get("actions").is_some())
        .or_else(|| last_json_line(trimmed));
    let Some(v) = parsed else {
        return 0;
    };
    let Some(arr) = v.get("actions").and_then(|a| a.as_array()) else {
        return 0;
    };
    let mut done = 0;
    for a in arr {
        if let Ok(act) = serde_json::from_value::<Action>(a.clone()) {
            if dispatch_action(act).is_ok() {
                done += 1;
            }
        }
    }
    done
}

/// Scan from the end for a line that is a JSON object carrying `actions`.
fn last_json_line(s: &str) -> Option<Value> {
    for line in s.lines().rev() {
        let t = line.trim();
        if t.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<Value>(t) {
                if v.get("actions").is_some() {
                    return Some(v);
                }
            }
        }
    }
    None
}

fn dispatch_action(action: Action) -> Result<(), String> {
    let ok = |v: Value| -> bool { v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) };
    match action {
        Action::Notify { title, body } => {
            osops::handle("notify", &json!({ "title": title, "body": body }));
            Ok(())
        }
        Action::Open { target } => {
            if ok(osops::handle("open", &json!({ "target": target }))) {
                Ok(())
            } else {
                Err("open failed".into())
            }
        }
        Action::Exec { program, args } => {
            if ok(exec::run(&json!({ "program": program, "args": args }))) {
                Ok(())
            } else {
                Err("exec failed".into())
            }
        }
        Action::Pub { topic, data } => {
            bus::publish(&topic, &data);
            Ok(())
        }
    }
}

// ── id + starter script ──

fn gen_id(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "hook" } else { slug };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{slug}-{nanos:x}")
}

fn default_script(event: &str) -> String {
    // Idiomatic stryke (see strykelang docs/STYLE_GUIDE.md): `<>` reads stdin,
    // `|>` pipelines, `val` bindings, `p` prints — not Perl 5 `my`/`print`/slurp.
    format!(
        "# zwire hook for event: {event}\n\
         # Envelope arrives as one JSON line on STDIN: {{ event, payload }}\n\
         # Print {{ actions: [ ... ] }} on STDOUT to act on the host.\n\
         # Actions: notify, open, exec, pub.\n\n\
         val $in = <> |> from_json\n\
         val $out = {{ actions => [{{ notify => {{ title => \"Hook: $in->{{event}}\", body => $in->{{payload}} |> to_json }} }}] }}\n\
         $out |> to_json |> p\n"
    )
}

// ── command handlers (dispatched from session.rs) ──

/// `hooks_list` → `{ ok, hooks: [Hook] }`.
pub fn list() -> Value {
    json!({ "ok": true, "hooks": load_manifest(&hooks_dir()) })
}

/// `hooks_events` → the catalog of lifecycle events + valid action verbs.
pub fn events() -> Value {
    json!({
        "ok": true,
        "events": [
            // ── catch-all ──
            { "name": "action", "desc": "ANY command/menu/palette/toolbar invocation — bind one hook and filter by $_.command", "sample": { "command": "palette:New tab" } },
            // ── runtime / browser lifecycle ──
            { "name": "app-open", "desc": "The browser launched (cold start)", "sample": Value::Null },
            { "name": "app-close", "desc": "The last window closed — the browser is quitting", "sample": Value::Null },
            { "name": "host-ready", "desc": "The native host started", "sample": Value::Null },
            { "name": "extension-installed", "desc": "The HUD extension was installed or updated", "sample": { "reason": "update" } },
            { "name": "browser-suspend", "desc": "The extension worker is suspending", "sample": Value::Null },
            { "name": "update-available", "desc": "A HUD extension update is ready", "sample": { "version": "0.6.0" } },
            // ── tabs ──
            { "name": "tab-created", "desc": "A tab was opened", "sample": { "tabId": 12, "url": "about:blank", "windowId": 1 } },
            { "name": "tab-closed", "desc": "A tab was closed", "sample": { "tabId": 12, "windowId": 1, "windowClosing": false } },
            { "name": "tab-activated", "desc": "The active tab changed", "sample": { "tabId": 12, "windowId": 1 } },
            { "name": "tab-updated", "desc": "A tab finished loading", "sample": { "tabId": 12, "url": "https://example.com", "status": "complete" } },
            { "name": "tab-moved", "desc": "A tab was moved within its window", "sample": { "tabId": 12, "windowId": 1, "fromIndex": 0, "toIndex": 2 } },
            { "name": "tab-detached", "desc": "A tab was pulled out of its window", "sample": { "tabId": 12, "oldWindowId": 1, "oldPosition": 2 } },
            { "name": "tab-attached", "desc": "A tab was docked into a window", "sample": { "tabId": 12, "newWindowId": 3, "newPosition": 0 } },
            { "name": "tab-replaced", "desc": "A tab was replaced (prerender/instant)", "sample": { "addedTabId": 13, "removedTabId": 12 } },
            { "name": "tab-highlighted", "desc": "The set of highlighted tabs changed", "sample": { "windowId": 1, "tabIds": [12, 13] } },
            { "name": "tab-zoom-changed", "desc": "A tab's zoom level changed", "sample": { "tabId": 12, "newZoom": 1.25, "oldZoom": 1.0 } },
            // ── windows ──
            { "name": "window-created", "desc": "A browser window opened", "sample": { "windowId": 1, "type": "normal", "incognito": false } },
            { "name": "window-closed", "desc": "A browser window closed", "sample": { "windowId": 1 } },
            { "name": "window-focus-changed", "desc": "Window focus changed (-1 = none)", "sample": { "windowId": 1 } },
            // ── navigation (top frame) ──
            { "name": "navigation-started", "desc": "A navigation is about to begin", "sample": { "tabId": 12, "url": "https://example.com" } },
            { "name": "navigation", "desc": "A tab navigated to a URL (committed)", "sample": { "tabId": 12, "url": "https://example.com", "transition": "link" } },
            { "name": "dom-content-loaded", "desc": "The page DOM finished parsing", "sample": { "tabId": 12, "url": "https://example.com" } },
            { "name": "navigation-completed", "desc": "A tab finished navigating", "sample": { "tabId": 12, "url": "https://example.com" } },
            { "name": "navigation-error", "desc": "A navigation failed", "sample": { "tabId": 12, "url": "https://example.com", "error": "net::ERR_ABORTED" } },
            { "name": "history-state-updated", "desc": "An in-page (SPA) history navigation happened", "sample": { "tabId": 12, "url": "https://example.com/route" } },
            // ── downloads ──
            { "name": "download-started", "desc": "A download began", "sample": { "id": 1, "url": "https://example.com/f.zip", "filename": "f.zip" } },
            { "name": "download-completed", "desc": "A download completed", "sample": { "id": 1 } },
            { "name": "download-erased", "desc": "A download was erased from history", "sample": { "id": 1 } },
            // ── bookmarks ──
            { "name": "bookmark-created", "desc": "A bookmark was added", "sample": { "id": "42", "title": "zwire", "url": "https://example.com" } },
            { "name": "bookmark-removed", "desc": "A bookmark was removed", "sample": { "id": "42" } },
            { "name": "bookmark-changed", "desc": "A bookmark's title/url changed", "sample": { "id": "42", "title": "zwire", "url": "https://example.com" } },
            { "name": "bookmark-moved", "desc": "A bookmark was moved", "sample": { "id": "42" } },
            // ── history ──
            { "name": "history-visited", "desc": "A URL was recorded in history", "sample": { "url": "https://example.com", "title": "Example" } },
            { "name": "history-removed", "desc": "History entries were removed", "sample": { "allHistory": false, "urls": ["https://example.com"] } },
            // ── sessions ──
            { "name": "session-restored", "desc": "A closed tab/window was restored", "sample": Value::Null },
            // ── management (other extensions) ──
            { "name": "management-installed", "desc": "Another extension was installed", "sample": { "id": "abcd…", "name": "Some Extension" } },
            { "name": "management-uninstalled", "desc": "Another extension was uninstalled", "sample": { "id": "abcd…" } },
            { "name": "management-enabled", "desc": "Another extension was enabled", "sample": { "id": "abcd…", "name": "Some Extension" } },
            { "name": "management-disabled", "desc": "Another extension was disabled", "sample": { "id": "abcd…", "name": "Some Extension" } },
            // ── input / system ──
            { "name": "command", "desc": "A registered keyboard command fired", "sample": { "command": "open-palette" } },
            { "name": "alarm", "desc": "A scheduled alarm fired", "sample": { "name": "hourly" } },
            { "name": "notification-clicked", "desc": "A HUD notification was clicked", "sample": { "id": "note-1" } },
            { "name": "notification-closed", "desc": "A HUD notification was dismissed", "sample": { "id": "note-1", "byUser": true } },
            { "name": "action-clicked", "desc": "The toolbar action icon was clicked", "sample": { "tabId": 12 } },
            { "name": "display-changed", "desc": "A monitor was added/removed/reconfigured", "sample": Value::Null },
            // ── HUD lifecycle ──
            { "name": "terminal-opened", "desc": "The HUD terminal overlay opened in a tab", "sample": { "tabId": 12 } },
            { "name": "terminal-closed", "desc": "The HUD terminal overlay closed in a tab", "sample": { "tabId": 12 } },
            { "name": "scheme-changed", "desc": "The HUD color scheme changed", "sample": { "scheme": "matrix" } },
            { "name": "palette-command", "desc": "A ⌘K palette command ran", "sample": { "command": "New tab" } },
            { "name": "session-saved", "desc": "A tmux/session snapshot was saved", "sample": { "count": 3 } },
            { "name": "pane-split", "desc": "A tmux pane was split", "sample": { "dir": "h", "paneId": 7 } },
            { "name": "audio-eq-changed", "desc": "The browser-wide audio engine config changed", "sample": { "spec": "0.0;gain,1.2" } }
        ],
        "actions": ["notify", "open", "exec", "pub"]
    })
}

/// `hooks_save` — create or update a hook; scaffolds a default script for a new one.
pub fn save(msg: &Value) -> Value {
    let dir = hooks_dir();
    let mut hook: Hook = match serde_json::from_value(msg["hook"].clone()) {
        Ok(h) => h,
        Err(e) => return json!({ "ok": false, "err": format!("bad hook: {e}") }),
    };
    if hook.id.trim().is_empty() {
        hook.id = gen_id(&hook.name);
    }
    if hook.timeout_ms == 0 {
        hook.timeout_ms = DEFAULT_TIMEOUT_MS;
    }
    let mut list = load_manifest(&dir);
    if let Some(existing) = list.iter_mut().find(|h| h.id == hook.id) {
        *existing = hook.clone();
    } else {
        let sp = script_path(&dir, &hook.id);
        if !sp.exists() {
            let _ = std::fs::write(&sp, default_script(&hook.event));
        }
        list.push(hook.clone());
    }
    match save_manifest(&dir, &list) {
        Ok(()) => json!({ "ok": true, "hook": hook }),
        Err(e) => json!({ "ok": false, "err": e }),
    }
}

/// `hooks_delete` — drop a hook and its script file.
pub fn delete(msg: &Value) -> Value {
    let dir = hooks_dir();
    let id = msg["id"].as_str().unwrap_or("");
    let mut list = load_manifest(&dir);
    list.retain(|h| h.id != id);
    if let Err(e) = save_manifest(&dir, &list) {
        return json!({ "ok": false, "err": e });
    }
    let _ = std::fs::remove_file(script_path(&dir, id));
    json!({ "ok": true })
}

/// `hooks_set_enabled` — toggle a hook on/off.
pub fn set_enabled(msg: &Value) -> Value {
    let dir = hooks_dir();
    let id = msg["id"].as_str().unwrap_or("");
    let enabled = msg["enabled"].as_bool().unwrap_or(false);
    let mut list = load_manifest(&dir);
    if let Some(h) = list.iter_mut().find(|h| h.id == id) {
        h.enabled = enabled;
    }
    match save_manifest(&dir, &list) {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "ok": false, "err": e }),
    }
}

/// `hooks_get_script` — the stryke source of a hook.
pub fn get_script(msg: &Value) -> Value {
    let dir = hooks_dir();
    let id = msg["id"].as_str().unwrap_or("");
    let code = std::fs::read_to_string(script_path(&dir, id)).unwrap_or_default();
    json!({ "ok": true, "code": code })
}

/// `hooks_script_path` — absolute path to a hook's script (for the LSP `file://` URI).
pub fn script_path_of(msg: &Value) -> Value {
    let dir = hooks_dir();
    let id = msg["id"].as_str().unwrap_or("");
    json!({ "ok": true, "path": script_path(&dir, id).to_string_lossy() })
}

/// `hooks_set_script` — write a hook's stryke source.
pub fn set_script(msg: &Value) -> Value {
    let dir = hooks_dir();
    let id = msg["id"].as_str().unwrap_or("");
    let code = msg["code"].as_str().unwrap_or("");
    match std::fs::write(script_path(&dir, id), code) {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "ok": false, "err": e.to_string() }),
    }
}

/// `hooks_test_run` — dry run: execute with a sample payload and return
/// stdout/stderr plus the *parsed* actions, but do NOT dispatch them.
pub fn test_run(msg: &Value) -> Value {
    let dir = hooks_dir();
    let id = msg["id"].as_str().unwrap_or("");
    let list = load_manifest(&dir);
    let Some(h) = list.iter().find(|h| h.id == id) else {
        return json!({ "ok": false, "err": "hook not found" });
    };
    let sp = script_path(&dir, id);
    if !sp.is_file() {
        return json!({ "ok": false, "err": "script file missing" });
    }
    let payload: Value = match msg["sample"].as_str() {
        Some(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        None => msg["sample"].clone(),
    };
    let envelope = json!({ "event": h.event, "payload": payload }).to_string();
    match run_script(&sp, &h.event, &envelope, Duration::from_millis(h.timeout_ms)) {
        Ok(out) => json!({
            "ok": true,
            "stdout": out.stdout,
            "stderr": out.stderr,
            "code": out.code,
            "timedOut": out.timed_out,
            "actions": parse_actions_preview(&out.stdout),
        }),
        Err(e) => json!({ "ok": false, "err": e }),
    }
}

/// `hook_fire` — the browser reports a lifecycle event; run matching hooks.
pub fn fire_cmd(msg: &Value) -> Value {
    let Some(event) = msg["event"].as_str() else {
        return json!({ "ok": false, "err": "no_event" });
    };
    fire(event, msg["payload"].clone());
    json!({ "ok": true, "fired": event })
}

fn parse_actions_preview(stdout: &str) -> Vec<Value> {
    let trimmed = stdout.trim();
    let parsed = serde_json::from_str::<Value>(trimmed)
        .ok()
        .filter(|v| v.get("actions").is_some())
        .or_else(|| last_json_line(trimmed));
    let Some(v) = parsed else {
        return Vec::new();
    };
    let Some(arr) = v.get("actions").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter(|a| serde_json::from_value::<Action>((*a).clone()).is_ok())
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_actions() {
        let v: Vec<Value> = serde_json::from_str(
            r#"[{"notify":{"title":"t","body":"b"}},{"exec":{"program":"echo","args":["hi"]}}]"#,
        )
        .unwrap();
        let ok = v
            .iter()
            .filter(|a| serde_json::from_value::<Action>((*a).clone()).is_ok())
            .count();
        assert_eq!(ok, 2);
    }

    #[test]
    fn skips_unknown_actions() {
        let preview = parse_actions_preview(
            r#"{"actions":[{"notify":{"title":"t"}},{"frobnicate":{"x":1}}]}"#,
        );
        assert_eq!(preview.len(), 1);
    }

    #[test]
    fn open_requires_target() {
        // `open` with no target must fail to deserialize (missing field).
        assert!(serde_json::from_value::<Action>(json!({"open": {}})).is_err());
    }

    #[test]
    fn malformed_stdout_yields_no_actions() {
        assert!(parse_actions_preview("not json at all").is_empty());
        assert!(parse_actions_preview("").is_empty());
    }

    #[test]
    fn finds_trailing_json_line() {
        let out = "log line one\nlog line two\n{\"actions\":[{\"notify\":{\"title\":\"t\",\"body\":\"b\"}}]}";
        assert_eq!(parse_actions_preview(out).len(), 1);
    }

    #[test]
    fn gen_id_slugifies_and_is_unique_ish() {
        let a = gen_id("My Hook!");
        assert!(a.starts_with("my-hook-"));
        let b = gen_id("");
        assert!(b.starts_with("hook-"));
    }

    #[test]
    fn pub_action_defaults_data_null() {
        let a: Action = serde_json::from_value(json!({"pub": {"topic": "t"}})).unwrap();
        match a {
            Action::Pub { data, .. } => assert!(data.is_null()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn exec_action_roundtrips_with_args() {
        let a: Action =
            serde_json::from_value(json!({"exec": {"program": "echo", "args": ["a", "b"]}}))
                .unwrap();
        match a {
            Action::Exec { program, args } => {
                assert_eq!(program, "echo");
                assert_eq!(args, vec!["a".to_string(), "b".to_string()]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn exec_action_args_default_empty() {
        let a: Action = serde_json::from_value(json!({"exec": {"program": "ls"}})).unwrap();
        match a {
            Action::Exec { args, .. } => assert!(args.is_empty()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn default_script_names_the_event_and_emits_actions() {
        let s = default_script("scheme-changed");
        assert!(s.contains("scheme-changed"), "script names the event: {s}");
        assert!(s.contains("actions"), "script emits an actions object: {s}");
    }

    #[test]
    fn last_json_line_skips_non_actions_objects() {
        // A JSON object without `actions` must NOT be picked, even though it
        // parses — the scan is for the *actions-carrying* trailing line, so
        // structured log lines above it can't be mistaken for the result.
        let out = "{\"log\":\"warming up\"}\n{\"metric\":42}\n{\"actions\":[{\"pub\":{\"topic\":\"t\"}}]}";
        let found = last_json_line(out).expect("actions line found");
        assert_eq!(
            found["actions"].as_array().map(|a| a.len()),
            Some(1),
            "picked the actions line, not the noise: {found}"
        );
    }

    #[test]
    fn last_json_line_none_when_no_actions_present() {
        // Pure log noise with a JSON object that lacks `actions` yields None.
        assert!(last_json_line("plain log\n{\"ok\":true}\ntrailing").is_none());
    }

    #[test]
    fn dispatch_stdout_counts_actions_among_log_noise() {
        // `pub` actions dispatch through the in-process bus, which is a no-op
        // with no subscribers — so this exercises the count without any real
        // OS side effect. Two valid actions on a trailing line among log lines
        // must both be counted.
        let out = "info: hook fired\ndebug: building actions\n\
                   {\"actions\":[{\"pub\":{\"topic\":\"a\",\"data\":1}},{\"pub\":{\"topic\":\"b\"}}]}";
        assert_eq!(dispatch_stdout(out), 2, "both pub actions dispatched");
    }

    #[test]
    fn dispatch_stdout_ignores_unknown_verbs_in_count() {
        // A bare, whole-stdout actions object with one known + one unknown verb
        // counts only the recognized `pub`. Unknown verbs fail to deserialize
        // and are silently skipped, never dispatched.
        let out = "{\"actions\":[{\"pub\":{\"topic\":\"t\"}},{\"frobnicate\":{\"x\":1}}]}";
        assert_eq!(dispatch_stdout(out), 1, "only the known verb counts");
    }

    #[test]
    fn dispatch_stdout_zero_for_empty_or_actionless() {
        assert_eq!(dispatch_stdout(""), 0);
        assert_eq!(dispatch_stdout("   \n  "), 0);
        assert_eq!(dispatch_stdout("not json at all"), 0);
        assert_eq!(dispatch_stdout("{\"actions\":[]}"), 0);
    }
}
