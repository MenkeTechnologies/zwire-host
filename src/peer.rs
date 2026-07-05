//! Host-to-host peering — a mesh of `zwire-host` daemons.
//!
//! A daemon can listen for peers on TCP (`serve --tcp <addr>`) and/or dial out
//! to peers (`--peer <addr>`, or the `peer_connect` command). Peer links carry
//! two things:
//!   * **bus federation** — a `pub` (or a `scheme`/`ui` change) on one host is
//!     forwarded to every peer, whose local subscribers then receive it, so the
//!     event bus spans all your machines; and
//!   * **remote requests** — `{"cmd":"remote","peer":"host:port","request":{…}}`
//!     runs a request on another host and returns its reply.
//!
//! TCP is guarded by a shared `--token` (or `$ZWIRE_HOST_TOKEN`): inbound TCP
//! connections must `auth` / `peer_hello` with it before doing anything
//! privileged. Local Unix-socket clients are trusted and never need it.
//!
//! Federation is single-hop: a forwarded event is delivered locally but not
//! re-forwarded, which covers star and fully-meshed topologies without loops.
use crate::proto::{read_ndjson, send_msg, Out, Peer};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

struct Config {
    token: Option<String>,
    name: String,
}

fn config() -> &'static Mutex<Config> {
    static C: OnceLock<Mutex<Config>> = OnceLock::new();
    C.get_or_init(|| {
        Mutex::new(Config {
            token: std::env::var("ZWIRE_HOST_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            name: hostname(),
        })
    })
}

fn hostname() -> String {
    crate::osops::hostname().unwrap_or_else(|| "host".to_string())
}

/// Set the peering token and this host's advertised name (from CLI flags).
pub fn configure(token: Option<String>, name: Option<String>) {
    let mut c = config().lock().unwrap();
    if let Some(t) = token.filter(|t| !t.is_empty()) {
        c.token = Some(t);
    }
    if let Some(n) = name {
        c.name = n;
    }
}

fn token() -> Option<String> {
    config().lock().unwrap().token.clone()
}

/// This host's name, sent in handshakes and shown in `peers`.
pub fn local_name() -> String {
    config().lock().unwrap().name.clone()
}

/// Whether TCP connections must authenticate (a token is configured).
pub fn auth_required() -> bool {
    token().is_some()
}

/// Validate a presented token against the configured one (always ok if none).
pub fn token_ok(presented: Option<&str>) -> bool {
    match token() {
        None => true,
        Some(tok) => presented == Some(tok.as_str()),
    }
}

/* ---- peer link registry ---- */

struct Link {
    out: Out,
    name: String,
}

fn links() -> &'static Mutex<(u64, HashMap<u64, Link>)> {
    static L: OnceLock<Mutex<(u64, HashMap<u64, Link>)>> = OnceLock::new();
    L.get_or_init(|| Mutex::new((0, HashMap::new())))
}

/// Register a peer link, replacing any existing link with the same name (a
/// reconnect, or the reverse direction of a mutual peering). Returns its id.
pub fn register_link(out: &Out, name: &str) -> u64 {
    let mut g = links().lock().unwrap();
    let stale: Vec<u64> =
        g.1.iter()
            .filter(|(_, l)| l.name == name)
            .map(|(id, _)| *id)
            .collect();
    for id in stale {
        g.1.remove(&id);
    }
    g.0 += 1;
    let id = g.0;
    g.1.insert(
        id,
        Link {
            out: out.clone(),
            name: name.to_string(),
        },
    );
    id
}

/// Drop a peer link (its connection closed).
pub fn unregister_link(id: u64) {
    links().lock().unwrap().1.remove(&id);
}

/// Names of currently connected peers.
pub fn peer_names() -> Vec<String> {
    let mut names: Vec<String> = links()
        .lock()
        .unwrap()
        .1
        .values()
        .map(|l| l.name.clone())
        .collect();
    names.sort();
    names
}

