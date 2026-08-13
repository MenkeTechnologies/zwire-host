//! Suite bus CLIENT against a real listening peer.
//!
//! `src/suite.rs`'s unit tests cover the refusals that never reach a socket (bad names, empty
//! verbs). Everything interesting about a client is what happens once bytes move, so this file
//! stands up an actual peer: a Unix socket in a private directory that speaks the `zgui-bridge`
//! NDJSON frames back, exactly as zcite/zreq/zgo do.
//!
//! The claim under test that is easiest to get wrong is enumeration. A socket FILE outlives the
//! process that bound it — the real `$TMPDIR/zgui` on this machine holds dozens of entries from
//! long-dead test runs — so listing the directory reports apps that are not running. `list` must
//! dial, and [`list_skips_a_stale_socket_file_and_keeps_the_live_one`] is what proves it does.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde_json::{json, Value};
use zwire_host::suite;

/// One test at a time.
///
/// [`sandbox`] calls `std::env::set_var` while other tests are already calling `suite::call`, which
/// reads the same variable through `getenv` — and a `setenv` concurrent with a `getenv` is not safe
/// on glibc or on Darwin. It shows up as roughly one run in thirty resolving a torn path and failing
/// with `not running on the bus (Invalid argument (os error 22))`: a flake with no bug behind it,
/// which is the worst kind, because the next real failure gets waved through as "that flaky one".
/// Ordering the tests is what removes the concurrency the race needs. Poisoning is ignored: a
/// panicking test has already failed, and poisoning would turn it into a whole-file failure.
fn serialize() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Point the client at a private socket directory, once per test process.
///
/// `suite` resolves `$XDG_RUNTIME_DIR/zgui` first, so setting it both isolates these tests from the
/// user's live bus (a stray `suite_call` must never reach a real running app) and keeps the real
/// directory's stale entries out of the enumeration assertions.
fn sandbox() -> &'static PathBuf {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("zwire-suite-test-{}", std::process::id()));
        std::env::set_var("XDG_RUNTIME_DIR", &base);
        let dir = base.join("zgui");
        std::fs::create_dir_all(&dir).expect("create sandbox socket dir");
        dir
    })
}

/// A peer that serves the bridge frames forever, replying per `answer`.
///
/// Returns once the listener is BOUND, so a caller never races the socket into existence. The
/// serving thread is detached and dies with the test process. It must serve without a connection
/// budget: `list` probes EVERY candidate in the sandbox, so a peer that stopped accepting after a
/// fixed count would be spent by another test's enumeration and then refuse the calls it exists to
/// answer.
fn spawn_peer(app: &str, answer: fn(&Value) -> Value) {
    let path = sandbox().join(format!("{app}.sock"));
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind peer socket");
    std::thread::spawn(move || {
        loop {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let Ok(mut w) = stream.try_clone() else {
                return;
            };
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue; // a liveness probe: connect, then hang up without sending a frame
            }
            let req: Value = match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let mut out = serde_json::to_string(&answer(&req)).unwrap();
            out.push('\n');
            let _ = w.write_all(out.as_bytes());
            let _ = w.flush();
        }
    });
}

/// A peer that echoes back what it was asked, in the bridge's reply envelope.
fn echo(req: &Value) -> Value {
    match req["t"].as_str().unwrap_or("") {
        "verbs" => json!({"t":"reply","id":req["id"],"ok":true,"value":{
            "app":"faux","verbs":[{"id":"doc.open","label":"Open","rev":"inverse"}],
            "state":[{"id":"selection"}],"events":[]}}),
        "get" => json!({"t":"reply","id":req["id"],"ok":true,"value":{"state":req["state"]}}),
        "call" => {
            json!({"t":"reply","id":req["id"],"ok":true,"value":{"verb":req["verb"],"args":req["args"]}})
        }
        _ => json!({"t":"reply","id":req["id"],"ok":false,"error":"unknown request kind"}),
    }
}

/// A peer that refuses everything, the way a bridge rejects an unknown verb.
fn refuse(req: &Value) -> Value {
    json!({"t":"reply","id":req["id"],"ok":false,"error":"no such verb: nope"})
}

