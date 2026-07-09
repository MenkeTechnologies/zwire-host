//! Streaming filesystem observers: `fs_watch` and `fs_tail`.
//!
//! `fs_watch` polls a file or directory and streams `{"ev":"fs","kind":…,"path":…}`
//! frames on create / modify / remove. `fs_tail` streams `{"ev":"line","data":…}`
//! as lines are appended to a file — `tail -f`, surviving truncation and log
//! rotation. Each observer runs on a background thread keyed by the request `id`
//! (so one connection can watch many paths), stops on `watch_stop`, and is torn
//! down when the connection closes. Poll-based, so no new dependencies.
use crate::proto::{send_msg, Out};
use crate::store::expand;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, UNIX_EPOCH};

/// Cap on entries scanned per `fs_watch` tick (guards watching a huge tree).
const MAX_ENTRIES: usize = 20_000;
/// Flush a `fs_tail` partial line once it grows past this without a newline.
const MAX_LINE: usize = 1 << 20;

/// A running observer. Dropping it stops the thread and joins it.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Watcher {
    /// Watch `path` (file or directory) for changes; `recursive` descends
    /// subdirectories, `interval_ms` sets the poll cadence (default 1000).
    pub fn fs_watch(out: &Out, req: &Value, id: String) -> Watcher {
        let path = expand(req["path"].as_str().unwrap_or("."));
        let recursive = req["recursive"].as_bool().unwrap_or(false);
        let interval = Duration::from_millis(req["interval_ms"].as_u64().unwrap_or(1000).max(100));
        Self::spawn(out, move |out, stop| {
            watch_loop(out, stop, &path, recursive, interval, &id)
        })
    }

    /// Tail `path`, streaming appended lines. `from` = `"start"` replays the
    /// whole file first; otherwise (default) only new lines are streamed.
    pub fn fs_tail(out: &Out, req: &Value, id: String) -> Watcher {
        let path = expand(req["path"].as_str().unwrap_or("."));
        let from_end = req["from"].as_str() != Some("start");
        let interval = Duration::from_millis(req["interval_ms"].as_u64().unwrap_or(300).max(50));
        Self::spawn(out, move |out, stop| {
            tail_loop(out, stop, &path, from_end, interval, &id)
        })
    }

    /// Stream a small rewritten JSON file (the zwire HUD engine meter frames)
    /// to the client on every change — a PUSH feed so the page never polls (which
    /// starves during scroll / page build). Pushes `{"ev":"meter","text":…}`.
    pub fn meter_stream(out: &Out, req: &Value, id: String) -> Watcher {
        let path = expand(req["path"].as_str().unwrap_or("."));
        let interval = Duration::from_millis(req["interval_ms"].as_u64().unwrap_or(33).max(16));
        Self::spawn(out, move |out, stop| {
            meter_loop(out, stop, &path, interval, &id)
        })
    }

    fn spawn(out: &Out, body: impl FnOnce(&Out, &AtomicBool) + Send + 'static) -> Watcher {
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, o) = (stop.clone(), out.clone());
        let handle = std::thread::spawn(move || body(&o, &s2));
        Watcher {
            stop,
            handle: Some(handle),
        }
    }
}

/// Sleep `dur`, waking early (every 100 ms) to notice a stop request.
fn sleep_sliced(stop: &AtomicBool, dur: Duration) {
    let mut slept = Duration::ZERO;
    while slept < dur && !stop.load(Ordering::Relaxed) {
        let slice = Duration::from_millis(100).min(dur - slept);
        std::thread::sleep(slice);
        slept += slice;
    }
}

fn with_id(mut ev: Value, id: &str) -> Value {
    if !id.is_empty() {
        ev["id"] = json!(id);
    }
    ev
}

