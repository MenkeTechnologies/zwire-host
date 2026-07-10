//! Bridge to the stryke language server (`stryke --lsp`) for the Hooks editor.
//!
//! A single long-lived `stryke --lsp` child speaks LSP JSON-RPC over stdio with
//! `Content-Length` framing. The frontend's in-editor LSP adapter (Monaco)
//! exchanges **unframed** JSON strings, so this module adds framing on the way to
//! the server and strips it on the way back:
//!   * [`StrykeLsp::send`] frames a JSON string and writes it to the child's stdin.
//!   * a reader thread parses framed messages from stdout and pushes each raw JSON
//!     payload as a `{"ev":"stryke-lsp-rx","message":…}` frame.
//!
//! Ported from the Audio-Haxor engine (`src-tauri/src/stryke_lsp.rs`). The framing
//! reader loop and stdin writer are carried over verbatim; re-hosted onto
//! zwire-host's session model — the child is owned by the session that started it
//! (dropped ⇒ killed, like a PTY/watcher), and Tauri's `app.emit` becomes a
//! `send_msg` push on that session's `Out`.

use crate::proto::{send_msg, Out};
use crate::stryke_runner::resolve_stryke;
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

/// A running `stryke --lsp` child owned by one session. Dropping it kills the
/// server, which unblocks the reader thread (its next read returns EOF).
pub struct StrykeLsp {
    child: Child,
    stdin: ChildStdin,
}

impl Drop for StrykeLsp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl StrykeLsp {
    /// Spawn `stryke --lsp`; a reader thread pushes `stryke-lsp-rx` frames on `out`
    /// and a final `stryke-lsp-exit` when the server closes stdout.
    pub fn start(out: &Out) -> Result<StrykeLsp, String> {
        let stryke =
            resolve_stryke().ok_or_else(|| "stryke binary not found on PATH".to_string())?;

        let mut child = Command::new(&stryke)
            .arg("--lsp")
            // Let scripts/hooks resolve `App::here()` to this app over the automation bus.
            .env("ZGUI_APP", "zwire")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("spawn stryke --lsp: {e}"))?;

        let stdin = child.stdin.take().ok_or("no stdin on stryke --lsp")?;
        let stdout = child.stdout.take().ok_or("no stdout on stryke --lsp")?;

        // Reader thread: parse Content-Length frames → push raw JSON payloads.
        let out = out.clone();
        std::thread::Builder::new()
            .name("stryke-lsp-reader".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    let mut content_length: usize = 0;
                    // Read headers until the blank line.
                    loop {
                        let mut line = String::new();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => {
                                let _ = send_msg(&out, &json!({"ev": "stryke-lsp-exit"}));
                                return;
                            }
                            Ok(_) => {}
                        }
                        let trimmed = line.trim_end_matches(['\r', '\n']);
                        if trimmed.is_empty() {
                            break;
                        }
                        if let Some(v) = trimmed.strip_prefix("Content-Length:") {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    if content_length == 0 {
                        continue;
                    }
                    let mut buf = vec![0u8; content_length];
                    if reader.read_exact(&mut buf).is_err() {
                        let _ = send_msg(&out, &json!({"ev": "stryke-lsp-exit"}));
                        return;
                    }
                    let payload = String::from_utf8_lossy(&buf).into_owned();
                    let _ = send_msg(&out, &json!({"ev": "stryke-lsp-rx", "message": payload}));
                }
            })
            .map_err(|e| e.to_string())?;

        Ok(StrykeLsp { child, stdin })
    }

    /// Send one unframed LSP JSON-RPC message to the server (adds Content-Length).
    pub fn send(&mut self, message: &str) -> Result<(), String> {
        // LSP Content-Length is the byte length of the payload.
        let header = format!("Content-Length: {}\r\n\r\n", message.len());
        self.stdin
            .write_all(header.as_bytes())
            .and_then(|_| self.stdin.write_all(message.as_bytes()))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("write to stryke --lsp: {e}"))
    }
}
