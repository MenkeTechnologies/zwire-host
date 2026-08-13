//! Extended filesystem capability — the ops the shared **file browser**
//! (`zpwr-file-browser/webui/file-browser.js`) calls on its host.
//!
//! [`crate::fsops`] covers the primitive verbs (read/write/list/walk/stat/mkdir/rm)
//! that every zwire caller uses. The file browser needs a much wider surface:
//! hashing, archives, directory comparison, text diff, duplicate detection, git
//! status, xattrs, disk usage, secure delete, chmod, symlink retargeting, and the
//! rename/trash/delete core. Those live here so `fsops` stays the small primitive
//! set it has always been.
//!
//! # Provenance
//! Every op is a faithful port of the corresponding pure function in
//! `zpwr-file-browser/crate/src/lib.rs` — the fleet's shared file-browser engine,
//! itself a verbatim port of Audio-Haxor's `fs_*` command layer. Bodies are the
//! upstream bodies; the only changes are mechanical:
//!   * `async fn … -> Result<T, String>` → sync `fn … -> Result<Value, String>`
//!     (the upstream bodies are synchronous; the `async` was signature parity
//!     with their Tauri wrappers),
//!   * typed return structs → the `serde_json` object they serialised to, so the
//!     JSON pipe stays the only contract,
//!   * paths run through [`crate::store::expand`], matching the rest of this host
//!     (a leading `~` is accepted everywhere here; upstream took absolute paths).
//!
//! # Why this is a port and not a dependency
//! The obviously better arrangement is for this host to depend on the shared
//! `zpwr-file-browser` crate and call its `dispatch(cmd, args)` — one line instead
//! of this file. That is blocked, not overlooked: `zpwr-file-browser` sets
//! `publish = false` and is absent from the registry, while `zwire-host` is a
//! published crate, and Cargo rejects a publish whose dependency has no registry
//! version (including an optional one). Taking the dependency would silently
//! de-publish this crate. If `zpwr-file-browser` is ever published, this module
//! should be deleted and replaced by that call.
use crate::store::expand;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Dispatch one extended `fs_*` command. `None` means "not one of ours", which
/// lets [`crate::fsops::handle`] report `unknown_cmd` for a genuine typo.
///
/// Replies are the host's usual envelope: `{"ok":true,"data":…}` on success,
/// `{"ok":false,"err":…}` on failure. The browser's shim unwraps `data`.
pub fn handle(cmd: &str, req: &Value) -> Option<Value> {
    let r = match cmd {
        /* ---- directory / info ---- */
        "fs_list_dir" => call(req, |a| {
            list_dir(&req_path(a, "dir_path")?, opt_bool(a, "include_hidden"))
        }),
        "fs_list_subdirs" => call(req, |a| {
            list_subdirs(&req_path(a, "dir_path")?, opt_bool(a, "include_hidden"))
        }),
        "fs_folder_size" => call(req, |a| {
            folder_size(&req_path(a, "folder_path")?, opt_u64(a, "timeout_ms"))
        }),
        "fs_get_info" => call(req, |a| get_info(&req_path(a, "path")?)),
        "fs_disk_usage" => call(req, |a| disk_usage(&req_path(a, "path")?)),
        "fs_xattrs" => call(req, |a| xattrs(&req_path(a, "path")?)),
        "fs_git_status" => call(req, |a| git_status(&req_path(a, "dir_path")?)),

        /* ---- mutate ---- */
        "fs_rename_file" => call(req, |a| {
            rename_file(&req_path(a, "old_path")?, &req_path(a, "new_path")?)
        }),
        "fs_delete_file" => call(req, |a| delete_file(&req_path(a, "file_path")?)),
        "fs_move_to_trash" => call(req, |a| move_to_trash(&req_path(a, "file_path")?)),
        "fs_secure_delete" => call(req, |a| secure_delete(&req_path(a, "file_path")?)),
        "fs_duplicate" => call(req, |a| duplicate(&req_path(a, "path")?)),
        "fs_copy_path" => call(req, |a| {
            copy_path(&req_path(a, "src")?, &req_path(a, "dest")?)
        }),
        "fs_create_dir" => call(req, |a| create_dir(&req_path(a, "dir_path")?)),
        "fs_create_file" => call(req, |a| create_file(&req_path(a, "file_path")?)),
        "fs_touch" => call(req, |a| touch(&req_path(a, "file_path")?)),
        "fs_chmod" => call(req, |a| {
            chmod(&req_path(a, "path")?, &req_str(a, "mode_octal")?)
        }),
        "fs_symlink_retarget" => call(req, |a| {
            symlink_retarget(&req_path(a, "path")?, &req_str(a, "new_target")?)
        }),

        /* ---- read ---- */
        "fs_read_file_base64" => call(req, |a| {
            read_file_base64(&req_path(a, "file_path")?, opt_u64(a, "max_bytes"))
        }),
        "fs_read_head" => call(req, |a| {
            read_head(&req_path(a, "file_path")?, opt_u64(a, "max_bytes"))
        }),
        "fs_read_head_bytes" => call(req, |a| {
            read_head_bytes(&req_path(a, "file_path")?, opt_u64(a, "max_bytes"))
        }),

        /* ---- search / compare / archive / hash ---- */
        "fs_grep" => call(req, |a| {
            grep(
                &req_path(a, "root")?,
                &req_str(a, "needle")?,
                opt_bool(a, "case_insensitive"),
                opt_u64(a, "max_results").map(|n| n as usize),
            )
        }),
        "fs_find_duplicates" => call(req, |a| {
            find_duplicates(
                &req_path(a, "dir")?,
                opt_bool(a, "recursive"),
                opt_u64(a, "min_size_bytes"),
            )
        }),
        "fs_compare_dirs" => call(req, |a| {
            compare_dirs(&req_path(a, "dir_a")?, &req_path(a, "dir_b")?)
        }),
        "fs_diff" => call(req, |a| {
            diff(&req_path(a, "path_a")?, &req_path(a, "path_b")?)
        }),
        "fs_hash" => call(req, |a| {
            hash(&req_path(a, "path")?, opt_vec_str(a, "algos"))
        }),
        "fs_compress" => call(req, |a| {
            compress(&req_paths(a, "paths")?, &req_path(a, "archive_path")?)
        }),
        "fs_extract" => call(req, |a| {
            extract(&req_path(a, "archive_path")?, &req_path(a, "dest_dir")?)
        }),

        /* ---- host helpers the engine does not cover ---- */
        "fs_home_dir" => call(req, |_| {
            Ok(json!(expand("~").to_string_lossy().to_string()))
        }),
        _ => return None,
    };
    Some(r)
}