/// Forward a published event to every peer; returns how many links took it.
pub fn broadcast(topic: &str, data: &Value) -> usize {
    let outs: Vec<Out> = {
        links()
            .lock()
            .unwrap()
            .1
            .values()
            .map(|l| l.out.clone())
            .collect()
    };
    let frame = json!({ "cmd": "peer_pub", "topic": topic, "data": data });
    outs.iter().filter(|o| send_msg(o, &frame).is_ok()).count()
}

/* ---- inbound: accept peers over TCP ---- */

/// Spawn the TCP listener that accepts peer/remote connections.
pub fn listen_tcp(addr: String) {
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(&addr) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("zwire-host: tcp bind {addr}: {e}");
                return;
            }
        };
        eprintln!("zwire-host: peering on tcp {addr}");
        for conn in listener.incoming().flatten() {
            std::thread::spawn(move || handle_inbound(conn));
        }
    });
}

fn handle_inbound(stream: TcpStream) {
    let Ok(rclone) = stream.try_clone() else {
        return;
    };
    let out = Peer::ndjson(Box::new(stream));
    // Inbound TCP is untrusted until it authenticates (if a token is set).
    let sess = crate::session::Session::guarded(auth_required());
    crate::transport::serve_conn(BufReader::new(rclone), out, sess);
}

/* ---- outbound: dial peers and keep the link up ---- */

/// Spawn a task that keeps a link to `addr` open, reconnecting on failure.
pub fn dial(addr: String) {
    std::thread::spawn(move || loop {
        if let Err(e) = try_dial(&addr) {
            eprintln!("zwire-host: peer {addr}: {e}");
        }
        std::thread::sleep(Duration::from_secs(5));
    });
}

fn try_dial(addr: &str) -> Result<(), String> {
    let stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    let rclone = stream.try_clone().map_err(|e| e.to_string())?;
    let out = Peer::ndjson(Box::new(stream));
    let mut reader = BufReader::new(rclone);

    // Handshake: introduce ourselves and prove the token.
    send_msg(
        &out,
        &json!({ "cmd": "peer_hello", "token": token(), "name": local_name() }),
    )
    .map_err(|e| e.to_string())?;
    let reply = read_ndjson(&mut reader).ok_or("peer closed during handshake")?;
    if reply["ok"] != json!(true) {
        return Err(reply["err"]
            .as_str()
            .unwrap_or("handshake rejected")
            .to_string());
    }
    let peer_name = reply["name"].as_str().unwrap_or(addr).to_string();
    let link = register_link(&out, &peer_name);
    eprintln!("zwire-host: peered with {peer_name} ({addr})");

    // We initiated a trusted dial, so this session is pre-authenticated; it
    // handles the peer's forwarded events (peer_pub) for the life of the link.
    let mut sess = crate::session::Session::guarded(false);
    while let Some(msg) = read_ndjson(&mut reader) {
        sess.handle(&out, &msg);
    }
    unregister_link(link);
    Ok(())
}

/* ---- remote requests ---- */

/// Run one request on the peer at `addr` and return its reply. Uses a fresh
/// connection (authenticating if a token is set), so it is independent of the
/// long-lived federation link. Intended for RPC commands, not streams.
pub fn remote(addr: &str, request: &Value) -> Result<Value, String> {
    let stream = TcpStream::connect(addr).map_err(|e| format!("connect {addr}: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let rclone = stream.try_clone().map_err(|e| e.to_string())?;
    let out = Peer::ndjson(Box::new(stream));
    let mut reader = BufReader::new(rclone);

    if let Some(tok) = token() {
        send_msg(&out, &json!({ "cmd": "auth", "token": tok })).map_err(|e| e.to_string())?;
        let _ = read_ndjson(&mut reader); // consume the auth ack
    }
    send_msg(&out, request).map_err(|e| e.to_string())?;
    read_ndjson(&mut reader).ok_or_else(|| "no reply from remote".to_string())
}
