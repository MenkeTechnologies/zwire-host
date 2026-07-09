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

/// Fire all enabled hooks bound to `event`. Returns immediately; scripts run on a
/// background thread. The hot path (no matching hook) is one small manifest read.
pub fn fire(event: &str, payload: Value) {
    let dir = hooks_dir();
    let matching: Vec<Hook> = load_manifest(&dir)
        .into_iter()
        .filter(|h| h.enabled && h.event == event)
        .collect();
    if matching.is_empty() {
        return;
    }
    let event = event.to_string();
    std::thread::spawn(move || {
        let envelope = json!({ "event": event, "payload": payload }).to_string();
        for h in matching {
            let sp = script_path(&dir, &h.id);
            if !sp.is_file() {
                continue;
            }
            match run_script(&sp, &event, &envelope, Duration::from_millis(h.timeout_ms)) {
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
    });
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
            { "name": "host-ready", "desc": "The native host started", "sample": Value::Null },
            { "name": "navigation", "desc": "A tab navigated to a URL", "sample": { "url": "https://example.com", "tabId": 12 } },
            { "name": "tab-created", "desc": "A tab was opened", "sample": { "tabId": 12, "url": "about:blank" } },
            { "name": "tab-closed", "desc": "A tab was closed", "sample": { "tabId": 12 } },
            { "name": "scheme-changed", "desc": "The HUD color scheme changed", "sample": { "scheme": "matrix" } },
            { "name": "palette-command", "desc": "A ⌘K palette command ran", "sample": { "command": "open-devtools" } },
            { "name": "session-saved", "desc": "A tmux/session snapshot was saved", "sample": { "name": "work" } },
            { "name": "pane-split", "desc": "A tmux pane was split", "sample": { "dir": "h" } },
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
}
