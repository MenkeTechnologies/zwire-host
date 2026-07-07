//! High-level, in-process helpers for crates that embed this one as a library.
//!
//! Sibling hosts such as `zpwrchrome-host` depend on `zwire-host` to **crawl the
//! filesystem** and **run commands** without speaking the wire protocol or
//! standing up a daemon — they just call these functions and get native Rust
//! values back:
//!
//! ```no_run
//! // in zpwrchrome-host:
//! for entry in zwire_host::api::walk("~/src", Some("rs")) {
//!     println!("{}", entry.path.display());
//! }
//! let out = zwire_host::api::exec("git", ["status", "--porcelain"]).unwrap();
//! println!("git exited {:?}:\n{}", out.code, String::from_utf8_lossy(&out.stdout));
//! ```
//!
//! Everything here is a thin, allocation-light wrapper over the same capability
//! functions the transports call, so behaviour is identical in-process and over
//! the socket.
use crate::proto::b64_decode;
use serde_json::{json, Value};
use std::path::PathBuf;

/// Captured result of [`exec`].
#[derive(Debug, Clone)]
pub struct ExecOutput {
    /// Process exit code, or `None` if it was killed by a signal.
    pub code: Option<i64>,
    /// Raw stdout bytes.
    pub stdout: Vec<u8>,
    /// Raw stderr bytes.
    pub stderr: Vec<u8>,
}

impl ExecOutput {
    /// stdout decoded lossily as UTF-8, trailing newline trimmed.
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim_end().to_string()
    }
}

/// Run `program args...` to completion in-process and capture its output.
/// Errors carry the failure reason (e.g. the program was not found).
pub fn exec<I, S>(program: &str, args: I) -> Result<ExecOutput, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    let v = crate::exec::run(&json!({ "program": program, "args": args }));
    if v["ok"] != json!(true) {
        return Err(v["err"].as_str().unwrap_or("exec_failed").to_string());
    }
    Ok(ExecOutput {
        code: v["code"].as_i64(),
        stdout: v["stdout"]
            .as_str()
            .and_then(b64_decode)
            .unwrap_or_default(),
        stderr: v["stderr"]
            .as_str()
            .and_then(b64_decode)
            .unwrap_or_default(),
    })
}

/// Run `program` with `input` fed to its stdin.
pub fn exec_stdin<I, S>(program: &str, args: I, input: &str) -> Result<ExecOutput, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    let v = crate::exec::run(&json!({ "program": program, "args": args, "stdin": input }));
    if v["ok"] != json!(true) {
        return Err(v["err"].as_str().unwrap_or("exec_failed").to_string());
    }
    Ok(ExecOutput {
        code: v["code"].as_i64(),
        stdout: v["stdout"]
            .as_str()
            .and_then(b64_decode)
            .unwrap_or_default(),
        stderr: v["stderr"]
            .as_str()
            .and_then(b64_decode)
            .unwrap_or_default(),
    })
}

/// One node found by [`walk`].
#[derive(Debug, Clone)]
pub struct Entry {
    /// Absolute path to the entry.
    pub path: PathBuf,
    /// Leaf file name.
    pub name: String,
    /// Whether the entry is a directory.
    pub dir: bool,
    /// File size in bytes (0 for directories / on stat failure).
    pub size: u64,
}

/// Recursively crawl `root` (a `~`-expandable path). `ext`, when given, keeps
/// only files with that extension (no leading dot). Capped internally so a crawl
/// of a huge tree can't run away.
pub fn walk(root: &str, ext: Option<&str>) -> Vec<Entry> {
    let mut req = json!({ "path": root });
    if let Some(x) = ext {
        req["ext"] = json!(x);
    }
    entries_from(crate::fsops::handle("fs_walk", &req))
}

/// Like [`walk`] but with the full filter set: `depth`, `dirs_only`, `ext`,
/// `contains` (substring match on the leaf name).
pub fn walk_filtered(
    root: &str,
    depth: Option<usize>,
    dirs_only: bool,
    ext: Option<&str>,
    contains: Option<&str>,
) -> Vec<Entry> {
    let mut req = json!({ "path": root, "dirs_only": dirs_only });
    if let Some(d) = depth {
        req["depth"] = json!(d);
    }
    if let Some(x) = ext {
        req["ext"] = json!(x);
    }
    if let Some(c) = contains {
        req["contains"] = json!(c);
    }
    entries_from(crate::fsops::handle("fs_walk", &req))
}

