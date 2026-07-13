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
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

/// Chrome caps a single native-messaging message (host→extension) at 1 MiB
/// (1_048_576 bytes); a larger frame makes Chrome terminate the host, which
/// drops the LSP port and shows "○ LSP unavailable" in the HUD. stryke's
/// completion can return tens of thousands of items (>2 MB) for some cursor
/// contexts, so cap the outbound frame just under the limit with headroom.
const NATIVE_MSG_CAP: usize = 1_000_000;

/// Byte length of the `stryke-lsp-rx` frame that would carry `payload` — i.e.
/// exactly what Chrome measures against its 1 MiB host→extension limit.
fn frame_len(payload: &str) -> usize {
    serde_json::to_vec(&json!({"ev": "stryke-lsp-rx", "message": payload}))
        .map(|v| v.len())
        .unwrap_or(usize::MAX)
}

/// Wrap one raw LSP payload for delivery to the HUD, keeping the frame under
/// Chrome's native-messaging cap. Small payloads forward verbatim. An oversized
/// completion response is trimmed (fewer items, `isIncomplete:true` so Monaco
/// re-queries as the prefix narrows); any other oversized response is answered
/// with an empty result so the client's pending request still resolves; an
/// oversized notification (no id to answer) is dropped via a no-op frame the
/// client ignores — never forwarded, which would kill the port.
fn lsp_frame(payload: &str) -> Value {
    if frame_len(payload) <= NATIVE_MSG_CAP {
        return json!({"ev": "stryke-lsp-rx", "message": payload});
    }
    if let Some(trimmed) = trim_oversized_completion(payload) {
        return json!({"ev": "stryke-lsp-rx", "message": trimmed});
    }
    if let Ok(v) = serde_json::from_str::<Value>(payload) {
        if let Some(id) = v.get("id").filter(|id| !id.is_null()) {
            let empty = json!({"jsonrpc": "2.0", "id": id, "result": null}).to_string();
            return json!({"ev": "stryke-lsp-rx", "message": empty});
        }
    }
    json!({"ev": "stryke-lsp-noop"})
}

/// If `payload` is an oversized completion response, return a trimmed copy whose
/// frame fits `NATIVE_MSG_CAP`, preserving the JSON-RPC `id`. `None` when it is
/// not a completion response. stryke returns completion as either a bare
/// `CompletionItem[]` or a `{isIncomplete, items:[…]}` list — both are handled.
fn trim_oversized_completion(payload: &str) -> Option<String> {
    let v: Value = serde_json::from_str(payload).ok()?;
    let id = v.get("id").filter(|id| !id.is_null())?.clone();
    let result = v.get("result")?;
    let items: Vec<Value> = if let Some(arr) = result.as_array() {
        arr.clone()
    } else {
        result.get("items").and_then(|i| i.as_array())?.clone()
    };
    if items.is_empty() {
        return None;
    }
    // Halve the kept count until the framed message fits (or a lone giant item
    // forces an empty list). Monaco filters client-side, so the head is fine.
    let mut keep = items.len();
    loop {
        let slice = Value::Array(items[..keep].to_vec());
        let list = json!({
            "jsonrpc": "2.0",
            "id": id.clone(),
            "result": { "isIncomplete": true, "items": slice },
        });
        if let Ok(s) = serde_json::to_string(&list) {
            if frame_len(&s) <= NATIVE_MSG_CAP {
                return Some(s);
            }
        }
        if keep <= 1 {
            let empty = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "isIncomplete": true, "items": [] },
            });
            return serde_json::to_string(&empty).ok();
        }
        keep /= 2;
    }
}

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
                    let _ = send_msg(&out, &lsp_frame(&payload));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Small payloads pass through verbatim inside a `stryke-lsp-rx` frame.
    #[test]
    fn small_payload_forwarded_verbatim() {
        let payload = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let frame = lsp_frame(payload);
        assert_eq!(frame["ev"], "stryke-lsp-rx");
        assert_eq!(frame["message"], payload);
    }

    /// An oversized bare-array completion is trimmed to a fitting frame that
    /// keeps the JSON-RPC id and marks the list incomplete for re-query.
    #[test]
    fn oversized_array_completion_is_trimmed() {
        let items: Vec<Value> = (0..40_000)
            .map(|i| json!({"label": format!("symbol_number_{i}"), "kind": 6}))
            .collect();
        let payload = json!({"jsonrpc": "2.0", "id": 7, "result": items}).to_string();
        assert!(frame_len(&payload) > NATIVE_MSG_CAP, "fixture must be oversized");

        let frame = lsp_frame(&payload);
        assert_eq!(frame["ev"], "stryke-lsp-rx");
        let msg = frame["message"].as_str().unwrap();
        assert!(frame_len(msg) <= NATIVE_MSG_CAP, "trimmed frame must fit the cap");
        let v: Value = serde_json::from_str(msg).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["result"]["isIncomplete"], true);
        let kept = v["result"]["items"].as_array().unwrap().len();
        assert!(kept > 0 && kept < 40_000, "some items kept, but not all: {kept}");
    }

    /// The `{isIncomplete, items}` completion shape is trimmed the same way.
    #[test]
    fn oversized_list_completion_is_trimmed() {
        let items: Vec<Value> = (0..40_000)
            .map(|i| json!({"label": format!("symbol_number_{i}")}))
            .collect();
        let payload =
            json!({"jsonrpc":"2.0","id":"c9","result":{"isIncomplete":false,"items":items}})
                .to_string();
        let frame = lsp_frame(&payload);
        let msg = frame["message"].as_str().unwrap();
        assert!(frame_len(msg) <= NATIVE_MSG_CAP);
        let v: Value = serde_json::from_str(msg).unwrap();
        assert_eq!(v["id"], "c9");
        assert_eq!(v["result"]["isIncomplete"], true);
    }

    /// A non-trimmable oversized response (e.g. a giant hover) still resolves the
    /// client's request — with an empty result — rather than forwarding a frame
    /// Chrome would reject.
    #[test]
    fn oversized_non_completion_response_resolves_empty() {
        let huge = "x".repeat(NATIVE_MSG_CAP + 10);
        let payload = json!({"jsonrpc":"2.0","id":42,"result":{"contents":huge}}).to_string();
        let frame = lsp_frame(&payload);
        assert_eq!(frame["ev"], "stryke-lsp-rx");
        let msg = frame["message"].as_str().unwrap();
        assert!(frame_len(msg) <= NATIVE_MSG_CAP);
        let v: Value = serde_json::from_str(msg).unwrap();
        assert_eq!(v["id"], 42);
        assert!(v["result"].is_null());
    }

    /// An oversized notification (no id to answer) becomes a no-op frame the HUD
    /// ignores — never an oversized frame that would drop the port.
    #[test]
    fn oversized_notification_becomes_noop() {
        let huge = "d".repeat(NATIVE_MSG_CAP + 10);
        let payload = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": "file:///a", "diagnostics": [{"message": huge}]}
        })
        .to_string();
        let frame = lsp_frame(&payload);
        assert_eq!(frame["ev"], "stryke-lsp-noop");
    }
}
