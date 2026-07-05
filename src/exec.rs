//! One-shot subprocess execution.
//!
//! `{"cmd":"exec","program":"git","args":["status"],...}` runs a program to
//! completion and returns its exit code plus base64 stdout/stderr. This is the
//! bridge that lets any client (emacs, a plugin, another language) drive the
//! toolchain through the host instead of shelling out itself.
use crate::proto::b64_encode;
use crate::store::expand;
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

/// Run the `exec` request and return the reply object. Recognised fields:
/// `program` (required), `args`, `cwd`, `env` (map of string→string, merged onto
/// the inherited environment), `stdin` (UTF-8 fed to the child).
pub fn run(req: &Value) -> Value {
    let Some(program) = req["program"].as_str() else {
        return json!({"ok": false, "err": "no_program"});
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

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "err": e.to_string()}),
    };
    // Write stdin if given, then close it so the child sees EOF.
    if let Some(mut si) = child.stdin.take() {
        if let Some(input) = req["stdin"].as_str() {
            let _ = si.write_all(input.as_bytes());
        }
    }
    match child.wait_with_output() {
        Ok(o) => json!({
            "ok": true,
            "code": o.status.code(),
            "stdout": b64_encode(&o.stdout),
            "stderr": b64_encode(&o.stderr),
        }),
        Err(e) => json!({"ok": false, "err": e.to_string()}),
    }
}
