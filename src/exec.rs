//! Subprocess execution.
//!
//! `{"cmd":"exec","program":"git","args":["status"],...}` runs a program to
//! completion and returns its exit code plus base64 stdout/stderr. This is the
//! bridge that lets any client (emacs, a plugin, another language) drive the
//! toolchain through the host instead of shelling out itself. The same core
//! ([`run_raw`]) backs the background [`crate::jobs`] runner.
use crate::proto::b64_encode;
use crate::store::expand;
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

/// Captured result of running a command to completion.
pub struct ExecResult {
    /// Exit code, or `None` if the process was killed by a signal.
    pub code: Option<i32>,
    /// Raw stdout bytes.
    pub stdout: Vec<u8>,
    /// Raw stderr bytes.
    pub stderr: Vec<u8>,
}

/// Run a command to completion, returning raw captured output. Recognised
/// fields: `program` (required), `args`, `cwd`, `env` (string→string, merged
/// onto the inherited environment), `stdin` (UTF-8 fed to the child).
pub fn run_raw(req: &Value) -> Result<ExecResult, String> {
    let Some(program) = req["program"].as_str() else {
        return Err("no_program".to_string());
    };
    let mut cmd = Command::new(program);
    if let Some(args) = req["args"].as_array() {
        for a in args {
            if let Some(s) = a.as_str() {
                cmd.arg(s);
            }
        }
    }
    if let Some(cwd) = req["cwd"].as_str() {
        cmd.current_dir(expand(cwd));
    }
    if let Some(env) = req["env"].as_object() {
        for (k, v) in env {
            if let Some(s) = v.as_str() {
                cmd.env(k, s);
            }
        }
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    // Write stdin if given, then close it so the child sees EOF.
    if let Some(mut si) = child.stdin.take() {
        if let Some(input) = req["stdin"].as_str() {
            let _ = si.write_all(input.as_bytes());
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    Ok(ExecResult {
        code: out.status.code(),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

/// Run the `exec` request and return the reply object (base64 stdout/stderr).
pub fn run(req: &Value) -> Value {
    match run_raw(req) {
        Ok(r) => json!({
            "ok": true,
            "code": r.code,
            "stdout": b64_encode(&r.stdout),
            "stderr": b64_encode(&r.stderr),
        }),
        Err(e) => json!({"ok": false, "err": e}),
    }
}
