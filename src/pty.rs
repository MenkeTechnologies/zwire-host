//! Multiplexed PTY sessions.
//!
//! `pty_spawn` opens a login shell (or any program) in a pseudo-terminal and
//! streams its output as `{"ev":"output","b64":…}` frames. Sessions are keyed by
//! the request's `id`, so one connection can drive many terminals at once —
//! `pty_write` / `pty_resize` / `pty_kill` route by the same `id`. An absent
//! `id` uses the default (empty) key and omits the `pty` field from frames, so
//! the original single-terminal zwire protocol is unchanged.
use crate::proto::{b64_encode, send_msg, Out};
use crate::store::expand;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::thread::JoinHandle;

/// One live pseudo-terminal: the master write side, a resize handle, the child
/// process, and the reader thread pumping output frames.
pub struct PtySession {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    reader: Option<JoinHandle<()>>,
}

fn default_shell() -> String {
    if cfg!(windows) {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

fn output_frame(key: &str, bytes: &[u8]) -> Value {
    let b64 = b64_encode(bytes);
    if key.is_empty() {
        json!({ "ev": "output", "b64": b64 })
    } else {
        json!({ "ev": "output", "pty": key, "b64": b64 })
    }
}

fn exit_frame(key: &str) -> Value {
    if key.is_empty() {
        json!({ "ev": "exit" })
    } else {
        json!({ "ev": "exit", "pty": key })
    }
}

impl PtySession {
    /// Spawn a terminal from a `pty_spawn` request. `key` is the session id used
    /// to tag frames (empty = legacy default). Returns `None` if the OS refuses
    /// to open the PTY or spawn the program.
    ///
    /// Recognised request fields: `rows`, `cols`, `shell` (program to run,
    /// defaults to `$SHELL`), `args` (defaults to `["-l"]`), `cwd`, `env`.
    pub fn spawn(out: &Out, req: &Value, key: String) -> Option<PtySession> {
        let rows = req["rows"].as_u64().unwrap_or(24) as u16;
        let cols = req["cols"].as_u64().unwrap_or(80) as u16;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .ok()?;

        let program = req["shell"]
            .as_str()
            .map_or_else(default_shell, String::from);
        let mut cmd = CommandBuilder::new(&program);
        match req["args"].as_array() {
            Some(args) => {
                for a in args {
                    if let Some(s) = a.as_str() {
                        cmd.arg(s);
                    }
                }
            }
            None => cmd.arg("-l"),
        }
        if let Some(cwd) = req["cwd"].as_str() {
            cmd.cwd(expand(cwd));
        }
        cmd.env("TERM", "xterm-256color");
        if let Some(env) = req["env"].as_object() {
            for (k, v) in env {
                if let Some(s) = v.as_str() {
                    cmd.env(k, s);
                }
            }
        }

        let child = pair.slave.spawn_command(cmd).ok()?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().ok()?;
        let writer = pair.master.take_writer().ok()?;

        let out2 = out.clone();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if send_msg(&out2, &output_frame(&key, &buf[..n])).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = send_msg(&out2, &exit_frame(&key));
        });

        Some(PtySession {
            writer,
            master: pair.master,
            child,
            reader: Some(handle),
        })
    }

    /// Feed bytes to the terminal (`pty_write`). Accepts `data` (UTF-8 string)
    /// or `b64` (binary) on the request.
    pub fn write(&mut self, req: &Value) {
        let bytes = if let Some(s) = req["data"].as_str() {
            s.as_bytes().to_vec()
        } else if let Some(b) = req["b64"].as_str() {
            crate::proto::b64_decode(b).unwrap_or_default()
        } else {
            return;
        };
        let _ = self.writer.write_all(&bytes);
        let _ = self.writer.flush();
    }

    /// Resize the terminal (`pty_resize`).
    pub fn resize(&self, req: &Value) {
        let rows = req["rows"].as_u64().unwrap_or(24) as u16;
        let cols = req["cols"].as_u64().unwrap_or(80) as u16;
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}