// Push the file's content on every change (mtime bump). Host-side thread, so it
// is immune to the page's main-thread throttling during scroll / initial build.
fn meter_loop(out: &Out, stop: &AtomicBool, path: &Path, interval: Duration, id: &str) {
    let mut last = 0u64;
    while !stop.load(Ordering::Relaxed) {
        if let Some(mt) = mtime_ms(path) {
            if mt != last {
                last = mt;
                if let Ok(bytes) = std::fs::read(path) {
                    if let Ok(text) = String::from_utf8(bytes) {
                        let frame = with_id(json!({ "ev": "meter", "text": text }), id);
                        if send_msg(out, &frame).is_err() {
                            break;
                        }
                    }
                }
            }
        }
        sleep_sliced(stop, interval);
    }
}

fn mtime_ms(p: &Path) -> Option<u64> {
    std::fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// A `path -> mtime(ms)` snapshot of everything under `root`.
fn snapshot(root: &Path, recursive: bool) -> HashMap<PathBuf, u64> {
    let mut out = HashMap::new();
    if root.is_file() {
        if let Some(m) = mtime_ms(root) {
            out.insert(root.to_path_buf(), m);
        }
        return out;
    }
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if let Some(m) = mtime_ms(&p) {
                out.insert(p.clone(), m);
            }
            if is_dir && recursive && depth < 32 {
                stack.push((p, depth + 1));
            }
            if out.len() >= MAX_ENTRIES {
                return out;
            }
        }
    }
    out
}

fn watch_loop(
    out: &Out,
    stop: &AtomicBool,
    root: &Path,
    recursive: bool,
    interval: Duration,
    id: &str,
) {
    let emit = |kind: &str, p: &Path| -> bool {
        let frame = with_id(
            json!({"ev": "fs", "kind": kind, "path": p.to_string_lossy()}),
            id,
        );
        send_msg(out, &frame).is_ok()
    };
    let mut prev = snapshot(root, recursive);
    while !stop.load(Ordering::Relaxed) {
        sleep_sliced(stop, interval);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let cur = snapshot(root, recursive);
        for (p, m) in &cur {
            let changed = match prev.get(p) {
                None => emit("created", p),
                Some(pm) if pm != m => emit("modified", p),
                _ => true,
            };
            if !changed {
                return; // port closed
            }
        }
        for p in prev.keys() {
            if !cur.contains_key(p) && !emit("removed", p) {
                return;
            }
        }
        prev = cur;
    }
}

fn tail_loop(
    out: &Out,
    stop: &AtomicBool,
    path: &Path,
    from_end: bool,
    interval: Duration,
    id: &str,
) {
    let mut pos: u64 = if from_end {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    let mut carry = String::new();
    while !stop.load(Ordering::Relaxed) {
        if let Ok(mut f) = File::open(path) {
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < pos {
                // Truncated or rotated: start over from the top.
                pos = 0;
                carry.clear();
            }
            if len > pos && f.seek(SeekFrom::Start(pos)).is_ok() {
                let mut buf = Vec::new();
                if f.take(len - pos).read_to_end(&mut buf).is_ok() {
                    pos += buf.len() as u64;
                    carry.push_str(&String::from_utf8_lossy(&buf));
                    if !flush_lines(out, &mut carry, id) {
                        return; // port closed
                    }
                }
            }
        }
        sleep_sliced(stop, interval);
    }
}

/// Emit each complete line in `carry`; returns false if the port closed.
fn flush_lines(out: &Out, carry: &mut String, id: &str) -> bool {
    while let Some(idx) = carry.find('\n') {
        let line: String = carry.drain(..=idx).collect();
        let line = line.trim_end_matches(['\r', '\n']);
        if !send_line(out, line, id) {
            return false;
        }
    }
    if carry.len() > MAX_LINE {
        let line = std::mem::take(carry);
        return send_line(out, &line, id);
    }
    true
}

fn send_line(out: &Out, line: &str, id: &str) -> bool {
    send_msg(out, &with_id(json!({"ev": "line", "data": line}), id)).is_ok()
}
