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

/// Read frames until one carries an `ok` field, skipping async push frames
/// (those have an `ev` key and no `ok`). The stryke LSP reader thread can
/// interleave `stryke-lsp-rx`/`stryke-lsp-exit` frames with command acks, so a
/// naked `nm_recv` isn't guaranteed to land on the ack.
fn nm_recv_ack(r: &mut impl Read) -> Value {
    for _ in 0..20 {
        let m = nm_recv(r).expect("a reply frame");
        if m.get("ok").is_some() {
            return m;
        }
    }
    panic!("no ok-bearing reply within 20 frames");
}

fn spawn_stdio(home: &PathBuf) -> Child {
    Command::new(BIN)
        .env("HOME", home)
        // Keep the state dir purely $HOME-relative: these would otherwise
        // redirect it out of the throwaway home.
        .env_remove("ZWIRE_STATE")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("APPDATA")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Where `app`'s persistent state lands under a throwaway `$HOME`, mirroring
/// `store::app_dir()` for the OS the test is built for — including the macOS
/// bundle-id folder the `zwire` app resolves to.
fn app_state_dir(home: &std::path::Path, app: &str) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        let folder = if app == "zwire" {
            "com.menketechnologies.zwire"
        } else {
            app
        };
        home.join("Library")
            .join("Application Support")
            .join(folder)
    }
    #[cfg(windows)]
    {
        home.join("AppData").join("Roaming").join(app)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        home.join(".config").join(app)
    }
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
    assert!(resp["version"].is_string(), "version present: {resp}");
    drop(si);
    let _ = child.wait();
}

