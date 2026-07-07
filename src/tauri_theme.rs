//! A ready-made Tauri v2 plugin for fleet-wide theme sync (feature `tauri`).
//!
//! Wiring an app to the shared `~/.zwire/global.toml` theme is then two lines in
//! the app's `src-tauri`:
//!
//! ```ignore
//! // Cargo.toml:  zwire-host = { git = "…", features = ["tauri"] }
//! tauri::Builder::default()
//!     .plugin(zwire_host::tauri_theme::init())   // <- theme_get/theme_set + `theme-changed`
//!     // …
//! ```
//!
//! plus loading `zgui-core/webui/theme-sync.js` in the app's frontend. The
//! plugin registers `theme_get` / `theme_set` commands (invoked from
//! `ZGui.themeSync` as `plugin:zwire-theme|theme_get`) and starts a background
//! watcher that emits a global `theme-changed` event whenever the shared theme
//! changes — from this app or any other in the fleet.
use crate::api;
use serde_json::{json, Value};
use tauri::{
    plugin::{Builder, TauriPlugin},
    Emitter, Runtime,
};

/// Current shared theme, `{ scheme, ui }`.
#[tauri::command]
fn theme_get() -> Value {
    let (scheme, ui) = api::theme_get();
    json!({ "scheme": scheme, "ui": ui })
}

/// Set the shared scheme and/or merge a partial light/fx object. Both optional,
/// so a caller sends only what changed.
#[tauri::command]
fn theme_set(scheme: Option<String>, ui: Option<Value>) {
    if let Some(s) = scheme {
        api::theme_set_scheme(&s);
    }
    if let Some(u) = ui {
        api::theme_set_ui(&u);
    }
}

/// The plugin. Add with `.plugin(zwire_host::tauri_theme::init())`.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("zwire-theme")
        .invoke_handler(tauri::generate_handler![theme_get, theme_set])
        .setup(|app, _api| {
            let handle = app.clone();
            // Bridge live shared-theme changes (this app or any other) to the
            // webview as a global `theme-changed` event. Fires once immediately so
            // the frontend converges on boot even before it calls theme_get.
            api::theme_watch(move |scheme, ui| {
                let _ = handle.emit("theme-changed", json!({ "scheme": scheme, "ui": ui }));
            });
            Ok(())
        })
        .build()
}
