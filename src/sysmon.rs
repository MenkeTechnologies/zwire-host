//! Live system-stats streamer.
//!
//! `sysinfo_start` spawns a [`Monitor`]: a background thread that pushes a
//! `{"sys":{…}}` frame every `interval_ms` until the connection closes or the
//! caller sends `sysinfo_stop` (which drops the `Monitor`). Running on its own
//! thread means the connection stays free for other RPCs — a HUD can stream
//! stats *and* run a shell over the same pipe.
use crate::proto::{send_msg, Out};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A running stats stream. Dropping it stops the thread and joins it.
pub struct Monitor {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Monitor {
    /// Start streaming `{"sys":…}` frames to `out` every `interval_ms`
    /// (floored at 250 ms). `id`, when non-empty, is echoed on every frame so a
    /// multiplexed client can tell streams apart.
    pub fn start(out: &Out, interval_ms: u64, id: String) -> Monitor {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let o = out.clone();
        let interval = Duration::from_millis(interval_ms.max(250));
        let handle = std::thread::spawn(move || stream(&o, &s2, interval, &id));
        Monitor {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Collect one stats snapshot as a JSON object. Shared by the streamer and the
/// one-shot `sysinfo_once` command.
pub fn snapshot(dt: f64, nets: &sysinfo::Networks, sys: &sysinfo::System) -> Value {
    use sysinfo::{Components, Disks, System};
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
    for (_, n) in nets {
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
    if let Some(b) = battery() {
        d.insert("batt".into(), b);
    }
    Value::Object(d)
}

fn stream(out: &Out, stop: &AtomicBool, interval: Duration, id: &str) {
    use sysinfo::{Networks, System};
    let mut sys = System::new();
    let mut nets = Networks::new_with_refreshed_list();
    let mut pip = public_ip();
    let mut last = Instant::now();
    let mut ticks: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        nets.refresh(true);
        let dt = last.elapsed().as_secs_f64().max(0.1);
        last = Instant::now();

        let mut snap = snapshot(dt, &nets, &sys);
        if let Some(ref p) = pip {
            if let Some(obj) = snap.as_object_mut() {
                obj.insert("pip".into(), json!(p));
            }
        }
        let mut frame = json!({ "sys": snap });
        if !id.is_empty() {
            frame["id"] = json!(id);
        }
        if send_msg(out, &frame).is_err() {
            return; // port closed
        }
        ticks += 1;
        if ticks.is_multiple_of(60) {
            if let Some(p) = public_ip() {
                pip = Some(p);
            }
        }
        // Sleep in short slices so `sysinfo_stop` takes effect promptly.
        let mut slept = Duration::ZERO;
        while slept < interval && !stop.load(Ordering::Relaxed) {
            let slice = Duration::from_millis(100).min(interval - slept);
            std::thread::sleep(slice);
            slept += slice;
        }
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// The route-to-the-internet local IP (no packets are actually sent).
pub fn local_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

fn public_ip() -> Option<String> {
    use std::io::{Read, Write};
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