/// Run one op body and wrap its `Result` in the host's reply envelope.
fn call(req: &Value, f: impl FnOnce(&Value) -> Result<Value, String>) -> Value {
    match f(req) {
        Ok(data) => json!({"ok": true, "data": data}),
        Err(e) => json!({"ok": false, "err": e}),
    }
}

/* ------------------------------- arg helpers ------------------------------ */

fn req_str(a: &Value, k: &str) -> Result<String, String> {
    a.get(k)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing or non-string argument `{k}`"))
}

/// A required path argument, `~`-expanded like every other path in this host.
fn req_path(a: &Value, k: &str) -> Result<PathBuf, String> {
    req_str(a, k).map(|s| expand(&s))
}

fn req_paths(a: &Value, k: &str) -> Result<Vec<PathBuf>, String> {
    a.get(k)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(expand))
                .collect::<Vec<_>>()
        })
        .ok_or_else(|| format!("missing or non-array argument `{k}`"))
}

fn opt_bool(a: &Value, k: &str) -> Option<bool> {
    a.get(k).and_then(Value::as_bool)
}

fn opt_u64(a: &Value, k: &str) -> Option<u64> {
    a.get(k).and_then(Value::as_u64)
}

fn opt_vec_str(a: &Value, k: &str) -> Option<Vec<String>> {
    a.get(k).and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    })
}

fn s(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

/* --------------------------------- shared -------------------------------- */

/// Format bytes to a human-readable string. Ported from the shared engine's
/// `format_size`, which the browser's size column expects verbatim.
fn format_size(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".into();
    }
    let units = ["B", "KB", "MB", "GB", "TB"];
    let i = ((bytes as f64).log(1024.0).floor() as usize).min(units.len() - 1);
    format!("{:.1} {}", bytes as f64 / 1024f64.powi(i as i32), units[i])
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// SHA-256 a whole file, streaming in 64 KiB blocks. `None` on any IO error.
fn sha256_file(path: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hex_encode(&hasher.finalize()))
}

/// `"%Y-%m-%d %H:%M"` in UTC, the format the browser's date columns render.
fn fmt_time(t: std::time::SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    dt.format("%Y-%m-%d %H:%M").to_string()
}

/// Recursive directory copy — `std::fs` has no built-in.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), dst_path)?;
        }
    }
    Ok(())
}