#[test]
fn palette_write_persists_and_get_returns_it() {
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();

    // Combined commandless write: scheme + ui + resolved palette in ONE message.
    // Exercises the merged-response path and the TOML round-trip of "--"-prefixed
    // var keys with "#hex" values.
    nm_send(
        &mut si,
        &json!({
            "scheme": "midnight",
            "ui": { "light": true },
            "palette": { "--accent": "#ff2a6d", "--bg-primary": "#05050a" }
        }),
    );
    let w = nm_recv(&mut so).expect("a write reply");
    assert_eq!(w["ok"], json!(true), "combined write ok: {w}");
    assert_eq!(w["scheme"], json!("midnight"), "scheme echoed: {w}");
    assert_eq!(w["ui"]["light"], json!(true), "ui echoed: {w}");
    assert_eq!(w["palette"]["--accent"], json!("#ff2a6d"), "palette echoed: {w}");

    // A fresh `get` must return the palette persisted to global.toml, verbatim.
    nm_send(&mut si, &json!({"cmd": "get"}));
    let g = nm_recv(&mut so).expect("a get reply");
    assert_eq!(g["scheme"], json!("midnight"), "scheme persisted: {g}");
    assert!(g["palette"].is_object(), "palette present: {g}");
    assert_eq!(g["palette"]["--accent"], json!("#ff2a6d"), "accent persisted: {g}");
    assert_eq!(g["palette"]["--bg-primary"], json!("#05050a"), "bg persisted: {g}");

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
    assert!(app_state_dir(&home, "myapp")
        .join("kv")
        .join("cfg.json")
        .exists());
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
fn background_job_runs_and_collects() {
    use base64::Engine;
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();

    // `notify: false` so the test doesn't fire a real desktop notification.
    #[cfg(windows)]
    let start = json!({"cmd":"job_start","program":"cmd","args":["/C","echo","jobbed"],"notify":false,"label":"t"});
    #[cfg(not(windows))]
    let start =
        json!({"cmd":"job_start","program":"echo","args":["jobbed"],"notify":false,"label":"t"});

    nm_send(&mut si, &start);
    let ack = nm_recv(&mut so).unwrap();
    assert_eq!(ack["ok"], json!(true), "start ack: {ack}");
    let id = ack["job"].as_u64().expect("a job id");

    // Poll until the finished job drains.
    let mut done = None;
    for _ in 0..60 {
        nm_send(&mut si, &json!({"cmd": "job_poll"}));
        let poll = nm_recv(&mut so).unwrap();
        if let Some(j) = poll["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["id"].as_u64() == Some(id))
        {
            done = Some(j.clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let job = done.expect("job never completed");
    assert_eq!(job["code"], json!(0), "job result: {job}");
    let out = base64::engine::general_purpose::STANDARD
        .decode(job["stdout"].as_str().unwrap())
        .unwrap();
    assert_eq!(String::from_utf8(out).unwrap().trim(), "jobbed");

    drop(si);
    let _ = child.wait();
}

#[test]
fn pubsub_delivers_to_subscribers() {
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();

    nm_send(&mut si, &json!({"cmd": "sub", "topic": "scheme"}));
    assert_eq!(nm_recv(&mut so).unwrap()["ok"], json!(true));

    // Snapshot-on-subscribe: subscribing to the `scheme` topic immediately
    // pushes the host's current scheme in the same frame shape as a live pub,
    // so a fresh client converges without a separate `get`. In a temp home
    // with no persisted theme this is the `cyberpunk` default. Consume it
    // before the pub below so we assert against the published frame, not the
    // hydration snapshot.
    let snap = nm_recv(&mut so).unwrap();
    assert_eq!(snap["ev"], json!("pub"), "snapshot frame: {snap}");
    assert_eq!(snap["topic"], json!("scheme"));
    assert_eq!(snap["data"]["scheme"], json!("cyberpunk"));

    nm_send(
        &mut si,
        &json!({"cmd": "pub", "topic": "scheme", "data": {"scheme": "matrix"}}),
    );
    // The event frame is pushed before the publish ack, on the same connection.
    let ev = nm_recv(&mut so).unwrap();
    assert_eq!(ev["ev"], json!("pub"), "event frame: {ev}");
    assert_eq!(ev["topic"], json!("scheme"));
    assert_eq!(ev["data"]["scheme"], json!("matrix"));
    let ack = nm_recv(&mut so).unwrap();
    assert_eq!(ack["delivered"], json!(1), "ack: {ack}");

    drop(si);
    let _ = child.wait();
}

#[test]
fn procs_ps_and_which() {
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();

    nm_send(&mut si, &json!({"cmd": "ps", "limit": 5}));
    let ps = nm_recv(&mut so).unwrap();
    let list = ps["procs"].as_array().expect("procs array");
    assert!(!list.is_empty(), "ps returned processes: {ps}");
    assert!(list[0]["pid"].is_number() && list[0]["name"].is_string());

    #[cfg(windows)]
    let shell = "cmd";
    #[cfg(not(windows))]
    let shell = "sh";
    nm_send(&mut si, &json!({"cmd": "which", "program": shell}));
    let w = nm_recv(&mut so).unwrap();
    assert!(w["path"].is_string(), "which {shell} -> {w}");

    nm_send(
        &mut si,
        &json!({"cmd": "which", "program": "definitely-not-a-real-binary-xyz"}),
    );
    assert!(nm_recv(&mut so).unwrap()["path"].is_null());

    drop(si);
    let _ = child.wait();
}

#[test]
fn fs_tail_streams_appended_lines() {
    let home = temp_home();
    let f = home.join("log.txt");
    std::fs::write(&f, "alpha\n").unwrap();

    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();

    nm_send(
        &mut si,
        &json!({"cmd":"fs_tail","path": f, "from":"start","interval_ms":50}),
    );

    // Read frames until we see the replayed first line (skipping the ack).
    let mut saw_alpha = false;
    for _ in 0..10 {
        let m = nm_recv(&mut so).unwrap();
        if m["ev"] == json!("line") && m["data"] == json!("alpha") {
            saw_alpha = true;
            break;
        }
    }
    assert!(saw_alpha, "tail replayed the existing line");

    // Append and expect it to stream through.
    {
        use std::io::Write;
        let mut fh = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
        fh.write_all(b"beta\n").unwrap();
    }
    let mut saw_beta = false;
    for _ in 0..10 {
        let m = nm_recv(&mut so).unwrap();
        if m["ev"] == json!("line") && m["data"] == json!("beta") {
            saw_beta = true;
            break;
        }
    }
    assert!(saw_beta, "tail streamed the appended line");

    drop(si);
    let _ = child.wait();
}

#[test]
fn peer_commands_present() {
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();

    nm_send(&mut si, &json!({"cmd": "peers"}));
    let peers = nm_recv(&mut so).unwrap();
    assert_eq!(peers["ok"], json!(true));
    assert!(peers["self"].is_string(), "self name: {peers}");
    assert_eq!(peers["peers"], json!([]), "no peers yet: {peers}");

    // hello advertises the peer capability.
    nm_send(&mut si, &json!({"cmd": "hello"}));
    let caps = nm_recv(&mut so).unwrap();
    assert!(caps["caps"].as_array().unwrap().iter().any(|c| c == "peer"));

    nm_send(&mut si, &json!({"cmd": "peer_connect"}));
    assert_eq!(nm_recv(&mut so).unwrap()["ok"], json!(false));

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
    for k in ["cpu", "mem", "uptime", "load", "io"] {
        assert!(!sys[k].is_null(), "missing {k}: {m}");
    }
    // The I/O segment is a `{r, w}` bytes-per-second pair on every platform.
    assert!(
        sys["io"]["r"].is_u64() && sys["io"]["w"].is_u64(),
        "io shape: {m}"
    );
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

#[test]
fn hooks_events_lists_catalog() {
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    nm_send(&mut si, &json!({"cmd": "hooks_events"}));
    let r = nm_recv(&mut so).expect("a reply");
    assert_eq!(r["ok"], json!(true));
    assert!(r["events"].as_array().map_or(false, |a| !a.is_empty()), "events present: {r}");
    assert!(
        r["actions"].as_array().map_or(false, |a| a.iter().any(|v| v == "notify")),
        "actions include notify: {r}"
    );
    drop(si);
    let _ = child.wait();
}

#[test]
fn hooks_save_list_get_roundtrip() {
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    nm_send(&mut si, &json!({"cmd":"hooks_save","hook":{"name":"Tabby","event":"tab-created","enabled":true}}));
    let saved = nm_recv(&mut so).expect("save reply");
    assert_eq!(saved["ok"], json!(true));
    let id = saved["hook"]["id"].as_str().expect("id").to_string();
    assert!(id.starts_with("tabby-"), "slug id: {id}");
    assert_eq!(saved["hook"]["timeout_ms"], json!(10000), "default timeout filled");
    nm_send(&mut si, &json!({"cmd":"hooks_list"}));
    let listed = nm_recv(&mut so).expect("list reply");
    let hooks = listed["hooks"].as_array().expect("hooks array");
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0]["event"], json!("tab-created"));
    nm_send(&mut si, &json!({"cmd":"hooks_get_script","id": id}));
    let scr = nm_recv(&mut so).expect("script reply");
    assert!(scr["code"].as_str().unwrap_or("").contains("actions"), "default script scaffolded: {scr}");
    drop(si);
    let _ = child.wait();
}

#[test]
fn hooks_enable_toggle_and_delete() {
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    nm_send(&mut si, &json!({"cmd":"hooks_save","hook":{"name":"X","event":"navigation","enabled":false}}));
    let id = nm_recv(&mut so).unwrap()["hook"]["id"].as_str().unwrap().to_string();
    nm_send(&mut si, &json!({"cmd":"hooks_set_enabled","id": id, "enabled": true}));
    assert_eq!(nm_recv(&mut so).unwrap()["ok"], json!(true));
    nm_send(&mut si, &json!({"cmd":"hooks_list"}));
    assert_eq!(nm_recv(&mut so).unwrap()["hooks"][0]["enabled"], json!(true));
    nm_send(&mut si, &json!({"cmd":"hooks_delete","id": id}));
    assert_eq!(nm_recv(&mut so).unwrap()["ok"], json!(true));
    nm_send(&mut si, &json!({"cmd":"hooks_list"}));
    assert_eq!(nm_recv(&mut so).unwrap()["hooks"].as_array().unwrap().len(), 0);
    drop(si);
    let _ = child.wait();
}

#[test]
fn stryke_run_executes_inline_or_reports_missing() {
    // CI-safe: with stryke installed the code runs and prints 42; without it the
    // host returns a clean not-found error. Never hangs (10s cap in the host).
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    nm_send(&mut si, &json!({"cmd":"stryke_run","code":"p 6 * 7"}));
    let r = nm_recv(&mut so).expect("a reply");
    if r["ok"] == json!(true) {
        assert!(r["stdout"].as_str().unwrap_or("").contains("42"), "stryke ran: {r}");
        assert_eq!(r["code"], json!(0));
    } else {
        assert!(r["err"].as_str().unwrap_or("").to_lowercase().contains("stryke"), "clean missing-binary error: {r}");
    }
    drop(si);
    let _ = child.wait();
}

#[test]
fn hooks_persist_across_host_processes() {
    // The Hooks page saves a hook in one native-message spawn; the background
    // worker fires it in a *different* spawn. Both must see the same on-disk
    // manifest — this is the disk-backed contract the browser design relies on.
    let home = temp_home();
    {
        let mut a = spawn_stdio(&home);
        let mut si = a.stdin.take().unwrap();
        let mut so = a.stdout.take().unwrap();
        nm_send(&mut si, &json!({"cmd":"hooks_save","hook":{"name":"Persist","event":"host-ready","enabled":true}}));
        assert_eq!(nm_recv(&mut so).unwrap()["ok"], json!(true));
        drop(si);
        let _ = a.wait();
    }
    let mut b = spawn_stdio(&home);
    let mut si = b.stdin.take().unwrap();
    let mut so = b.stdout.take().unwrap();
    nm_send(&mut si, &json!({"cmd":"hooks_list"}));
    let listed = nm_recv(&mut so).expect("list reply");
    let arr = listed["hooks"].as_array().expect("hooks array");
    assert_eq!(arr.len(), 1, "hook persisted across processes: {listed}");
    assert_eq!(arr[0]["name"], json!("Persist"));
    drop(si);
    let _ = b.wait();
}

#[test]
fn stryke_lsp_send_without_start_errors() {
    // No stryke needed: sending before start hits the None guard cleanly.
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    nm_send(&mut si, &json!({"cmd":"stryke_lsp_send","message":"{}"}));
    let r = nm_recv(&mut so).expect("a reply");
    assert_eq!(r["ok"], json!(false));
    assert!(r["err"].as_str().unwrap_or("").contains("not running"), "guard msg: {r}");
    drop(si);
    let _ = child.wait();
}

#[test]
fn stryke_lsp_stop_is_idempotent_without_start() {
    // No stryke needed: stopping a server that was never started must still
    // reply cleanly with ok:true — the Hooks editor closing sends stop
    // unconditionally, so a no-op stop can never error or hang.
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    nm_send(&mut si, &json!({"cmd":"stryke_lsp_stop"}));
    let r = nm_recv(&mut so).expect("a reply");
    assert_eq!(r["ok"], json!(true), "idempotent stop: {r}");
    // A second stop is still fine.
    nm_send(&mut si, &json!({"cmd":"stryke_lsp_stop"}));
    assert_eq!(nm_recv(&mut so).expect("a reply")["ok"], json!(true));
    drop(si);
    let _ = child.wait();
}

#[test]
fn stryke_lsp_start_stop_cycle() {
    // CI-safe: `stryke_lsp_start` spawns `stryke --lsp`. With stryke present the
    // start acks ok:true and a following stop tears the child down and re-acks
    // ok:true; without stryke the start acks ok:false with a stryke-mentioning
    // error, and stop is still a clean no-op. Never hangs (stdin dropped ⇒ EOF).
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();

    nm_send(&mut si, &json!({"cmd":"stryke_lsp_start"}));
    let start = nm_recv_ack(&mut so);
    if start["ok"] == json!(true) {
        // Server is up; stop must reply ok:true after killing the child. Use
        // the ack-draining recv since the reader thread pushes a trailing
        // `stryke-lsp-exit` frame when the child dies.
        nm_send(&mut si, &json!({"cmd":"stryke_lsp_stop"}));
        let stop = nm_recv_ack(&mut so);
        assert_eq!(stop["ok"], json!(true), "stop after start: {stop}");
    } else {
        assert!(
            start["err"].as_str().unwrap_or("").to_lowercase().contains("stryke"),
            "clean missing-binary error: {start}"
        );
        // Even after a failed start, stop stays a clean no-op.
        nm_send(&mut si, &json!({"cmd":"stryke_lsp_stop"}));
        assert_eq!(nm_recv_ack(&mut so)["ok"], json!(true));
    }
    drop(si);
    let _ = child.wait();
}

#[test]
fn stryke_run_reports_error_for_bad_code() {
    // CI-safe: a syntax-error script. With stryke present the child compiles,
    // fails, and exits non-zero → ok:true with code != 0 (and stderr text);
    // without stryke the host returns ok:false mentioning the missing binary.
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    nm_send(&mut si, &json!({"cmd":"stryke_run","code":")("}));
    let r = nm_recv(&mut so).expect("a reply");
    if r["ok"] == json!(true) {
        assert_eq!(r["timedOut"], json!(false), "should not time out: {r}");
        // A compile failure surfaces as a non-zero exit code and/or stderr.
        let nonzero = r["code"].as_i64().map_or(false, |c| c != 0);
        let has_stderr = !r["stderr"].as_str().unwrap_or("").is_empty();
        assert!(nonzero || has_stderr, "bad code flagged an error: {r}");
    } else {
        assert!(
            r["err"].as_str().unwrap_or("").to_lowercase().contains("stryke"),
            "clean missing-binary error: {r}"
        );
    }
    drop(si);
    let _ = child.wait();
}

#[test]
fn stryke_run_passes_stdin_through() {
    // CI-safe: `p <>` echoes the whole of stdin. With stryke present the
    // supplied `stdin` string must round-trip to stdout; without stryke the
    // host returns a clean missing-binary error.
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    nm_send(
        &mut si,
        &json!({"cmd":"stryke_run","code":"p <>","stdin":"zwire-stdin-marker"}),
    );
    let r = nm_recv(&mut so).expect("a reply");
    if r["ok"] == json!(true) {
        assert_eq!(r["code"], json!(0), "clean run: {r}");
        assert!(
            r["stdout"].as_str().unwrap_or("").contains("zwire-stdin-marker"),
            "stdin round-tripped to stdout: {r}"
        );
    } else {
        assert!(
            r["err"].as_str().unwrap_or("").to_lowercase().contains("stryke"),
            "clean missing-binary error: {r}"
        );
    }
    drop(si);
    let _ = child.wait();
}

#[test]
fn hooks_save_empty_name_gets_slug_id() {
    let home = temp_home();
    let mut child = spawn_stdio(&home);
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    nm_send(&mut si, &json!({"cmd":"hooks_save","hook":{"name":"","event":"navigation","enabled":false}}));
    let saved = nm_recv(&mut so).expect("save reply");
    assert_eq!(saved["ok"], json!(true));
    assert!(saved["hook"]["id"].as_str().unwrap_or("").starts_with("hook-"), "slug fallback: {saved}");
    drop(si);
    let _ = child.wait();
}
