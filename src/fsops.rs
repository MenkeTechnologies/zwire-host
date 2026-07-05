//! Filesystem capability: read/write/list/stat/mkdir/rm.
//!
//! Paths accept a leading `~`. Binary payloads cross the JSON pipe as base64
//! (`b64`); text convenience fields (`text`) are also accepted on writes and
//! returned on reads when the bytes are valid UTF-8.
use crate::proto::{b64_decode, b64_encode};
use crate::store::expand;
use serde_json::{json, Value};
use std::time::UNIX_EPOCH;

/// Cap a single `fs_read` at 64 MiB so a huge file can't exhaust memory.
const MAX_READ: u64 = 64 * 1024 * 1024;

/// Cap a single `fs_walk` at 50k entries so crawling `/` can't run away.
const MAX_WALK: usize = 50_000;

/// Dispatch an `fs_*` command. Returns the reply object (without `id`, which the
/// caller stamps on). Unknown commands yield `{"ok":false,"err":"unknown_cmd"}`.
pub fn handle(cmd: &str, req: &Value) -> Value {
    match cmd {
        "fs_read" => read(req),
        "fs_write" => write(req, false),
        "fs_append" => write(req, true),
        "fs_list" => list(req),
        "fs_walk" => walk(req),
        "fs_stat" => stat(req),
        "fs_mkdir" => mkdir(req),
        "fs_rm" => rm(req),
        _ => json!({"ok": false, "err": "unknown_cmd"}),
    }
}

fn path_of(req: &Value) -> Option<std::path::PathBuf> {
    req["path"].as_str().map(expand)
}

fn err(e: impl ToString) -> Value {
    json!({"ok": false, "err": e.to_string()})
}

fn read(req: &Value) -> Value {
    let Some(p) = path_of(req) else {
        return json!({"ok": false, "err": "no_path"});
    };
    match std::fs::metadata(&p) {
        Ok(m) if m.len() > MAX_READ => return json!({"ok": false, "err": "too_large"}),
        Err(e) => return err(e),
        _ => {}
    }
    match std::fs::read(&p) {
        Ok(bytes) => {
            let mut out = json!({"ok": true, "b64": b64_encode(&bytes)});
            if let Ok(s) = String::from_utf8(bytes) {
                out["text"] = json!(s);
            }
            out
        }
        Err(e) => err(e),
    }
}

fn write(req: &Value, append: bool) -> Value {
    let Some(p) = path_of(req) else {
        return json!({"ok": false, "err": "no_path"});
    };
    let bytes = if let Some(b) = req["b64"].as_str() {
        match b64_decode(b) {
            Some(v) => v,
            None => return json!({"ok": false, "err": "bad_b64"}),
        }
    } else if let Some(t) = req["text"].as_str() {
        t.as_bytes().to_vec()
    } else {
        return json!({"ok": false, "err": "no_content"});
    };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let res = if append {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .and_then(|mut f| f.write_all(&bytes))
    } else {
        std::fs::write(&p, &bytes)
    };
    match res {
        Ok(()) => json!({"ok": true, "bytes": bytes.len()}),
        Err(e) => err(e),
    }
}

fn list(req: &Value) -> Value {
    let Some(p) = path_of(req) else {
        return json!({"ok": false, "err": "no_path"});
    };
    match std::fs::read_dir(&p) {
        Ok(rd) => {
            let mut entries = Vec::new();
            for e in rd.flatten() {
                let md = e.metadata().ok();
                entries.push(json!({
                    "name": e.file_name().to_string_lossy(),
                    "dir": md.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                    "size": md.as_ref().map(|m| m.len()).unwrap_or(0),
                }));
            }
            json!({"ok": true, "entries": entries})
        }
        Err(e) => err(e),
    }
}

/// Recursively crawl a directory tree — the "crawl filesystem from a plugin"
/// path. Fields: `path` (root), `depth` (max levels, default unlimited),
/// `dirs_only`, `ext` (only files with this extension), `contains` (substring
/// filter on the leaf name). Capped at [`MAX_WALK`] entries; `truncated` flags
/// when the cap was hit.
fn walk(req: &Value) -> Value {
    let Some(root) = path_of(req) else {
        return json!({"ok": false, "err": "no_path"});
    };
    let max_depth = req["depth"].as_u64().map(|d| d as usize);
    let dirs_only = req["dirs_only"].as_bool().unwrap_or(false);
    let ext = req["ext"].as_str();
    let contains = req["contains"].as_str();

    let mut entries = Vec::new();
    let mut stack = vec![(root.clone(), 0usize)];
    let mut truncated = false;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let path = e.path();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir && max_depth.is_none_or(|m| depth < m) {
                stack.push((path.clone(), depth + 1));
            }
            if dirs_only && !is_dir {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(c) = contains {
                if !name.contains(c) {
                    continue;
                }
            }
            if let Some(x) = ext {
                if path.extension().and_then(|s| s.to_str()) != Some(x) {
                    continue;
                }
            }
            entries.push(json!({
                "path": path.to_string_lossy(),
                "name": name,
                "dir": is_dir,
                "size": e.metadata().map(|m| m.len()).unwrap_or(0),
            }));
            if entries.len() >= MAX_WALK {
                truncated = true;
                break;
            }
        }
        if truncated {
            break;
        }
    }
    json!({"ok": true, "root": root.to_string_lossy(), "count": entries.len(), "truncated": truncated, "entries": entries})
}

fn mtime(md: &std::fs::Metadata) -> Option<u64> {
    md.modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn stat(req: &Value) -> Value {
    let Some(p) = path_of(req) else {
        return json!({"ok": false, "err": "no_path"});
    };
    match std::fs::metadata(&p) {
        Ok(m) => json!({
            "ok": true, "exists": true,
            "dir": m.is_dir(), "file": m.is_file(),
            "size": m.len(), "mtime": mtime(&m),
        }),
        Err(_) => json!({"ok": true, "exists": false}),
    }
}

fn mkdir(req: &Value) -> Value {
    let Some(p) = path_of(req) else {
        return json!({"ok": false, "err": "no_path"});
    };
    match std::fs::create_dir_all(&p) {
        Ok(()) => json!({"ok": true}),
        Err(e) => err(e),
    }
}

fn rm(req: &Value) -> Value {
    let Some(p) = path_of(req) else {
        return json!({"ok": false, "err": "no_path"});
    };
    let recursive = req["recursive"].as_bool().unwrap_or(false);
    let res = match std::fs::metadata(&p) {
        Ok(m) if m.is_dir() && recursive => std::fs::remove_dir_all(&p),
        Ok(m) if m.is_dir() => std::fs::remove_dir(&p),
        Ok(_) => std::fs::remove_file(&p),
        Err(_) => return json!({"ok": true}), // already gone
    };
    match res {
        Ok(()) => json!({"ok": true}),
        Err(e) => err(e),
    }
}