/// Pick the first free `"<stem> <word>[ n][.ext]"` beside `src`, Finder-style.
/// Shared by duplicate ("copy"). Bounded so a pathological directory can't hang.
fn free_sibling(src: &Path, word: &str) -> Result<PathBuf, String> {
    let parent = src
        .parent()
        .ok_or_else(|| format!("No parent directory for: {}", s(src)))?;
    let file_name = src
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("Invalid file name: {}", s(src)))?;
    // Dotfiles like `.bashrc` have no real extension: keep the whole name as the
    // stem so the copy is `.bashrc copy`, not `.bashrc copy.`.
    let (stem, ext) = match file_name.rsplit_once('.') {
        Some((st, e)) if !st.is_empty() => (st, Some(e)),
        _ => (file_name, None),
    };
    for n in 1..=1000u32 {
        let name = match ext {
            None if n == 1 => format!("{stem} {word}"),
            None => format!("{stem} {word} {n}"),
            Some(e) if n == 1 => format!("{stem} {word}.{e}"),
            Some(e) => format!("{stem} {word} {n}.{e}"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!("Too many {word} siblings (1000+)"))
}

/* ---------------------------- directory / info ---------------------------- */

fn list_dir(dir: &Path, include_hidden: Option<bool>) -> Result<Value, String> {
    let show_hidden = include_hidden.unwrap_or(false);
    if !dir.exists() {
        return Err(format!("Directory not found: {}", s(dir)));
    }
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", s(dir)));
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
        let ep = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // Nautilus convention: dotfiles hidden by default, Ctrl+H toggles.
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let meta = std::fs::metadata(&ep).ok();
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(fmt_time)
            .unwrap_or_default();
        // `created()` is `Err` on filesystems without birth-time (some ext4
        // mounts, older NFS); the empty string renders as the UI's "—".
        let created = meta
            .as_ref()
            .and_then(|m| m.created().ok())
            .map(fmt_time)
            .unwrap_or_default();
        entries.push(json!({
            "name": name,
            "path": s(&ep),
            "isDir": ep.is_dir(),
            "size": size,
            "sizeFormatted": format_size(size),
            "modified": modified,
            "created": created,
            "ext": ep.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default(),
        }));
    }
    // Directories first, then case-insensitive by name.
    entries.sort_by(|a, b| {
        let a_dir = a["isDir"].as_bool().unwrap_or(false);
        let b_dir = b["isDir"].as_bool().unwrap_or(false);
        b_dir.cmp(&a_dir).then_with(|| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });
    Ok(json!({ "entries": entries, "path": s(dir) }))
}

fn list_subdirs(dir: &Path, include_hidden: Option<bool>) -> Result<Value, String> {
    let show_hidden = include_hidden.unwrap_or(false);
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", s(dir)));
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
        let Ok(ty) = entry.file_type() else { continue };
        if !ty.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        out.push((name, s(&entry.path())));
    }
    out.sort_by_key(|(name, _)| name.to_lowercase());
    Ok(Value::Array(
        out.into_iter()
            .map(|(name, path)| json!({ "name": name, "path": path }))
            .collect(),
    ))
}

fn folder_size(root: &Path, timeout_ms: Option<u64>) -> Result<Value, String> {
    let timeout = std::time::Duration::from_millis(timeout_ms.unwrap_or(2000).clamp(100, 30_000));
    let deadline = std::time::Instant::now() + timeout;

    fn walk(path: &Path, deadline: std::time::Instant) -> Result<(u64, u64), String> {
        if std::time::Instant::now() > deadline {
            return Err("timeout".into());
        }
        let entries = std::fs::read_dir(path).map_err(|e| e.to_string())?;
        let (mut bytes, mut files) = (0u64, 0u64);
        for entry in entries.flatten() {
            if std::time::Instant::now() > deadline {
                return Err("timeout".into());
            }
            // `entry.metadata()` is `lstat` on Unix, so symlinks report neither
            // is_file nor is_dir and are skipped — the safe default, since
            // following them could cycle (`/a → /b → /a`) or double-count.
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_file() {
                bytes = bytes.saturating_add(meta.len());
                files = files.saturating_add(1);
            } else if meta.is_dir() {
                // Permission-denied subdirs are common; skip rather than abort.
                if let Ok((sb, sf)) = walk(&entry.path(), deadline) {
                    bytes = bytes.saturating_add(sb);
                    files = files.saturating_add(sf);
                }
            }
        }
        Ok((bytes, files))
    }

    let (bytes, files) = walk(root, deadline)?;
    Ok(json!({ "bytes": bytes, "files": files }))
}

fn get_info(p: &Path) -> Result<Value, String> {
    let symlink_meta = std::fs::symlink_metadata(p).map_err(|e| e.to_string())?;
    let is_symlink = symlink_meta.file_type().is_symlink();
    let symlink_target = if is_symlink {
        std::fs::read_link(p).ok().map(|t| s(&t))
    } else {
        None
    };
    // Follow the link for "actual content" stats; fall back to the link's own
    // metadata when the target is gone (a broken symlink).
    let meta = std::fs::metadata(p).unwrap_or_else(|_| symlink_meta.clone());
    let kind = if is_symlink {
        "symlink"
    } else if meta.is_dir() {
        "dir"
    } else if meta.is_file() {
        "file"
    } else {
        "other"
    };
    let to_ms = |t: std::time::SystemTime| -> Option<i64> {
        t.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_millis() as i64)
    };
    let (mode_octal, mode_string) = mode_strings(&meta, kind);
    // Recursive size + count for directories, bounded so `/` can't pin the
    // dispatcher. A partial number is acceptable — the user can re-run.
    let (size, item_count) = if meta.is_dir() && !is_symlink {
        const MAX_ENTRIES: u64 = 100_000;
        let mut total_size = 0u64;
        let mut count = 0u64;
        let mut stack = vec![p.to_path_buf()];
        while let Some(dir) = stack.pop() {
            if count >= MAX_ENTRIES {
                break;
            }
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in rd.flatten() {
                if count >= MAX_ENTRIES {
                    break;
                }
                count += 1;
                let Ok(em) = entry.metadata() else { continue };
                if em.is_dir() && !em.file_type().is_symlink() {
                    stack.push(entry.path());
                } else if em.is_file() {
                    total_size = total_size.saturating_add(em.len());
                }
            }
        }
        (total_size, Some(count))
    } else {
        (meta.len(), None)
    };
    let (uid, gid) = owner_ids(&meta);
    Ok(json!({
        "path": s(p),
        "name": p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| s(p)),
        "kind": kind,
        "size": size,
        "itemCount": item_count,
        "mtimeMs": meta.modified().ok().and_then(to_ms),
        "ctimeMs": meta.created().ok().and_then(to_ms),
        "atimeMs": meta.accessed().ok().and_then(to_ms),
        "modeOctal": mode_octal,
        "modeString": mode_string,
        "isReadonly": meta.permissions().readonly(),
        "isSymlink": is_symlink,
        "symlinkTarget": symlink_target,
        "uid": uid,
        "gid": gid,
    }))
}

