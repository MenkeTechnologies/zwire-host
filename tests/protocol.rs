//! Exercises the message protocol against the real binary, over both the Chrome
//! native-messaging stdio transport and the Unix-socket NDJSON daemon.
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_zwire-host");

/// A throwaway `$HOME` so tests never touch the developer's real `~/.zwire`.
fn temp_home() -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("zwh-home-{}-{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/* ---- native-messaging (u32-framed) helpers ---- */
fn nm_send(w: &mut impl Write, v: &Value) {
    let d = serde_json::to_vec(v).unwrap();
    w.write_all(&(d.len() as u32).to_le_bytes()).unwrap();
    w.write_all(&d).unwrap();
    w.flush().unwrap();
}
fn nm_recv(r: &mut impl Read) -> Option<Value> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).ok()?;
    let n = u32::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

fn spawn_stdio(home: &PathBuf) -> Child {
    Command::new(BIN)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

/// An `exec` request that prints `word` — cross-platform, since `echo` is a
/// `cmd` builtin (not an executable) on Windows.
fn echo_exec(word: &str) -> Value {
    #[cfg(windows)]
    {
        json!({"cmd":"exec","program":"cmd","args":["/C","echo",word]})
    }
    #[cfg(not(windows))]
    {
        json!({"cmd":"exec","program":"echo","args":[word]})
    }
}

#[test]
fn get_returns_scheme_and_ui() {
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    nm_send(&mut si, &json!({"cmd": "get"}));
    let resp = nm_recv(&mut so).expect("a reply");
    assert_eq!(resp["ok"], json!(true));
    assert!(resp["scheme"].is_string(), "scheme present: {resp}");
    assert!(resp["ui"].is_object(), "ui present: {resp}");
    drop(si);
    let _ = child.wait();
}

#[test]
fn hello_advertises_caps() {
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    nm_send(&mut si, &json!({"cmd": "hello", "id": 7}));
    let resp = nm_recv(&mut so).expect("a reply");
    assert_eq!(resp["ok"], json!(true));
    assert_eq!(resp["id"], json!(7), "id echoed: {resp}");
    assert!(resp["caps"].as_array().unwrap().iter().any(|c| c == "pty"));
    assert!(resp["version"].is_string());
    drop(si);
    let _ = child.wait();
}

#[test]
fn kv_roundtrip_and_merge() {
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();

    nm_send(
        &mut si,
        &json!({"cmd":"kv_set","app":"myapp","key":"cfg","value":{"a":1}}),
    );
    assert_eq!(nm_recv(&mut so).unwrap()["ok"], json!(true));

    nm_send(
        &mut si,
        &json!({"cmd":"kv_merge","app":"myapp","key":"cfg","value":{"b":2}}),
    );
    let merged = nm_recv(&mut so).unwrap();
    assert_eq!(merged["value"], json!({"a":1,"b":2}), "merged: {merged}");

    nm_send(&mut si, &json!({"cmd":"kv_get","app":"myapp","key":"cfg"}));
    assert_eq!(nm_recv(&mut so).unwrap()["value"], json!({"a":1,"b":2}));

    nm_send(&mut si, &json!({"cmd":"kv_keys","app":"myapp"}));
    assert_eq!(nm_recv(&mut so).unwrap()["keys"], json!(["cfg"]));

    // The store must live under the app's own dir, isolated from zwire's.
    assert!(home.join(".myapp/kv/cfg.json").exists());
    drop(si);
    let _ = child.wait();
}

#[test]
fn fs_write_read_and_walk() {
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();

    let f = home.join("note.txt");
    nm_send(
        &mut si,
        &json!({"cmd":"fs_write","path": f, "text":"hello host"}),
    );
    assert_eq!(nm_recv(&mut so).unwrap()["ok"], json!(true));

    nm_send(&mut si, &json!({"cmd":"fs_read","path": f}));
    let read = nm_recv(&mut so).unwrap();
    assert_eq!(read["text"], json!("hello host"), "read back: {read}");

    // Crawl the temp home for *.txt — the "crawl filesystem from a plugin" path.
    nm_send(&mut si, &json!({"cmd":"fs_walk","path": home, "ext":"txt"}));
    let walk = nm_recv(&mut so).unwrap();
    assert_eq!(walk["ok"], json!(true));
    let names: Vec<&str> = walk["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(names.contains(&"note.txt"), "walk found note.txt: {walk}");
    drop(si);
    let _ = child.wait();
}

#[test]
fn exec_runs_a_program() {
    use base64::Engine;
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    nm_send(&mut si, &echo_exec("zwire"));
    let resp = nm_recv(&mut so).unwrap();
    assert_eq!(resp["ok"], json!(true));
    assert_eq!(resp["code"], json!(0));
    let out = base64::engine::general_purpose::STANDARD
        .decode(resp["stdout"].as_str().unwrap())
        .unwrap();
    assert_eq!(String::from_utf8(out).unwrap().trim(), "zwire");
    drop(si);
    let _ = child.wait();
}

#[test]
fn sysinfo_stream_has_core_fields() {
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    nm_send(&mut si, &json!({"cmd": "sysinfo_start"}));
    // First frame is the `{ok,streaming}` ack; the next is a `{sys}` frame.
    let ack = nm_recv(&mut so).expect("ack");
    assert_eq!(ack["streaming"], json!(true), "ack: {ack}");
    let m = nm_recv(&mut so).expect("a sys frame");
    let sys = &m["sys"];
    for k in ["cpu", "mem", "uptime", "load"] {
        assert!(!sys[k].is_null(), "missing {k}: {m}");
    }
    let _ = child.kill();
    let _ = child.wait();
}

/* ---- local-socket daemon (Unix domain socket / Windows named pipe) ---- */

/// A unique endpoint for a test daemon: a temp `.sock` path on Unix, a
/// per-process pipe name on Windows.
fn test_endpoint() -> String {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    #[cfg(windows)]
    {
        format!("zwh-test-{}-{}", std::process::id(), n)
    }
    #[cfg(not(windows))]
    {
        std::env::temp_dir()
            .join(format!("zwh-test-{}-{}.sock", std::process::id(), n))
            .to_string_lossy()
            .into_owned()
    }
}

/// Run `zwire-host call --socket <ep> <request>` and parse the first reply line.
fn call(home: &PathBuf, ep: &str, request: &str) -> Option<Value> {
    let out = Command::new(BIN)
        .args(["call", "--socket", ep, request])
        .env("HOME", home)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    serde_json::from_str(line).ok()
}

#[test]
fn socket_daemon_round_trips_over_the_wire() {
    let home = temp_home();
    let ep = test_endpoint();

    let mut daemon = Command::new(BIN)
        .args(["serve", "--socket", &ep])
        .env("HOME", &home)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Poll via the real client until the daemon is accepting connections.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut hello = None;
    while Instant::now() < deadline {
        if let Some(v) = call(&home, &ep, "{\"cmd\":\"hello\",\"id\":\"h1\"}") {
            hello = Some(v);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let hello = hello.expect("daemon never answered hello");
    assert_eq!(hello["ok"], json!(true));
    assert_eq!(hello["id"], json!("h1"), "id echoed: {hello}");

    // exec over the socket/pipe
    let exec = call(&home, &ep, &echo_exec("sock").to_string()).expect("exec reply");
    assert_eq!(exec["code"], json!(0), "exec over socket: {exec}");

    let _ = daemon.kill();
    let _ = daemon.wait();
    #[cfg(not(windows))]
    let _ = std::fs::remove_file(&ep);
}
