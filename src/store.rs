//! Per-app persistent state under the OS application-data directory.
//!
//! The base dir matches the C++ HUD colour patch (`base::DIR_APP_DATA`) and
//! `scripts/state-dir.sh`, so the `hud-scheme` the host writes is the exact file
//! the compiled colour mixer reads — no split-brain across two locations:
//!   * macOS   `~/Library/Application Support/zwire`
//!   * Windows `%APPDATA%\zwire`
//!   * other   `${XDG_CONFIG_HOME:-~/.config}/zwire`
//! `$ZWIRE_STATE` overrides the whole path (same contract as the launcher).
//!
//! Two layers live here:
//!   * a generic namespaced key/value store (`kv_*`) any app can use, at
//!     `<state>/kv/<key>.json`;
//!   * the zwire scheme + UI files (`hud-scheme`, `hud-ui.json`) that the HUD
//!     protocol reads and writes.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Colour schemes the zwire HUD accepts. Writes of anything else are rejected so
/// a typo can't poison the file the compiled colour mixer reads.
pub const SCHEMES: &[&str] = &[
    "cyberpunk",
    "midnight",
    "matrix",
    "ember",
    "arctic",
    "crimson",
    "toxic",
    "vapor",
];

/// The user's home directory, falling back to `.` so we never panic on a
/// stripped environment. `USERPROFILE` covers Windows.
pub fn home() -> PathBuf {
    let h = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(h)
}

/// Expand a leading `~` / `~/` to the home directory; otherwise pass through.
pub fn expand(p: &str) -> PathBuf {
    if p == "~" {
        return home();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(p)
}

/// Restrict a caller-supplied name to a safe filesystem token (alnum plus
/// `-`, `_`, `.`), so an `app`/`key` can never escape its directory.
fn sanitize(name: &str, fallback: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    let s = s.trim_matches('.').to_string();
    if s.is_empty() {
        fallback.to_string()
    } else {
        s
    }
}

/// Platform base directory for persistent app state, mirroring the C++ HUD
/// patch (`base::DIR_APP_DATA`) and `scripts/state-dir.sh`:
///   * macOS   `~/Library/Application Support`
///   * Windows `%APPDATA%` (Roaming)
///   * other   `${XDG_CONFIG_HOME:-~/.config}`
fn state_base() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home().join("Library").join("Application Support")
    }
    #[cfg(windows)]
    {
        std::env::var("APPDATA")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData").join("Roaming"))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        std::env::var("XDG_CONFIG_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"))
    }
}

/// The base directory for an app's state, created on demand. `app` empty or
/// missing resolves to `zwire`. For the `zwire` app, `$ZWIRE_STATE` overrides
/// the whole path (the launcher/native-host contract), keeping the host, the
/// C++ colour mixer, and the shell scripts pointed at one directory.
pub fn app_dir(app: &str) -> PathBuf {
    let name = sanitize(app, "zwire");
    let d = match std::env::var("ZWIRE_STATE") {
        Ok(s) if !s.is_empty() && name == "zwire" => PathBuf::from(s),
        _ => state_base().join(&name),
    };
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Atomically replace a file: write a sibling `.tmp` then rename over the target
/// so a reader never observes a half-written file.
fn write_atomic(path: &Path, bytes: &[u8]) -> bool {
    let tmp = path.with_extension("tmp");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&tmp, bytes).is_ok() {
        return std::fs::rename(&tmp, path).is_ok();
    }
    false
}

/* ---- generic key/value store ---- */

fn kv_path(app: &str, key: &str) -> PathBuf {
    app_dir(app)
        .join("kv")
        .join(format!("{}.json", sanitize(key, "default")))
}

/// Read a stored value, or `null` when the key is absent or unreadable.
pub fn kv_get(app: &str, key: &str) -> Value {
    std::fs::read_to_string(kv_path(app, key))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

/// Replace a key's value wholesale.
pub fn kv_set(app: &str, key: &str, value: &Value) -> bool {
    serde_json::to_vec(value)
        .map(|b| write_atomic(&kv_path(app, key), &b))
        .unwrap_or(false)
}

/// Shallow-merge an object into a key (top-level keys only); returns the merged
/// value. Non-object stored values are overwritten by `partial`.
pub fn kv_merge(app: &str, key: &str, partial: &Value) -> Value {
    let mut cur = kv_get(app, key);
    match (cur.as_object_mut(), partial.as_object()) {
        (Some(c), Some(p)) => {
            for (k, v) in p {
                c.insert(k.clone(), v.clone());
            }
        }
        _ => cur = partial.clone(),
    }
    kv_set(app, key, &cur);
    cur
}

/// Delete a key. Returns `true` if the file is gone afterwards (including if it
/// never existed).
pub fn kv_del(app: &str, key: &str) -> bool {
    let p = kv_path(app, key);
    !p.exists() || std::fs::remove_file(&p).is_ok()
}

/// List the keys stored for an app.
pub fn kv_keys(app: &str) -> Vec<String> {
    let dir = app_dir(app).join("kv");
    let mut keys = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(k) = name.strip_suffix(".json") {
                keys.push(k.to_string());
            }
        }
    }
    keys.sort();
    keys
}

/* ---- legacy zwire scheme + ui ---- */

/// Current HUD scheme (defaults to `cyberpunk`).
pub fn current_scheme(d: &Path) -> String {
    std::fs::read_to_string(d.join("hud-scheme"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "cyberpunk".into())
}

/// Persist the HUD scheme (caller validates against [`SCHEMES`]).
pub fn write_scheme(d: &Path, s: &str) {
    write_atomic(&d.join("hud-scheme"), format!("{s}\n").as_bytes());
}

/// Current HUD UI-preference object (empty object when unset).
pub fn current_ui(d: &Path) -> Value {
    std::fs::read_to_string(d.join("hud-ui.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

/// Shallow-merge `partial` into the HUD UI object and persist it; returns the
/// merged object.
pub fn write_ui(d: &Path, partial: &Value) -> Value {
    let mut cur = current_ui(d);
    if let (Some(c), Some(p)) = (cur.as_object_mut(), partial.as_object()) {
        for (k, v) in p {
            c.insert(k.clone(), v.clone());
        }
    }
    if let Ok(s) = serde_json::to_vec(&cur) {
        write_atomic(&d.join("hud-ui.json"), &s);
    }
    cur
}