/// `(0644, "-rw-r--r--")` on Unix; `(None, None)` elsewhere.
#[cfg(unix)]
fn mode_strings(meta: &std::fs::Metadata, kind: &str) -> (Option<String>, Option<String>) {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode();
    let mut out = String::with_capacity(10);
    out.push(match kind {
        "dir" => 'd',
        "symlink" => 'l',
        _ => '-',
    });
    for (bit, ch) in [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ] {
        out.push(if mode & bit != 0 { ch } else { '-' });
    }
    (Some(format!("{:04o}", mode & 0o7777)), Some(out))
}

#[cfg(not(unix))]
fn mode_strings(_meta: &std::fs::Metadata, _kind: &str) -> (Option<String>, Option<String>) {
    (None, None)
}

#[cfg(unix)]
fn owner_ids(meta: &std::fs::Metadata) -> (Option<u32>, Option<u32>) {
    use std::os::unix::fs::MetadataExt;
    (Some(meta.uid()), Some(meta.gid()))
}

#[cfg(not(unix))]
fn owner_ids(_meta: &std::fs::Metadata) -> (Option<u32>, Option<u32>) {
    (None, None)
}

/// Total / free space of the mount holding `p`. Needs the `sysinfo-caps`
/// feature (the same dependency the `sysinfo_*` stream uses); without it the
/// op reports its own absence rather than inventing a number.
#[cfg(feature = "sysinfo-caps")]
fn disk_usage(p: &Path) -> Result<Value, String> {
    let disks = sysinfo::Disks::new_with_refreshed_list();
    // Longest matching mount-point prefix — the most specific mount, so nested
    // mounts (`/home` under `/`) resolve to the inner one.
    let mut best: Option<(usize, &sysinfo::Disk)> = None;
    for d in disks.list().iter() {
        let mp = d.mount_point();
        if p.starts_with(mp) {
            let plen = mp.as_os_str().len();
            if best.is_none_or(|(l, _)| plen > l) {
                best = Some((plen, d));
            }
        }
    }
    let Some((_, d)) = best else {
        return Ok(Value::Null);
    };
    let total = d.total_space();
    let available = d.available_space();
    let used = total.saturating_sub(available);
    Ok(json!({
        "total": total,
        "available": available,
        "used": used,
        "usedPct": if total > 0 { (used as f64 / total as f64) * 100.0 } else { 0.0 },
        "mount": s(d.mount_point()),
    }))
}

#[cfg(not(feature = "sysinfo-caps"))]
fn disk_usage(_p: &Path) -> Result<Value, String> {
    Err("disk usage needs the `sysinfo-caps` feature".to_string())
}

#[cfg(unix)]
fn xattrs(p: &Path) -> Result<Value, String> {
    if !p.exists() {
        return Err(format!("Path does not exist: {}", s(p)));
    }
    // A filesystem without xattr support errors on `list`; that is "none", not
    // a failure the user needs to see.
    let Ok(iter) = xattr::list(p) else {
        return Ok(json!([]));
    };
    let out: Vec<Value> = iter
        .map(|n| {
            let size = xattr::get(p, &n)
                .ok()
                .flatten()
                .map(|v| v.len() as u64)
                .unwrap_or(0);
            json!({ "name": n.to_string_lossy(), "size": size })
        })
        .collect();
    Ok(Value::Array(out))
}

#[cfg(not(unix))]
fn xattrs(_p: &Path) -> Result<Value, String> {
    Ok(json!([]))
}

/// Map of absolute path → two-letter porcelain status code for `dir`'s repo.
///
/// This shells out to `git`, matching the shared engine. It is the one op here
/// that depends on an external program: reimplementing status against the index
/// and worktree would be a second, drifting answer to a question git already
/// answers exactly. A missing `git`, or a path outside any repo, yields an empty
/// map — the browser then simply draws no status badges.
fn git_status(dir: &Path) -> Result<Value, String> {
    if !dir.is_dir() {
        return Ok(json!({}));
    }
    // `--porcelain=v1 -z` emits `XY <path>\0`, avoiding the rename arrow and the
    // quoting rules of the non-`-z` form.
    let Ok(out) = std::process::Command::new("git")
        .current_dir(dir)
        .args(["status", "--porcelain=v1", "-z"])
        .output()
    else {
        return Ok(json!({})); // git not installed
    };
    if !out.status.success() {
        return Ok(json!({})); // not a repo, etc.
    }
    // Porcelain paths are repo-relative; the browser shows absolute paths.
    let toplevel = match std::process::Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return Ok(json!({})),
    };
    let root = PathBuf::from(&toplevel);
    let mut map = serde_json::Map::new();
    let mut iter = out.stdout.split(|&b| b == 0);
    while let Some(chunk) = iter.next() {
        // 2-byte code + space + path.
        if chunk.len() < 4 {
            continue;
        }
        let code = String::from_utf8_lossy(&chunk[0..2]).to_string();
        let rel = String::from_utf8_lossy(&chunk[3..]).to_string();
        // Renames/copies emit the ORIGINAL name as a second chunk; consume it so
        // the next turn doesn't read it as a fresh entry. Check before the move.
        let is_rename = code.starts_with('R') || code.starts_with('C');
        map.insert(s(&root.join(&rel)), json!(code));
        if is_rename {
            let _ = iter.next();
        }
    }
    Ok(Value::Object(map))
}

