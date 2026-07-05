//! Background jobs — fire-and-forget long-running commands.
//!
//! `job_start` spawns a command on a daemon-owned thread and returns a job id
//! *immediately*, so a client (an editor, a plugin) can submit a slow build or
//! test run and get on with its life. The job keeps running even after the
//! submitting connection closes, because the registry is process-global rather
//! than per-connection.
//!
//! Completion is delivered two ways:
//!   * a desktop **notification** fired by the daemon itself (via the `notify`
//!     capability) — so you're told the moment it finishes, editor focused or
//!     not; and
//!   * a **poll/collect** path (`job_poll` / `job_list` / `job_result`) so a
//!     client can pull the exit code and captured output back into itself.
use crate::exec;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Keep at most this many finished-but-uncollected jobs; oldest are dropped so a
/// client that never polls can't leak memory. Their notifications still fired.
const MAX_FINISHED: usize = 200;
/// Cap captured output per stream at 4 MiB so a chatty job can't blow up the
/// registry.
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

struct Job {
    id: u64,
    label: String,
    done: bool,
    code: Option<i64>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    error: Option<String>,
}

impl Job {
    fn result_value(&self) -> Value {
        json!({
            "ok": true,
            "id": self.id,
            "label": self.label,
            "done": true,
            "code": self.code,
            "error": self.error,
            "stdout": crate::proto::b64_encode(&self.stdout),
            "stderr": crate::proto::b64_encode(&self.stderr),
        })
    }
}

#[derive(Default)]
struct Registry {
    next_id: u64,
    jobs: HashMap<u64, Job>,
}

fn registry() -> &'static Mutex<Registry> {
    static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Registry::default()))
}

fn cap(mut v: Vec<u8>) -> Vec<u8> {
    v.truncate(MAX_OUTPUT);
    v
}

/// Handle any `job_*` command; returns the reply object.
pub fn handle(cmd: &str, req: &Value) -> Value {
    match cmd {
        "job_start" => start(req),
        "job_poll" => poll(),
        "job_list" => list(),
        "job_result" => result(req),
        _ => json!({"ok": false, "err": "unknown_cmd"}),
    }
}

/// Spawn a background job. Fields: `program` (required), `args`, `cwd`, `env`,
/// `stdin` (as [`exec::run_raw`]), plus `label` (shown in the notification and
/// listings) and `notify` (fire a desktop notification on completion, default
/// true). Replies `{"ok":true,"job":<id>}` immediately.
fn start(req: &Value) -> Value {
    if req["program"].as_str().is_none() {
        return json!({"ok": false, "err": "no_program"});
    }
    let label = req["label"]
        .as_str()
        .or_else(|| req["program"].as_str())
        .unwrap_or("job")
        .to_string();
    let notify = req["notify"].as_bool().unwrap_or(true);

    let id = {
        let mut reg = registry().lock().unwrap();
        reg.next_id += 1;
        let id = reg.next_id;
        reg.jobs.insert(
            id,
            Job {
                id,
                label: label.clone(),
                done: false,
                code: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                error: None,
            },
        );
        id
    };

    let req = req.clone();
    std::thread::spawn(move || {
        let outcome = exec::run_raw(&req);
        let mut summary = String::new();
        {
            let mut reg = registry().lock().unwrap();
            if let Some(job) = reg.jobs.get_mut(&id) {
                job.done = true;
                match outcome {
                    Ok(res) => {
                        job.code = res.code.map(i64::from);
                        job.stdout = cap(res.stdout);
                        job.stderr = cap(res.stderr);
                        summary = match res.code {
                            Some(0) => "completed".to_string(),
                            Some(c) => format!("exited {c}"),
                            None => "terminated".to_string(),
                        };
                    }
                    Err(e) => {
                        summary = format!("failed to start: {e}");
                        job.error = Some(e);
                    }
                }
            }
            evict_overflow(&mut reg);
        }
        if notify {
            crate::osops::handle(
                "notify",
                &json!({
                    "title": format!("zwire-host: {label}"),
                    "body": format!("job #{id} {summary}"),
                }),
            );
        }
    });

    json!({"ok": true, "job": id})
}

/// Drop the oldest finished jobs beyond [`MAX_FINISHED`].
fn evict_overflow(reg: &mut Registry) {
    let mut finished: Vec<u64> = reg.jobs.values().filter(|j| j.done).map(|j| j.id).collect();
    if finished.len() <= MAX_FINISHED {
        return;
    }
    finished.sort_unstable();
    for id in &finished[..finished.len() - MAX_FINISHED] {
        reg.jobs.remove(id);
    }
}

/// Drain every finished job, returning their full results and removing them.
/// This is the "tell me what completed" call for a polling client.
fn poll() -> Value {
    let mut reg = registry().lock().unwrap();
    let done: Vec<u64> = reg.jobs.values().filter(|j| j.done).map(|j| j.id).collect();
    let mut results: Vec<Value> = done
        .iter()
        .filter_map(|id| reg.jobs.remove(id).map(|j| j.result_value()))
        .collect();
    results.sort_by_key(|v| v["id"].as_u64().unwrap_or(0));
    json!({"ok": true, "jobs": results})
}

/// Non-destructive status of every known job.
fn list() -> Value {
    let reg = registry().lock().unwrap();
    let mut jobs: Vec<Value> = reg
        .jobs
        .values()
        .map(|j| json!({"id": j.id, "label": j.label, "running": !j.done, "code": j.code}))
        .collect();
    jobs.sort_by_key(|v| v["id"].as_u64().unwrap_or(0));
    json!({"ok": true, "jobs": jobs})
}

/// Fetch one job by `id`. A finished job is returned and removed; a still-running
/// one replies `{"ok":true,"running":true}`.
fn result(req: &Value) -> Value {
    let Some(id) = req["id"].as_u64() else {
        return json!({"ok": false, "err": "no_id"});
    };
    let mut reg = registry().lock().unwrap();
    match reg.jobs.get(&id) {
        None => json!({"ok": false, "err": "no_such_job"}),
        Some(j) if !j.done => json!({"ok": true, "id": id, "running": true}),
        Some(_) => reg.jobs.remove(&id).unwrap().result_value(),
    }
}
