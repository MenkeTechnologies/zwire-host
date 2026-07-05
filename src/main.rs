// zwire native-messaging host (Rust, self-contained — no Python/psutil).
//   scheme bridge  : {cmd:"get"} / {scheme:"…"} / {ui:{…}}  <-> ~/.zwire files
//   system stats   : {cmd:"sysinfo_start"} -> stream {sys:{…}} every 2s
//   PTY terminal   : {cmd:"pty_spawn"} -> relay bytes <-> a login shell
// Cross-platform (macOS/Linux/Windows) via `sysinfo` + `portable-pty`.
use base64::Engine;
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Out = Arc<Mutex<io::Stdout>>;
const ALLOWED: &[&str] = &[
    "cyberpunk",
    "midnight",
    "matrix",
    "ember",
    "arctic",
    "crimson",
    "toxic",
    "vapor",
];

fn zwire_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let d = Path::new(&home).join(".zwire");
    let _ = std::fs::create_dir_all(&d);
    d
}

/* ---- native messaging framing ---- */
fn read_msg<R: Read>(r: &mut R) -> Option<Value> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).ok()?;
    let n = u32::from_le_bytes(len) as usize;
    if n == 0 || n > 128 * 1024 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}
fn send_msg(out: &Out, v: &Value) -> io::Result<()> {
    let data = serde_json::to_vec(v).unwrap_or_default();
    let mut o = out.lock().unwrap();
    o.write_all(&(data.len() as u32).to_le_bytes())?;
    o.write_all(&data)?;
    o.flush()
}