/* --------------------------------- mutate --------------------------------- */

fn rename_file(old: &Path, new: &Path) -> Result<Value, String> {
    std::fs::rename(old, new)
        .map(|_| Value::Null)
        .map_err(|e| e.to_string())
}

fn delete_file(p: &Path) -> Result<Value, String> {
    if !p.exists() {
        return Err("File not found".into());
    }
    if p.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
    .map(|_| Value::Null)
    .map_err(|e| e.to_string())
}

fn move_to_trash(p: &Path) -> Result<Value, String> {
    if !p.exists() {
        return Err("File not found".into());
    }
    trash::delete(p)
        .map(|_| Value::Null)
        .map_err(|e| e.to_string())
}

/// Overwrite a file's bytes with zeros, flush, then unlink. Directories are
/// refused — a recursive shred is too easy to fire by accident.
fn secure_delete(p: &Path) -> Result<Value, String> {
    use std::io::{Seek, SeekFrom, Write};
    let meta = std::fs::symlink_metadata(p).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        // Unlink the link itself; never shred what it points at.
        std::fs::remove_file(p).map_err(|e| e.to_string())?;
        return Ok(Value::Null);
    }
    if meta.is_dir() {
        return Err("Secure delete on directories is disabled — pick individual files".into());
    }
    let len = meta.len();
    if len > 0 {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(false)
            .open(p)
            .map_err(|e| format!("open for overwrite: {e}"))?;
        let zeros = [0u8; 64 * 1024];
        f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        let mut written = 0u64;
        while written < len {
            let chunk = ((len - written) as usize).min(zeros.len());
            f.write_all(&zeros[..chunk]).map_err(|e| e.to_string())?;
            written += chunk as u64;
        }
        // Flush before unlinking — the kernel may drop the dirty buffer once
        // the inode is gone, leaving the original bytes on disk.
        f.sync_all().map_err(|e| format!("sync: {e}"))?;
    }
    std::fs::remove_file(p).map_err(|e| e.to_string())?;
    Ok(Value::Null)
}

fn duplicate(src: &Path) -> Result<Value, String> {
    if !src.exists() {
        return Err(format!("Path does not exist: {}", s(src)));
    }
    let dest = free_sibling(src, "copy")?;
    if src.is_dir() {
        copy_dir_recursive(src, &dest).map_err(|e| e.to_string())?;
    } else {
        std::fs::copy(src, &dest).map_err(|e| e.to_string())?;
    }
    Ok(json!(s(&dest)))
}

fn copy_path(src: &Path, dest: &Path) -> Result<Value, String> {
    if !src.exists() {
        return Err(format!("Source does not exist: {}", s(src)));
    }
    if dest.exists() {
        return Err(format!("Destination already exists: {}", s(dest)));
    }
    if src.is_dir() {
        copy_dir_recursive(src, dest).map_err(|e| e.to_string())?;
    } else {
        std::fs::copy(src, dest).map_err(|e| e.to_string())?;
    }
    Ok(Value::Null)
}

fn create_dir(p: &Path) -> Result<Value, String> {
    if p.exists() {
        return Err(format!("Path already exists: {}", s(p)));
    }
    std::fs::create_dir(p)
        .map(|_| Value::Null)
        .map_err(|e| e.to_string())
}

