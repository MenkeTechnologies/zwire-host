//! Cross-process live theme sync.
//!
//! The shared theme (`~/.zwire/global.toml`) is written by whichever app's
//! zwire-host process the user toggled in. Snapshot-on-subscribe already makes a
//! *newly connecting* client converge, but two apps open at once each run their
//! OWN host process with a process-local bus — so a change in one wouldn't reach
//! the other's already-subscribed clients.
//!
//! This watcher closes that gap: a background thread (started lazily on the first
//! theme `sub`) polls the shared file and, when the scheme/ui differs from what
//! THIS process last saw, republishes it to this process's local bus subscribers.
//! So a toggle in Audio-Haxor fans out to the live zwire HUD, zemacs, etc.
//!
//! Echo control: local writes call [`note_scheme`] / [`note_ui`] to record the
//! value this process just wrote, so the watcher recognises its own change and
//! doesn't re-publish it (the write path already published to local subs).
use crate::{bus, store};
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

fn last() -> &'static Mutex<(String, Value)> {
    static L: OnceLock<Mutex<(String, Value)>> = OnceLock::new();
    L.get_or_init(|| Mutex::new((String::new(), Value::Null)))
}

/// Record the scheme this process just wrote so the watcher won't echo it.
pub fn note_scheme(s: &str) {
    last().lock().unwrap().0 = s.to_string();
}

/// Record the ui this process just wrote so the watcher won't echo it.
pub fn note_ui(ui: &Value) {
    last().lock().unwrap().1 = ui.clone();
}

/// Start the shared-theme file watcher exactly once per process. No-op if the
/// theme feature is never used (called from the first `sub` to a theme topic).
pub fn ensure_started() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return; // already running
    }
    // Seed with the current values so the first tick doesn't fire a spurious change.
    let d = store::theme_dir();
    {
        let mut l = last().lock().unwrap();
        l.0 = store::current_scheme(&d);
        l.1 = store::current_ui(&d);
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_millis(700));
        let d = store::theme_dir();
        let scheme = store::current_scheme(&d);
        let ui = store::current_ui(&d);
        // Decide what changed under the lock, then release it BEFORE publishing so
        // we never hold `last` across the bus fan-out.
        let (pub_scheme, pub_ui) = {
            let mut l = last().lock().unwrap();
            let ps = l.0 != scheme;
            if ps {
                l.0 = scheme.clone();
            }
            let pu = l.1 != ui;
            if pu {
                l.1 = ui.clone();
            }
            (ps, pu)
        };
        if pub_scheme {
            bus::publish("scheme", &json!({ "scheme": scheme }));
        }
        if pub_ui {
            bus::publish("ui", &ui);
        }
    });
}