fn entries_from(v: Value) -> Vec<Entry> {
    v["entries"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|e| Entry {
                    path: PathBuf::from(e["path"].as_str().unwrap_or("")),
                    name: e["name"].as_str().unwrap_or("").to_string(),
                    dir: e["dir"].as_bool().unwrap_or(false),
                    size: e["size"].as_u64().unwrap_or(0),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Read a whole file (`~`-expandable path) into bytes.
pub fn read_file(path: &str) -> Result<Vec<u8>, String> {
    let v = crate::fsops::handle("fs_read", &json!({ "path": path }));
    if v["ok"] != json!(true) {
        return Err(v["err"].as_str().unwrap_or("read_failed").to_string());
    }
    Ok(v["b64"].as_str().and_then(b64_decode).unwrap_or_default())
}

/// Write bytes to a file (`~`-expandable path), creating parent dirs.
pub fn write_file(path: &str, bytes: &[u8]) -> Result<(), String> {
    let v = crate::fsops::handle(
        "fs_write",
        &json!({ "path": path, "b64": crate::proto::b64_encode(bytes) }),
    );
    if v["ok"] == json!(true) {
        Ok(())
    } else {
        Err(v["err"].as_str().unwrap_or("write_failed").to_string())
    }
}

// ─────────────────────────── shared fleet theme ───────────────────────────
// Helpers for an app backend (Tauri/JUCE) to sync colour scheme + light/fx with
// the fleet-wide theme (~/.zwire/global.toml). Read + write are in-process store
// ops; `theme_watch` bridges live changes (from THIS or ANY app) to a callback,
// which a backend forwards to its webview as a `theme-changed` event. Pairs with
// zgui-core's `ZGui.themeSync` on the frontend.

/// The current shared theme as `(scheme, ui)`, where `ui` is the light/fx object.
pub fn theme_get() -> (String, Value) {
    let d = crate::store::theme_dir();
    (crate::store::current_scheme(&d), crate::store::current_ui(&d))
}

/// Set the shared colour scheme: persist to `global.toml` (+ the `hud-scheme`
/// projection) and notify every local subscriber + peer so the fleet follows.
pub fn theme_set_scheme(scheme: &str) {
    let d = crate::store::theme_dir();
    crate::store::write_scheme(&d, scheme);
    crate::theme_watch::note_scheme(scheme);
    let data = json!({ "scheme": scheme });
    crate::bus::publish("scheme", &data);
    crate::peer::broadcast("scheme", &data);
}

/// Merge a partial light/fx object (e.g. `{"light":true}`) into the shared ui and
/// notify subscribers + peers. Returns the merged ui.
pub fn theme_set_ui(partial: &Value) -> Value {
    let d = crate::store::theme_dir();
    let ui = crate::store::write_ui(&d, partial);
    crate::theme_watch::note_ui(&ui);
    crate::bus::publish("ui", &ui);
    crate::peer::broadcast("ui", &ui);
    ui
}

/// Watch the shared theme for changes and invoke `on_change(scheme, ui)` — once
/// immediately (so the caller converges to the current value) and thereafter
/// whenever `global.toml` changes, from this app or any other. Spawns a
/// background thread and returns at once. An app backend uses this to push live
/// theme updates into its UI (emit a Tauri/JUCE `theme-changed` event).
pub fn theme_watch<F>(on_change: F)
where
    F: Fn(String, Value) + Send + 'static,
{
    std::thread::spawn(move || {
        let d = crate::store::theme_dir();
        let mut last = (crate::store::current_scheme(&d), crate::store::current_ui(&d));
        on_change(last.0.clone(), last.1.clone());
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let cur = (crate::store::current_scheme(&d), crate::store::current_ui(&d));
            if cur != last {
                last = cur.clone();
                on_change(cur.0, cur.1);
            }
        }
    });
}
