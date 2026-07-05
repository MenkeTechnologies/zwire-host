//! Process tools: list (`ps`), control (`kill`), and PATH lookup (`which`).
//!
//! `ps`/`kill` reuse the already-present `sysinfo` crate, so they add no
//! dependencies; `which` is plain PATH scanning. Together they turn the host
//! into a small activity monitor any app can drive.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Dispatch a process command.
pub fn handle(cmd: &str, req: &Value) -> Value {
    match cmd {
        "ps" => ps(req),
        "kill" => kill(req),
        "which" => which(req),
        _ => json!({"ok": false, "err": "unknown_cmd"}),
    }
}

/// `{"cmd":"ps","filter"?:sub,"limit"?:N}` — processes sorted by memory, biggest
/// first. `filter` keeps only names containing the substring; `limit` caps the
/// list (default 50).
fn ps(req: &Value) -> Value {
    use sysinfo::System;
    let sys = System::new_all();
    let filter = req["filter"].as_str();
    let limit = req["limit"].as_u64().unwrap_or(50) as usize;

    let mut procs: Vec<(u32, String, u64, f32)> = sys
        .processes()
        .values()
        .map(|p| {
            (
                p.pid().as_u32(),
                p.name().to_string_lossy().into_owned(),
                p.memory(),
                p.cpu_usage(),
            )
        })
        .filter(|(_, name, _, _)| filter.is_none_or(|f| name.contains(f)))
        .collect();
    procs.sort_unstable_by_key(|p| std::cmp::Reverse(p.2));
    procs.truncate(limit);

    let list: Vec<Value> = procs
        .into_iter()
        .map(|(pid, name, mem, cpu)| {
            json!({"pid": pid, "name": name, "mem": mem, "cpu": (cpu * 100.0).round() / 100.0})
        })
        .collect();
    json!({"ok": true, "procs": list})
}

/// `{"cmd":"kill","pid":N,"signal"?:"term"|"kill"}` — signal a process. Defaults
/// to a graceful `term`, falling back to a hard kill if the platform lacks the
/// requested signal.
fn kill(req: &Value) -> Value {
    use sysinfo::{Pid, Signal, System};
    let Some(pid) = req["pid"].as_u64() else {
        return json!({"ok": false, "err": "no_pid"});
    };
    let pid = Pid::from_u32(pid as u32);
    let sys = System::new_all();
    let Some(proc_) = sys.process(pid) else {
        return json!({"ok": false, "err": "no_such_process"});
    };
    let signal = match req["signal"].as_str() {
        Some("kill") => Signal::Kill,
        _ => Signal::Term,
    };
    let ok = proc_.kill_with(signal).unwrap_or_else(|| proc_.kill());
    json!({ "ok": ok })
}

/// `{"cmd":"which","program":"git"}` — resolve a program to an absolute path via
/// `$PATH` (or check it directly if it already contains a path separator).
fn which(req: &Value) -> Value {
    let Some(program) = req["program"].as_str() else {
        return json!({"ok": false, "err": "no_program"});
    };
    let path = locate(program).map(|p| p.to_string_lossy().into_owned());
    json!({"ok": true, "path": path})
}

fn has_separator(p: &str) -> bool {
    p.contains('/') || (cfg!(windows) && p.contains('\\'))
}

fn locate(program: &str) -> Option<PathBuf> {
    if has_separator(program) {
        let p = crate::store::expand(program);
        return is_executable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(program);
        if is_executable(&cand) {
            return Some(cand);
        }
        #[cfg(windows)]
        for ext in ["exe", "cmd", "bat", "com"] {
            let c = dir.join(format!("{program}.{ext}"));
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}
