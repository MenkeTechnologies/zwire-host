//! Per-app persistent state under the OS application-data directory.
//!
//! The base dir matches the C++ HUD colour patch (`base::DIR_APP_DATA`) and
//! `scripts/state-dir.sh`, so the `hud-scheme` the host writes is the exact file
//! the compiled colour mixer reads — no split-brain across two locations:
//!   * macOS   `~/Library/Application Support/com.menketechnologies.zwire`
//!   * Windows `%APPDATA%\zwire`
//!   * other   `${XDG_CONFIG_HOME:-~/.config}/zwire`
//!
//! On macOS the zwire folder is the bundle identifier (matching the .app's
//! CFBundleIdentifier and `scripts/state-dir.sh`); the other platforms keep the
//! short `zwire` name.
//!
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

/// True for a built-in scheme name OR a custom-scheme marker (`custom` / `custom-N`).
/// A custom scheme carries its colours in `[theme.palette]` (synced separately); the
/// `scheme` name only records that a custom (vs built-in) scheme is active, so consumers
/// apply the palette instead of a baked table. Accepting these here is what lets a custom
/// scheme PERSIST — otherwise the write is rejected and a poll reverts to the last built-in.
pub fn is_valid_scheme(s: &str) -> bool {
    SCHEMES.contains(&s)
        || s == "custom"
        || s.strip_prefix("custom-")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

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

/// On-disk folder name for the zwire browser's own state. On macOS this is the
/// bundle identifier (matching the .app's `CFBundleIdentifier`, the convention
/// that Application Support dirs are named by reverse-DNS id, and
/// `scripts/state-dir.sh`); the other platforms keep the short `zwire` name.
#[cfg(target_os = "macos")]
const ZWIRE_DIRNAME: &str = "com.menketechnologies.zwire";
#[cfg(not(target_os = "macos"))]
const ZWIRE_DIRNAME: &str = "zwire";

/// The base directory for an app's state, created on demand. `app` empty or
/// missing resolves to the zwire app. For the `zwire` app, `$ZWIRE_STATE`
/// overrides the whole path (the launcher/native-host contract) and otherwise
/// the folder is [`ZWIRE_DIRNAME`], keeping the host, the C++ colour mixer, and
/// the shell scripts pointed at one directory. Any other `app` gets its own
/// `<app>` sub-folder for the generic kv store.
pub fn app_dir(app: &str) -> PathBuf {
    let name = sanitize(app, "zwire");
    let d = if name == "zwire" {
        match std::env::var("ZWIRE_STATE") {
            Ok(s) if !s.is_empty() => PathBuf::from(s),
            _ => state_base().join(ZWIRE_DIRNAME),
        }
    } else {
        state_base().join(&name)
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

// ─────────────────────── shared fleet-wide theme ───────────────────────
// The colour scheme + light/fx prefs (and user-defined custom schemes) live in
// ONE app-independent file, `~/.zwire/global.toml`, so EVERY zwire-host client —
// the browser HUD/newtab/zpwrchrome, Audio-Haxor, zemacs, zpwr-daw, the whole
// fleet — reads and writes the same theme. `d` is the shared theme dir
// (`theme_dir()`); the legacy per-app `hud-scheme`/`hud-ui.json` split is gone.
//
//   [theme]
//   scheme = "midnight"
//   [theme.ui]
//   light = false
//   scanlines = true
//   [theme.palette]          # RESOLVED active colours (var → hex) — the canonical
//   --accent = "#ff2a6d"     # colour source; consumers read exact hex without the
//   --bg-primary = "#05050a" # baked scheme tables, and custom/edited palettes sync
//   [[theme.schemes]]        # the saved custom-scheme LIBRARY (ordered), shared so
//   name = "My Theme"        # every fleet surface sees the same named schemes
//   [theme.schemes.vars]     # each scheme's full colour map (var → hex)
//   --accent = "#0a0d16"

/// The shared theme directory (`~/.zwire`, overridable via `$ZWIRE_GLOBAL_DIR`).
/// App-independent on purpose: this is the fleet's single theme location.
pub fn theme_dir() -> PathBuf {
    let d = match std::env::var("ZWIRE_GLOBAL_DIR") {
        Ok(s) if !s.is_empty() => PathBuf::from(s),
        _ => home().join(".zwire"),
    };
    let _ = std::fs::create_dir_all(&d);
    d
}

fn global_path(d: &Path) -> PathBuf {
    d.join("global.toml")
}

/// Load `global.toml` as a TOML table (empty table when absent/unparseable).
fn load_global(d: &Path) -> toml::Value {
    std::fs::read_to_string(global_path(d))
        .ok()
        .and_then(|s| s.parse::<toml::Value>().ok())
        .filter(|v| v.is_table())
        .unwrap_or_else(|| toml::Value::Table(Default::default()))
}

fn save_global(d: &Path, v: &toml::Value) {
    if let Ok(s) = toml::to_string_pretty(v) {
        write_atomic(&global_path(d), s.as_bytes());
    }
}

/// Set `root[path…] = val`, creating intermediate tables. `root` must be a table.
fn set_path(root: &mut toml::Value, path: &[&str], val: toml::Value) {
    fn go(tbl: &mut toml::map::Map<String, toml::Value>, path: &[&str], val: toml::Value) {
        if path.len() == 1 {
            tbl.insert(path[0].to_string(), val);
            return;
        }
        let e = tbl
            .entry(path[0].to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        if !e.is_table() {
            *e = toml::Value::Table(Default::default());
        }
        go(e.as_table_mut().unwrap(), &path[1..], val);
    }
    if let Some(tbl) = root.as_table_mut() {
        go(tbl, path, val);
    }
}

fn ui_from(root: &toml::Value) -> Value {
    root.get("theme")
        .and_then(|t| t.get("ui"))
        .and_then(|u| serde_json::to_value(u).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

/// Current colour scheme (`[theme].scheme`), defaulting to `cyberpunk`.
pub fn current_scheme(d: &Path) -> String {
    load_global(d)
        .get("theme")
        .and_then(|t| t.get("scheme"))
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "cyberpunk".into())
}

/// Serialize the read-modify-write of `global.toml` across ALL host processes
/// (Chrome spawns one per sendNativeMessage / connectNative). Without this, a
/// concurrent `{scheme}` + `{ui}` write both `load_global` the OLD file and the
/// later `save_global` clobbers the earlier — the sporadic "picked scheme
/// reverts to the old one" bug. An advisory exclusive lock on a sidecar file
/// (auto-released on process exit, so no stale locks) makes each RMW atomic.
fn with_global_lock<F: FnOnce()>(d: &Path, f: F) {
    let _ = std::fs::create_dir_all(d);
    match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(d.join("global.toml.lock"))
    {
        Ok(lock) => {
            let _ = lock.lock(); // blocks until we hold the exclusive lock
            f();
            let _ = lock.unlock();
        }
        Err(_) => f(), // lock unavailable — best-effort write rather than drop it
    }
}

/// Persist the colour scheme (caller validates against [`SCHEMES`] or a custom
/// `[schemes.*]`). Preserves the rest of `global.toml` (ui prefs, custom schemes).
pub fn write_scheme(d: &Path, s: &str) {
    with_global_lock(d, || {
        let mut root = load_global(d);
        set_path(
            &mut root,
            &["theme", "scheme"],
            toml::Value::String(s.to_string()),
        );
        save_global(d, &root);
    });
    // Also emit a plain `hud-scheme` text file beside global.toml. The native C++
    // browser chrome reads the scheme with a tiny FilePathWatcher; giving it a
    // one-line text projection means Chromium needs no TOML parser. `global.toml`
    // stays the single source of truth (one writer), so the two never drift.
    write_atomic(&d.join("hud-scheme"), format!("{s}\n").as_bytes());
    // Refresh the hud-light projection too, so a browser started after a scheme
    // change (but before any light toggle) still sees the current light state.
    let light = current_ui(d)
        .get("light")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    write_hud_light(d, light);
    // Keep the base-colour projection current for the freshly-selected scheme too, so a
    // browser that only sees `hud-scheme`/`hud-palette` change still paints the right base.
    write_hud_palette(d, &current_palette(d));
    // Transitional: also write the legacy per-app location (`<app-data>/zwire/
    // hud-scheme`) that a browser built BEFORE the ~/.zwire C++ patch reads, so
    // window-chrome colouring keeps working until that browser is rebuilt.
    // Harmless afterwards (nothing reads it). Remove once every build is current.
    write_atomic(
        &app_dir("zwire").join("hud-scheme"),
        format!("{s}\n").as_bytes(),
    );
}

/// Current light/fx UI-preference object (`[theme.ui]`; empty when unset).
pub fn current_ui(d: &Path) -> Value {
    ui_from(&load_global(d))
}

/// Shallow-merge `partial` into `[theme.ui]` and persist; returns the merged
/// object. Preserves scheme + custom schemes.
pub fn write_ui(d: &Path, partial: &Value) -> Value {
    let mut ui = json!({});
    with_global_lock(d, || {
        let mut root = load_global(d);
        ui = ui_from(&root);
        if let (Some(c), Some(p)) = (ui.as_object_mut(), partial.as_object()) {
            for (k, v) in p {
                c.insert(k.clone(), v.clone());
            }
        }
        let ui_toml =
            toml::Value::try_from(&ui).unwrap_or_else(|_| toml::Value::Table(Default::default()));
        set_path(&mut root, &["theme", "ui"], ui_toml);
        save_global(d, &root);
    });
    write_hud_light(
        d,
        ui.get("light").and_then(|v| v.as_bool()).unwrap_or(false),
    );
    ui
}

/// Create `global.toml` with a default theme when it's absent, so consumers
/// (zemacs and the rest of the fleet) always have a file to read on a fresh
/// machine — called once at host startup. The dir is created by [`theme_dir`].
/// Never clobbers an existing file. The palette is left for the HUD to fill on
/// first paint (the host has no colour tables); scheme + light are enough to
/// theme the fleet from launch.
pub fn ensure_global(d: &Path) {
    if global_path(d).exists() {
        return;
    }
    with_global_lock(d, || {
        if global_path(d).exists() {
            return; // another host process seeded it while we waited on the lock
        }
        let mut root = toml::Value::Table(Default::default());
        set_path(
            &mut root,
            &["theme", "scheme"],
            toml::Value::String("cyberpunk".into()),
        );
        set_path(
            &mut root,
            &["theme", "ui", "light"],
            toml::Value::Boolean(false),
        );
        save_global(d, &root);
    });
    // Plain projections the native chrome watches, so a first-run browser paints
    // the default immediately without waiting for a HUD write.
    write_atomic(&d.join("hud-scheme"), b"cyberpunk\n");
    write_hud_light(d, false);
}

/// Current resolved colour palette (`[theme.palette]`; empty object when unset).
/// This is the fleet's canonical colour source: a CSS-var → hex map for the
/// active scheme + light/dark, so any consumer (zemacs, a Vivaldi mod, a plain
/// script) reads exact colours here without needing zgui's baked scheme tables.
pub fn current_palette(d: &Path) -> Value {
    load_global(d)
        .get("theme")
        .and_then(|t| t.get("palette"))
        .and_then(|p| serde_json::to_value(p).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

/// Persist the resolved active palette (CSS-var → hex map) to `[theme.palette]`,
/// replacing the previous one. Preserves scheme + ui + custom `[schemes.*]`.
/// Returns the stored object (empty when the input wasn't an object).
pub fn write_palette(d: &Path, palette: &Value) -> Value {
    let obj = if palette.is_object() {
        palette.clone()
    } else {
        json!({})
    };
    with_global_lock(d, || {
        let mut root = load_global(d);
        let p_toml =
            toml::Value::try_from(&obj).unwrap_or_else(|_| toml::Value::Table(Default::default()));
        set_path(&mut root, &["theme", "palette"], p_toml);
        save_global(d, &root);
    });
    // Refresh the native chrome's base-colour projection so tabs/toolbar/omnibox follow
    // the exact resolved colours — the same for a custom scheme as a built-in.
    write_hud_palette(d, &obj);
    obj
}

/// The user's saved custom-scheme LIBRARY (`[theme.schemes]`, an ordered array of
/// `{ name, vars }`), or an empty array when none. This is the shared home for every
/// named custom colourscheme — so a scheme saved in one surface (the HUD settings)
/// is visible to the whole fleet, not just the origin that created it.
pub fn current_schemes(d: &Path) -> Value {
    load_global(d)
        .get("theme")
        .and_then(|t| t.get("schemes"))
        .and_then(|s| serde_json::to_value(s).ok())
        .filter(|v| v.is_array())
        .unwrap_or_else(|| json!([]))
}

/// Persist the saved-scheme library to `[theme.schemes]`, replacing the previous one.
/// Preserves the active scheme + palette + ui. Returns the stored array (empty when
/// the input wasn't an array). Each element is `{ name: String, vars: {css-var: hex} }`.
pub fn write_schemes(d: &Path, schemes: &Value) -> Value {
    let arr = if schemes.is_array() {
        schemes.clone()
    } else {
        json!([])
    };
    with_global_lock(d, || {
        let mut root = load_global(d);
        let s_toml =
            toml::Value::try_from(&arr).unwrap_or_else(|_| toml::Value::Array(Default::default()));
        set_path(&mut root, &["theme", "schemes"], s_toml);
        save_global(d, &root);
    });
    arr
}

/// Plain `hud-light` text projection ("1"/"0") beside `hud-scheme`, so the native
/// C++ chrome can follow light mode with a tiny FilePathWatcher (no TOML parse) —
/// mirroring how `hud-scheme` projects the colour scheme.
fn write_hud_light(d: &Path, light: bool) {
    write_atomic(&d.join("hud-light"), if light { b"1\n" } else { b"0\n" });
}

/// The base tokens the native chrome paints from (frame/toolbar/tab/omnibox + accents).
/// Order is irrelevant — the projection is keyed — but this is the set the C++ reads.
const HUD_PALETTE_KEYS: [&str; 10] = [
    "--bg-primary",
    "--bg-secondary",
    "--bg-card",
    "--bg-hover",
    "--text",
    "--text-dim",
    "--accent",
    "--cyan",
    "--magenta",
    "--border",
];

/// `hud-palette` text projection beside `hud-scheme`: the RESOLVED base colours of the
/// active scheme as `--var=#rrggbb` lines. This lets the native chrome paint ANY scheme
/// — built-in OR custom — straight from the real colours, with no baked scheme table and
/// no TOML parser (the whole point of the text projections). The chrome reads whichever
/// keys it knows and ignores the rest; an empty/absent file leaves it on its fallback.
/// Every consumer already mirrors the active scheme's resolved palette into
/// `[theme.palette]`, so this stays in lockstep with what the fleet actually shows.
fn write_hud_palette(d: &Path, palette: &Value) {
    let obj = match palette.as_object() {
        Some(o) if !o.is_empty() => o,
        _ => return, // no resolved palette yet — don't clobber a good projection with nothing
    };
    let mut out = String::new();
    for k in HUD_PALETTE_KEYS {
        if let Some(hex) = obj.get(k).and_then(|v| v.as_str()) {
            let hex = hex.trim();
            // Only project plain #rrggbb hex — the native parser is deliberately tiny.
            if hex.starts_with('#') && hex.len() == 7 {
                out.push_str(k);
                out.push('=');
                out.push_str(hex);
                out.push('\n');
            }
        }
    }
    if !out.is_empty() {
        write_atomic(&d.join("hud-palette"), out.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_scheme_accepts_builtins_and_custom_markers() {
        // built-ins
        for s in SCHEMES {
            assert!(is_valid_scheme(s), "built-in {s} should be valid");
        }
        // custom markers: the live custom + numbered saved presets
        assert!(is_valid_scheme("custom"));
        assert!(is_valid_scheme("custom-0"));
        assert!(is_valid_scheme("custom-12"));
        // rejects: unknown names + malformed custom markers (so a typo can't poison the file)
        assert!(!is_valid_scheme("bogus"));
        assert!(!is_valid_scheme("custom-"));
        assert!(!is_valid_scheme("custom-x"));
        assert!(!is_valid_scheme("custom-1a"));
        assert!(!is_valid_scheme(""));
    }

    #[test]
    fn write_hud_palette_projects_base_hex_and_skips_non_hex() {
        let d = std::env::temp_dir().join(format!("zwire-hudpal-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        let pal = json!({
            "--bg-primary": "#05050a",
            "--accent": "#ff2a6d",
            "--cyan": "#05d9e8",
            "--magenta": "#d300c5",
            "--border": "#1a1a3e",
            "--text": "#e0f0ff",
            "--accent-glow": "rgba(255, 42, 109, 0.4)", // rgba → skipped (tiny native parser)
            "--bogus": "notacolor",                     // unknown key → not projected
        });
        write_hud_palette(&d, &pal);
        let out = std::fs::read_to_string(d.join("hud-palette")).unwrap();
        assert!(out.contains("--accent=#ff2a6d\n"), "accent projected");
        assert!(out.contains("--cyan=#05d9e8\n"), "cyan projected");
        assert!(out.contains("--bg-primary=#05050a\n"), "bg projected");
        assert!(!out.contains("accent-glow"), "rgba variant not projected");
        assert!(!out.contains("bogus"), "unknown key not projected");

        // An empty palette must NOT clobber an existing good projection.
        let d2 = std::env::temp_dir().join(format!("zwire-hudpal-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d2);
        write_hud_palette(&d2, &json!({}));
        assert!(!d2.join("hud-palette").exists(), "empty palette writes nothing");

        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&d2);
    }
}