#[test]
fn verbs_call_and_get_reach_a_live_peer_and_return_its_value() {
    let _g = serialize();
    spawn_peer("faux-echo", echo);

    let surface = suite::verbs("faux-echo").expect("verbs");
    assert_eq!(surface["app"], json!("faux"));
    // The peer's reversibility class survives the hop — this is what lets zwire's trigger editor
    // tell an author, at authoring time, whether a cross-app step can be unwound by its owner.
    assert_eq!(surface["verbs"][0]["rev"], json!("inverse"));

    let called = suite::call("faux-echo", "doc.open", json!({"path": "/tmp/x.pdf"})).expect("call");
    assert_eq!(called["verb"], json!("doc.open"));
    assert_eq!(called["args"]["path"], json!("/tmp/x.pdf"));

    let state = suite::get("faux-echo", "selection").expect("get");
    assert_eq!(state["state"], json!("selection"));
}

#[test]
fn a_peer_error_reply_becomes_an_error_not_a_silent_null() {
    let _g = serialize();
    spawn_peer("faux-refuse", refuse);
    let err = suite::call("faux-refuse", "nope", json!({})).unwrap_err();
    assert!(err.contains("no such verb"), "unexpected error: {err}");
}

#[test]
fn call_with_no_args_still_sends_an_object() {
    let _g = serialize();
    // `args` is `#[serde(default)]` on the bridge side, but a null there deserializes to `Value::Null`
    // and a handler indexing it gets nothing. Normalizing to `{}` in the client is what keeps a
    // zero-argument verb callable from a palette step that supplied no JSON body.
    spawn_peer("faux-noargs", echo);
    let v = suite::call("faux-noargs", "ping", Value::Null).expect("call");
    assert_eq!(v["args"], json!({}));
}

#[test]
fn list_skips_a_stale_socket_file_and_keeps_the_live_one() {
    let _g = serialize();
    spawn_peer("faux-live", echo);
    // A file with a .sock name and nobody listening — exactly what a crashed app leaves behind.
    std::fs::write(sandbox().join("faux-dead.sock"), b"").expect("write stale socket file");

    let listed = suite::list();
    let apps: Vec<String> = listed["apps"]
        .as_array()
        .expect("apps array")
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect();

    assert!(
        apps.contains(&"faux-live".to_string()),
        "live peer missing: {apps:?}"
    );
    assert!(
        !apps.contains(&"faux-dead".to_string()),
        "stale socket file reported as a running app: {apps:?}"
    );
    // `probed` counts the ENTRIES, so a caller can distinguish "nothing installed" from "nothing
    // running" — the stale file is counted there even though it is not an app.
    assert!(
        listed["probed"].as_u64().unwrap_or(0) > apps.len() as u64,
        "probed should exceed the live count when a stale entry exists: {listed}"
    );
}

#[test]
fn a_peer_that_hangs_up_without_replying_is_an_error() {
    let _g = serialize();
    // Bind and immediately drop the listener's accept loop: the connect succeeds, the read gets EOF.
    let path = sandbox().join("faux-mute.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind");
    std::thread::spawn(move || {
        while let Ok((s, _)) = listener.accept() {
            drop(s);
        }
    });
    let err = suite::call("faux-mute", "x", json!({})).unwrap_err();
    // WHICH syscall notices the hangup is a race, not a behaviour. If our request lands before the
    // peer's `drop` the write succeeds and the read hits EOF ("closed without replying"); if the
    // drop wins, the write itself fails with EPIPE. Both are the same event — the peer went away
    // before answering — and pinning only the first outcome made this test fail about one run in
    // ten on a loaded machine. What must stay pinned is that a mute peer is an ERROR naming the
    // peer, never a silent success or a hang.
    assert!(
        err.starts_with("faux-mute: ")
            && (err.contains("closed without replying")
                || err.contains("read:")
                || err.contains("write:")),
        "unexpected error: {err}"
    );
}

#[test]
fn suite_commands_dispatch_through_the_host_command_table() {
    let _g = serialize();
    spawn_peer("faux-cmd", echo);

    let v = suite::command(
        "suite_call",
        &json!({"app":"faux-cmd","verb":"doc.open","args":{"n":1}}),
    )
    .expect("suite_call is a suite command");
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["result"]["verb"], json!("doc.open"));

    let bad = suite::command("suite_verbs", &json!({"app":"faux-not-running"}))
        .expect("suite_verbs is a suite command");
    assert_eq!(bad["ok"], json!(false));
    assert!(bad["err"]
        .as_str()
        .unwrap_or("")
        .contains("not running on the bus"));
}