/* ---- scheme + ui files ---- */
fn current_scheme(d: &Path) -> String {
    std::fs::read_to_string(d.join("hud-scheme"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "cyberpunk".into())
}
fn write_scheme(d: &Path, s: &str) {
    let tmp = d.join("hud-scheme.tmp");
    if std::fs::write(&tmp, format!("{s}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, d.join("hud-scheme"));
    }
}
fn current_ui(d: &Path) -> Value {
    std::fs::read_to_string(d.join("hud-ui.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}
fn write_ui(d: &Path, partial: &Value) -> Value {
    let mut cur = current_ui(d);
    if let (Some(c), Some(p)) = (cur.as_object_mut(), partial.as_object()) {
        for (k, v) in p {
            c.insert(k.clone(), v.clone());
        }
    }
    let tmp = d.join("hud-ui.json.tmp");
    if serde_json::to_string(&cur)
        .ok()
        .and_then(|s| std::fs::write(&tmp, s).ok())
        .is_some()
    {
        let _ = std::fs::rename(&tmp, d.join("hud-ui.json"));
    }
    cur
}

/* ---- system stats ---- */
fn local_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}
fn public_ip() -> Option<String> {
    let mut s = std::net::TcpStream::connect("api.ipify.org:80").ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok();
    s.write_all(b"GET / HTTP/1.0\r\nHost: api.ipify.org\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok();
    buf.rsplit("\r\n\r\n")
        .next()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
}
#[cfg(target_os = "macos")]
fn battery() -> Option<Value> {
    let out = std::process::Command::new("pmset")
        .args(["-g", "batt"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let idx = s.find('%')?;
    let start = s[..idx]
        .rfind(|c: char| !c.is_ascii_digit())
        .map(|i| i + 1)
        .unwrap_or(0);
    let pct: i64 = s[start..idx].parse().ok()?;
    let c = s.contains("AC Power") || s.contains("charging") || s.contains("charged");
    Some(json!({"p": pct, "c": c}))
}
#[cfg(not(target_os = "macos"))]
fn battery() -> Option<Value> {
    None
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn stream_sysinfo(out: &Out) {
    use sysinfo::{Components, Disks, Networks, System};
    let mut sys = System::new();
    let mut nets = Networks::new_with_refreshed_list();
    let mut pip = public_ip();
    let mut last = Instant::now();
    let mut ticks: u64 = 0;
    loop {
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        nets.refresh(true);
        let dt = last.elapsed().as_secs_f64().max(0.1);
        last = Instant::now();

        let mut d = serde_json::Map::new();
        d.insert("cpu".into(), json!(sys.global_cpu_usage().round() as i64));
        let (mu, mt) = (sys.used_memory(), sys.total_memory());
        d.insert(
            "mem".into(),
            json!({"u": mu, "t": mt, "p": (mu * 100).checked_div(mt).unwrap_or(0)}),
        );
        let (su, st) = (sys.used_swap(), sys.total_swap());
        if st > 0 {
            d.insert("swap".into(), json!({"u": su, "t": st, "p": su * 100 / st}));
        }
        let la = System::load_average();
        d.insert(
            "load".into(),
            json!([round2(la.one), round2(la.five), round2(la.fifteen)]),
        );
        d.insert("uptime".into(), json!(System::uptime()));

        let disks = Disks::new_with_refreshed_list();
        if let Some(root) = disks.iter().find(|k| k.mount_point() == Path::new("/")) {
            let (t, a) = (root.total_space(), root.available_space());
            if t > 0 {
                d.insert(
                    "disk".into(),
                    json!({"u": t - a, "t": t, "p": (t - a) * 100 / t}),
                );
            }
        }
        let (mut up, mut down) = (0u64, 0u64);
        for (_, n) in &nets {
            up += n.transmitted();
            down += n.received();
        }
        d.insert(
            "net".into(),
            json!({"up": (up as f64 / dt) as u64, "down": (down as f64 / dt) as u64}),
        );

        let comps = Components::new_with_refreshed_list();
        let mut tmax = f32::MIN;
        for c in &comps {
            if let Some(t) = c.temperature() {
                if t > tmax {
                    tmax = t;
                }
            }
        }
        if tmax > f32::MIN {
            d.insert("temp".into(), json!(tmax.round() as i64));
        }
        if let Some(h) = System::host_name() {
            d.insert("host".into(), json!(h.split('.').next().unwrap_or(&h)));
        }
        if let Some(ip) = local_ip() {
            d.insert("lip".into(), json!(ip));
        }
        if let Some(ref p) = pip {
            d.insert("pip".into(), json!(p));
        }
        if let Some(b) = battery() {
            d.insert("batt".into(), b);
        }
        if send_msg(out, &json!({"sys": Value::Object(d)})).is_err() {
            return; // port closed
        }
        ticks += 1;
        if ticks.is_multiple_of(60) {
            if let Some(p) = public_ip() {
                pip = Some(p);
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/* ---- PTY terminal ---- */
fn pty_relay(out: &Out, first: &Value) {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    let rows = first["rows"].as_u64().unwrap_or(24) as u16;
    let cols = first["cols"].as_u64().unwrap_or(80) as u16;
    let pty = native_pty_system();
    let pair = match pty.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(_) => return,
    };
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut cmd = CommandBuilder::new(&shell);
    cmd.arg("-l");
    cmd.env("TERM", "xterm-256color");
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(_) => return,
    };
    drop(pair.slave);
    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(_) => return,
    };
    let master = pair.master;

    let out2 = out.clone();
    let rt = std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
                    if send_msg(&out2, &json!({"ev": "output", "b64": b64})).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut stdin = io::stdin();
    while let Some(m) = read_msg(&mut stdin) {
        match m["cmd"].as_str() {
            Some("pty_write") => {
                if let Some(data) = m["data"].as_str() {
                    let _ = writer.write_all(data.as_bytes());
                    let _ = writer.flush();
                }
            }
            Some("pty_resize") => {
                let r = m["rows"].as_u64().unwrap_or(24) as u16;
                let c = m["cols"].as_u64().unwrap_or(80) as u16;
                let _ = master.resize(PtySize {
                    rows: r,
                    cols: c,
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
            Some("pty_kill") => break,
            _ => {}
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    drop(writer);
    drop(master);
    let _ = rt.join();
    let _ = send_msg(out, &json!({"ev": "exit"}));
}

fn main() {
    let out: Out = Arc::new(Mutex::new(io::stdout()));
    let d = zwire_dir();
    let mut stdin = io::stdin();
    while let Some(msg) = read_msg(&mut stdin) {
        if let Some(cmd) = msg["cmd"].as_str() {
            match cmd {
                "pty_spawn" => {
                    pty_relay(&out, &msg);
                    break;
                }
                "sysinfo_start" => {
                    stream_sysinfo(&out);
                    break;
                }
                "get" => {
                    let _ = send_msg(
                        &out,
                        &json!({"ok": true, "scheme": current_scheme(&d), "ui": current_ui(&d)}),
                    );
                    continue;
                }
                _ => {}
            }
        }
        if !msg["ui"].is_null() {
            let ui = write_ui(&d, &msg["ui"]);
            let _ = send_msg(&out, &json!({"ok": true, "ui": ui}));
            continue;
        }
        if let Some(s) = msg["scheme"].as_str() {
            if ALLOWED.contains(&s) {
                write_scheme(&d, s);
                let _ = send_msg(&out, &json!({"ok": true, "scheme": s}));
            } else {
                let _ = send_msg(&out, &json!({"ok": false}));
            }
        }
    }
}