fn create_file(p: &Path) -> Result<Value, String> {
    if p.exists() {
        return Err(format!("Path already exists: {}", s(p)));
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(p)
        .map(|_| Value::Null)
        .map_err(|e| e.to_string())
}

fn touch(p: &Path) -> Result<Value, String> {
    if !p.exists() {
        std::fs::File::create(p).map_err(|e| e.to_string())?;
    }
    let now = filetime::FileTime::now();
    filetime::set_file_times(p, now, now).map_err(|e| e.to_string())?;
    Ok(Value::Null)
}

#[cfg(unix)]
fn chmod(p: &Path, mode_octal: &str) -> Result<Value, String> {
    use std::os::unix::fs::PermissionsExt;
    if !p.exists() {
        return Err(format!("File not found: {}", s(p)));
    }
    let trimmed = mode_octal.trim().trim_start_matches('0');
    let mode = u32::from_str_radix(if trimmed.is_empty() { "0" } else { trimmed }, 8)
        .map_err(|e| format!("Invalid octal mode '{mode_octal}': {e}"))?;
    if mode > 0o7777 {
        return Err(format!("Mode out of range: {mode_octal}"));
    }
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
        .map(|_| Value::Null)
        .map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn chmod(_p: &Path, _mode_octal: &str) -> Result<Value, String> {
    Err("chmod is not supported on this platform".into())
}

#[cfg(unix)]
fn symlink_retarget(p: &Path, new_target: &str) -> Result<Value, String> {
    let meta = std::fs::symlink_metadata(p).map_err(|e| e.to_string())?;
    if !meta.file_type().is_symlink() {
        return Err(format!("Not a symlink: {}", s(p)));
    }
    std::fs::remove_file(p).map_err(|e| format!("unlink: {e}"))?;
    std::os::unix::fs::symlink(new_target, p).map_err(|e| format!("symlink: {e}"))?;
    Ok(Value::Null)
}

#[cfg(not(unix))]
fn symlink_retarget(_p: &Path, _new_target: &str) -> Result<Value, String> {
    Err("Symlink retarget not supported on this platform".into())
}

/* ---------------------------------- read ---------------------------------- */

fn read_file_base64(p: &Path, max_bytes: Option<u64>) -> Result<Value, String> {
    let cap = max_bytes
        .unwrap_or(2 * 1024 * 1024)
        .clamp(64 * 1024, 16 * 1024 * 1024);
    let meta = std::fs::metadata(p).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("Not a regular file".into());
    }
    if meta.len() > cap {
        return Err(format!(
            "File too large: {} bytes (cap {})",
            meta.len(),
            cap
        ));
    }
    let bytes = std::fs::read(p).map_err(|e| e.to_string())?;
    Ok(json!(crate::proto::b64_encode(&bytes)))
}

/// First `max_bytes` of a file (default 4 KiB, clamped 256..64 KiB).
fn head_bytes(p: &Path, max_bytes: Option<u64>) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let cap = max_bytes.unwrap_or(4 * 1024).clamp(256, 64 * 1024);
    let f = std::fs::File::open(p).map_err(|e| e.to_string())?;
    let mut buf = Vec::with_capacity(cap as usize);
    f.take(cap)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

fn read_head(p: &Path, max_bytes: Option<u64>) -> Result<Value, String> {
    Ok(json!(
        String::from_utf8_lossy(&head_bytes(p, max_bytes)?).into_owned()
    ))
}

fn read_head_bytes(p: &Path, max_bytes: Option<u64>) -> Result<Value, String> {
    Ok(json!(head_bytes(p, max_bytes)?))
}

/* ---------------------- search / compare / archive / hash ------------------ */

fn hash(p: &Path, algos: Option<Vec<String>>) -> Result<Value, String> {
    let want = algos.unwrap_or_else(|| vec!["sha256".into()]);
    let want_sha256 = want.iter().any(|a| a.eq_ignore_ascii_case("sha256"));
    let want_md5 = want.iter().any(|a| a.eq_ignore_ascii_case("md5"));
    if !want_sha256 && !want_md5 {
        return Err("No supported algorithm requested (sha256, md5)".into());
    }
    if !p.exists() {
        return Err(format!("File not found: {}", s(p)));
    }
    let meta = std::fs::metadata(p).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("Hashing folders is not supported".into());
    }
    let digest = sha256_file(p).ok_or_else(|| format!("read failed: {}", s(p)))?;
    let mut digests = serde_json::Map::new();
    if want_sha256 {
        digests.insert("sha256".into(), json!(digest));
    } else {
        // MD5 was asked for alone. There is no MD5 here, so return SHA-256 under
        // a name that says so rather than silently mislabelling the digest.
        digests.insert("sha256_md5_unavailable".into(), json!(digest));
    }
    Ok(json!({ "path": s(p), "size": meta.len(), "digests": digests }))
}

fn grep(
    root: &Path,
    needle: &str,
    case_insensitive: Option<bool>,
    max_results: Option<usize>,
) -> Result<Value, String> {
    use std::io::{BufRead, BufReader, Read};
    let ci = case_insensitive.unwrap_or(false);
    let limit = max_results.unwrap_or(500).min(5000);
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", s(root)));
    }
    if needle.is_empty() {
        return Err("Empty search needle".into());
    }
    let target = if ci {
        needle.to_lowercase()
    } else {
        needle.to_string()
    };
    let mut out: Vec<Value> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    'outer: while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if out.len() >= limit {
                break 'outer;
            }
            let path = entry.path();
            // Skip dotdirs (.git, .svn, …) and dotfiles.
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(ty) = entry.file_type() else { continue };
            if ty.is_dir() {
                stack.push(path);
                continue;
            }
            if !ty.is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() > 4 * 1024 * 1024 {
                continue; // skip files > 4 MiB
            }
            // Binary sniff: a NUL in the first 8 KiB means "not text".
            let Ok(mut f) = std::fs::File::open(&path) else {
                continue;
            };
            let mut probe = [0u8; 8192];
            let Ok(n) = f.read(&mut probe) else { continue };
            if probe[..n].contains(&0) {
                continue;
            }
            drop(f);
            let Ok(f) = std::fs::File::open(&path) else {
                continue;
            };
            for (i, line) in BufReader::new(f).lines().enumerate() {
                if out.len() >= limit {
                    break 'outer;
                }
                let Ok(text) = line else { break };
                let hay = if ci {
                    text.to_lowercase()
                } else {
                    text.clone()
                };
                if hay.contains(&target) {
                    out.push(json!({
                        "path": s(&path),
                        "line": (i as u64) + 1,
                        "text": text.chars().take(300).collect::<String>(),
                    }));
                }
            }
        }
    }
    Ok(Value::Array(out))
}

