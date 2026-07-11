//! Shared request/response log. Chrome spawns a SEPARATE native-messaging host
//! process per `sendNativeMessage` (and one per persistent `connectNative`
//! port), so no single process sees every command. To let the HUD "HOST" tab
//! show ALL tx/rx to zwire-host regardless of which client/process handled it,
//! every process appends a compact JSON line to one shared ring-capped file
//! (`~/.zwire/hostlog.jsonl`). The HOST tab reads it back via the `hostlog`
//! command. Streaming frames (sysinfo/pty/job output) are pushed with
//! `out.send` directly rather than `respond`, so only real commands + their
//! replies land here — not the high-frequency stream noise.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 768 * 1024; // trim the ring file past this
const TRIM_KEEP: usize = 2000; // lines to keep when trimming

/// High-frequency internal plumbing that would drown the real commands: the
/// theme-sync `get` poll (every ~1.5s per HUD page) and bus (un)subscribe
/// churn. These aren't "commands the user runs", so keep them out of the log.
/// sysinfo frames ARE kept (the statusbar stream the user asked to see) — the
/// HOST tab hides them behind a toggle instead.
const NOISE_CMDS: [&str; 3] = ["get", "sub", "unsub"];

fn log_path() -> PathBuf {
    crate::store::theme_dir().join("hostlog.jsonl")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The command name of a request, incl. the legacy commandless `{scheme}`/`{ui}`.
fn cmd_of(req: &Value) -> &str {
    if let Some(c) = req.get("cmd").and_then(|c| c.as_str()) {
        return c;
    }
    if !req["scheme"].is_null() {
        "scheme"
    } else if !req["ui"].is_null() {
        "ui"
    } else {
        "?"
    }
}

/// Append one tx/rx entry to the shared log. `dir` is "tx" (request) or "rx"
/// (response). `req` is always the originating request (for the cmd + id);
/// `data` is what to summarise (the request for tx, the response for rx).
pub fn record(dir: &str, req: &Value, data: &Value) {
    let cmd = cmd_of(req);
    // Never log the log-reader itself, the high-frequency plumbing polls, or
    // anything explicitly opted out.
    if cmd == "hostlog"
        || NOISE_CMDS.contains(&cmd)
        || req.get("_nolog").and_then(|b| b.as_bool()).unwrap_or(false)
    {
        return;
    }
    let mut summary = data.to_string();
    if summary.len() > 400 {
        summary.truncate(400);
        summary.push('…');
    }
    let entry = json!({
        "t": now_ms(),
        "dir": dir,
        "cmd": cmd,
        "pid": std::process::id(),
        "msg": summary,
    });
    let path = log_path();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        // Format the ENTIRE line (incl. newline) into one buffer, then a single write_all → one
        // write() syscall, which O_APPEND makes atomic across processes. `writeln!(f, "{entry}")`
        // does NOT: it formats the JSON field-by-field, one write() per piece, so concurrent
        // zwire-host processes interleave fields and corrupt lines ({"{cmd""cmd:"…).
        let line = format!("{entry}\n");
        let _ = f.write_all(line.as_bytes());
    }
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        trim(&path);
    }
}

/// Keep only the newest `TRIM_KEEP` lines. Best-effort (tmp+rename); a rare race
/// with a concurrent append can drop a line, which is fine for a monitor log.
fn trim(path: &Path) {
    let Ok(data) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = data.lines().collect();
    if lines.len() <= TRIM_KEEP {
        return;
    }
    let tail = lines[lines.len() - TRIM_KEEP..].join("\n");
    let tmp = path.with_extension("jsonl.tmp");
    if std::fs::write(&tmp, format!("{tail}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Return the newest `n` entries (oldest→newest) for the HOST tab.
pub fn read_tail(n: usize) -> Vec<Value> {
    let Ok(data) = std::fs::read_to_string(log_path()) else {
        return Vec::new();
    };
    let lines: Vec<&str> = data.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..]
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}
