//! Wire transport + small reply helpers, shared by every transport.
//!
//! The host speaks two framings over one JSON message model:
//!   * **Native messaging** (Chrome): a little-endian `u32` byte length followed
//!     by a UTF-8 JSON body.
//!   * **NDJSON** (everything else — sockets, tmux, emacs, scripts, other
//!     languages): one JSON object per line.
//!
//! A [`Peer`] is the write half of a connection; it frames outgoing messages
//! per its mode. Capabilities (RPC replies, sysinfo frames, PTY output) only
//! ever see [`Out`] and never care which framing is in use.
use base64::Engine;
use serde_json::Value;
use std::io::{self, BufRead, Read, Write};
use std::sync::{Arc, Mutex};

/// Largest inbound message we will allocate for (128 MiB). Guards against a
/// bogus length prefix asking us to buffer the whole address space.
const MAX_MSG: usize = 128 * 1024 * 1024;

/// Outbound framing for a connection.
#[derive(Clone, Copy)]
pub enum Framing {
    /// Chrome native messaging: `u32` little-endian length prefix + JSON body.
    Native,
    /// One compact JSON object per line.
    Ndjson,
    /// The zgui-bridge automation bus: NDJSON, but every message is wrapped in
    /// the frame that bus speaks — a reply when it carries a correlation `id`,
    /// an event when it does not.
    ///
    /// This is what lets the bus serve the SAME [`crate::Session`] as every
    /// other transport. A capability that pushes frames on its own schedule
    /// (sysinfo, `fs_tail`, a bus subscription, PTY output) writes plain host
    /// messages through [`Out`] and never learns which transport is carrying
    /// them; the framing turns each one into `{"t":"event","value":…}` on its
    /// way out.
    Zgui,
}

/// One host message as the zgui-bridge frame that carries it.
///
/// * `{ok:false, err}` → `{"t":"reply", ok:false, error}` — the bus names a
///   failure `error`, the host names it `err`, and this is the one place that
///   knows.
/// * anything else with an `id` → `{"t":"reply", id, ok:true, value}`, `value`
///   being the message itself (its `id` stripped: the frame carries it).
/// * anything without an `id` → `{"t":"event", value}`. Nothing asked for it,
///   so it cannot be a reply to anything.
fn zgui_frame(v: &Value) -> Value {
    // The id is copied verbatim, not coerced to a number: a capability that keys itself by `id`
    // (a watcher, a PTY) answers under ITS key, and turning that into "no id" would file a real
    // reply as an unsolicited event.
    let id = v.get("id").filter(|i| !i.is_null()).cloned();
    let failed = v.get("ok") == Some(&Value::Bool(false));
    if failed {
        let err = v
            .get("err")
            .and_then(Value::as_str)
            .unwrap_or("refused")
            .to_string();
        let mut f = serde_json::Map::new();
        f.insert("t".into(), Value::String("reply".into()));
        if let Some(i) = id {
            f.insert("id".into(), i);
        }
        f.insert("ok".into(), Value::Bool(false));
        f.insert("error".into(), Value::String(err));
        return Value::Object(f);
    }
    let mut body = v.clone();
    if let Some(o) = body.as_object_mut() {
        o.remove("id");
    }
    match id {
        Some(i) => serde_json::json!({ "t": "reply", "id": i, "ok": true, "value": body }),
        None => serde_json::json!({ "t": "event", "value": body }),
    }
}

/// The write half of a connection. Cloneable via [`Out`] so background threads
/// (sysinfo streamer, PTY reader) can push frames concurrently; the mutex keeps
/// their frames from interleaving mid-message.
pub struct Peer {
    w: Mutex<Box<dyn Write + Send>>,
    framing: Framing,
}

/// Shared handle to a [`Peer`]. Every capability writes through this.
pub type Out = Arc<Peer>;

impl Peer {
    /// Wrap a writer with the given framing.
    pub fn new(w: Box<dyn Write + Send>, framing: Framing) -> Out {
        Arc::new(Peer {
            w: Mutex::new(w),
            framing,
        })
    }
    /// Native-messaging (Chrome) sink over `stdout`.
    pub fn native(w: Box<dyn Write + Send>) -> Out {
        Self::new(w, Framing::Native)
    }
    /// NDJSON sink (sockets and CLI).
    pub fn ndjson(w: Box<dyn Write + Send>) -> Out {
        Self::new(w, Framing::Ndjson)
    }
    /// zgui-bridge sink (the automation bus).
    pub fn zgui(w: Box<dyn Write + Send>) -> Out {
        Self::new(w, Framing::Zgui)
    }
    /// Frame and write one message. Errors when the peer has hung up, which
    /// streaming callers use as the signal to stop.
    pub fn send(&self, v: &Value) -> io::Result<()> {
        let framed;
        let v = match self.framing {
            Framing::Zgui => {
                framed = zgui_frame(v);
                &framed
            }
            _ => v,
        };
        let data = serde_json::to_vec(v).unwrap_or_default();
        let mut o = self.w.lock().unwrap();
        match self.framing {
            Framing::Native => {
                o.write_all(&(data.len() as u32).to_le_bytes())?;
                o.write_all(&data)?;
            }
            Framing::Ndjson | Framing::Zgui => {
                o.write_all(&data)?;
                o.write_all(b"\n")?;
            }
        }
        o.flush()
    }
}

/// Send a message on `out` (thin wrapper over [`Peer::send`]).
pub fn send_msg(out: &Out, v: &Value) -> io::Result<()> {
    out.send(v)
}

/// Send `v` as a reply to `req`, copying `req`'s correlation `id` (when present)
/// onto it so multiplexed callers can match replies to requests.
pub fn respond(out: &Out, req: &Value, mut v: Value) {
    if let (Some(obj), Some(id)) = (v.as_object_mut(), req.get("id")) {
        if !id.is_null() {
            obj.insert("id".into(), id.clone());
        }
    }
    // Record the response (rx) to the shared host log. Only real command replies
    // pass through here; streaming frames (sysinfo/pty/job) use out.send directly
    // and are intentionally not logged.
    crate::hostlog::record("rx", req, &v);
    let _ = out.send(&v);
}

/// Read one native-messaging (`u32`-prefixed) message. `None` on EOF, a short
/// read, an out-of-range length, or invalid JSON — any of which ends the stream.
pub fn read_native<R: Read>(r: &mut R) -> Option<Value> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).ok()?;
    let n = u32::from_le_bytes(len) as usize;
    if n == 0 || n > MAX_MSG {
        return None;
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

/// Read one NDJSON message: the next non-blank line parsed as JSON. `None` on
/// EOF. Blank lines are skipped; malformed lines yield `Some(Value::Null)` so
/// the caller can reply with an error rather than dropping the connection.
pub fn read_ndjson<R: BufRead>(r: &mut R) -> Option<Value> {
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        return Some(serde_json::from_str(t).unwrap_or(Value::Null));
    }
}

/// Standard base64 encode (any binary payload crossing the JSON pipe).
pub fn b64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Standard base64 decode; `None` on malformed input.
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}