fn find_duplicates(
    root: &Path,
    recursive: Option<bool>,
    min_size_bytes: Option<u64>,
) -> Result<Value, String> {
    let recursive = recursive.unwrap_or(false);
    let min_size = min_size_bytes.unwrap_or(1);
    if !root.is_dir() {
        return Err(format!("Not a directory: {}", s(root)));
    }
    // Pass 1: bucket by (size, extension) — cheap, and files that differ in
    // either can't be identical, so only real candidates get hashed.
    let mut by_pre_key: HashMap<(u64, String), Vec<PathBuf>> = HashMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(ty) = entry.file_type() else { continue };
            if ty.is_dir() {
                if recursive {
                    stack.push(path);
                }
                continue;
            }
            if !ty.is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() < min_size {
                continue;
            }
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            by_pre_key.entry((meta.len(), ext)).or_default().push(path);
        }
    }
    // Pass 2: hash only buckets holding two or more candidates.
    let mut groups: Vec<(u64, String, Vec<String>)> = Vec::new();
    for ((size, _ext), paths) in by_pre_key {
        if paths.len() < 2 {
            continue;
        }
        let mut by_hash: HashMap<String, Vec<String>> = HashMap::new();
        for p in paths {
            if let Some(hex) = sha256_file(&p) {
                by_hash.entry(hex).or_default().push(s(&p));
            }
        }
        for (hex, paths) in by_hash {
            if paths.len() >= 2 {
                groups.push((size, hex, paths));
            }
        }
    }
    // Biggest reclaimable total first.
    groups.sort_by_key(|(size, _, paths)| std::cmp::Reverse(paths.len() as u64 * size));
    Ok(Value::Array(
        groups
            .into_iter()
            .map(|(size, hash, paths)| json!({ "hash": hash, "size": size, "paths": paths }))
            .collect(),
    ))
}

fn compare_dirs(dir_a: &Path, dir_b: &Path) -> Result<Value, String> {
    if !dir_a.is_dir() {
        return Err(format!("Not a directory: {}", s(dir_a)));
    }
    if !dir_b.is_dir() {
        return Err(format!("Not a directory: {}", s(dir_b)));
    }
    const MAX_ENTRIES: usize = 50_000;
    /// Relative path → (is_dir, size) for every non-hidden entry under `root`.
    fn walk(root: &Path, cap: usize) -> HashMap<String, (bool, u64)> {
        let mut out: HashMap<String, (bool, u64)> = HashMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            if out.len() >= cap {
                break;
            }
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in rd.flatten() {
                if out.len() >= cap {
                    break;
                }
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let path = entry.path();
                let Ok(ty) = entry.file_type() else { continue };
                let Ok(rel) = path.strip_prefix(root) else {
                    continue;
                };
                let rel = rel.to_string_lossy().to_string();
                if ty.is_dir() {
                    out.insert(rel, (true, 0));
                    stack.push(path);
                } else if ty.is_file() {
                    out.insert(rel, (false, entry.metadata().map(|m| m.len()).unwrap_or(0)));
                }
            }
        }
        out
    }
    // Files above this are assumed equal when their sizes match — hashing every
    // large media file would make a comparison unusable.
    const HASH_CAP: u64 = 16 * 1024 * 1024;
    let fingerprint = |path: &Path| -> Option<String> {
        let meta = std::fs::metadata(path).ok()?;
        if meta.len() > HASH_CAP {
            return None;
        }
        sha256_file(path)
    };

    let map_a = walk(dir_a, MAX_ENTRIES);
    let map_b = walk(dir_b, MAX_ENTRIES);
    let mut only_a: Vec<String> = Vec::new();
    let mut different: Vec<String> = Vec::new();
    for (rel, (a_is_dir, size_a)) in &map_a {
        let Some(&(b_is_dir, size_b)) = map_b.get(rel) else {
            only_a.push(rel.clone());
            continue;
        };
        if *a_is_dir != b_is_dir || (!*a_is_dir && *size_a != size_b) {
            different.push(rel.clone());
            continue;
        }
        if *a_is_dir {
            continue; // same rel path on both sides; contents compared per-entry
        }
        // Sizes agree — confirm with content hashes.
        match (fingerprint(&dir_a.join(rel)), fingerprint(&dir_b.join(rel))) {
            (Some(a), Some(b)) if a != b => different.push(rel.clone()),
            (None, None) => {} // both over the cap; sizes match, treat as same
            (None, _) | (_, None) => different.push(rel.clone()),
            _ => {}
        }
    }
    let mut only_b: Vec<String> = map_b
        .keys()
        .filter(|rel| !map_a.contains_key(*rel))
        .cloned()
        .collect();
    only_a.sort();
    only_b.sort();
    different.sort();
    Ok(json!({ "onlyInA": only_a, "onlyInB": only_b, "different": different }))
}

fn diff(path_a: &Path, path_b: &Path) -> Result<Value, String> {
    const MAX_BYTES: u64 = 4 * 1024 * 1024;
    let read_text = |p: &Path| -> Result<String, String> {
        let meta = std::fs::metadata(p).map_err(|e| format!("stat {}: {e}", s(p)))?;
        if !meta.is_file() {
            return Err(format!("Not a regular file: {}", s(p)));
        }
        if meta.len() > MAX_BYTES {
            return Err(format!("File too large to diff (> 4 MiB): {}", s(p)));
        }
        let bytes = std::fs::read(p).map_err(|e| e.to_string())?;
        if bytes.iter().take(8192).any(|&b| b == 0) {
            return Err(format!("Binary file: {}", s(p)));
        }
        Ok(String::from_utf8_lossy(&bytes).to_string())
    };
    let a = read_text(path_a)?;
    let b = read_text(path_b)?;
    let ops: Vec<Value> = similar::TextDiff::from_lines(&a, &b)
        .iter_all_changes()
        .map(|change| {
            let tag = match change.tag() {
                similar::ChangeTag::Equal => "equal",
                similar::ChangeTag::Delete => "delete",
                similar::ChangeTag::Insert => "insert",
            };
            let old_idx = change.old_index();
            let new_idx = change.new_index();
            json!({
                "tag": tag,
                "aLineStart": old_idx.unwrap_or(0),
                "aLineEnd": old_idx.map(|i| i + 1).unwrap_or(0),
                "bLineStart": new_idx.unwrap_or(0),
                "bLineEnd": new_idx.map(|i| i + 1).unwrap_or(0),
                "text": change.value().trim_end_matches('\n'),
            })
        })
        .collect();
    Ok(Value::Array(ops))
}

fn compress(paths: &[PathBuf], archive_path: &Path) -> Result<Value, String> {
    if paths.is_empty() {
        return Err("No paths to compress".into());
    }
    if archive_path.exists() {
        return Err(format!("Archive already exists: {}", s(archive_path)));
    }
    let file = std::fs::File::create(archive_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    /// Add `current` (file or directory, recursively) under `root`'s parent, so
    /// the archive holds `<name>/…` rather than the whole absolute path.
    fn add_path(
        zip: &mut zip::ZipWriter<std::fs::File>,
        options: zip::write::SimpleFileOptions,
        root: &Path,
        current: &Path,
    ) -> Result<(), String> {
        use std::io::{Read, Write};
        let rel = current
            .strip_prefix(root.parent().unwrap_or(root))
            .map_err(|e| e.to_string())?;
        let rel_str = rel.to_string_lossy().to_string();
        if current.is_dir() {
            // A trailing slash is how the zip spec marks a directory entry.
            zip.add_directory(format!("{}/", rel_str.trim_end_matches('/')), options)
                .map_err(|e| e.to_string())?;
            for entry in std::fs::read_dir(current).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                add_path(zip, options, root, &entry.path())?;
            }
        } else {
            zip.start_file(rel_str, options)
                .map_err(|e| e.to_string())?;
            let mut f = std::fs::File::open(current).map_err(|e| e.to_string())?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            zip.write_all(&buf).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    for p in paths {
        if !p.exists() {
            return Err(format!("Source does not exist: {}", s(p)));
        }
        add_path(&mut zip, options, p, p)?;
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(json!(s(archive_path)))
}

fn extract(archive: &Path, dest: &Path) -> Result<Value, String> {
    use std::io::{Read, Write};
    if !archive.exists() {
        return Err(format!("Archive does not exist: {}", s(archive)));
    }
    if dest.exists() {
        return Err(format!("Destination already exists: {}", s(dest)));
    }
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    // Lowercase the tail so `.ZIP` / `.Tar.GZ` match too.
    let lower = s(archive).to_lowercase();
    let file = || std::fs::File::open(archive).map_err(|e| e.to_string());
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file()?));
        tar.set_overwrite(false);
        tar.unpack(dest).map_err(|e| e.to_string())?;
    } else if lower.ends_with(".tar") {
        let mut tar = tar::Archive::new(file()?);
        tar.set_overwrite(false);
        tar.unpack(dest).map_err(|e| e.to_string())?;
    } else if lower.ends_with(".zip") {
        let mut zip = zip::ZipArchive::new(file()?).map_err(|e| e.to_string())?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
            // Zip-slip guard: `enclosed_name` is None for `..` or absolute
            // components, which would otherwise write outside `dest`.
            let Some(rel) = entry.enclosed_name() else {
                continue;
            };
            let out_path = dest.join(rel);
            if entry.is_dir() {
                std::fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
                continue;
            }
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut out_file = std::fs::File::create(&out_path).map_err(|e| e.to_string())?;
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            out_file.write_all(&buf).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode));
            }
        }
    } else {
        return Err("Unsupported archive format. Supported: .zip, .tar, .tar.gz, .tgz".into());
    }
    Ok(json!(s(dest)))
}
