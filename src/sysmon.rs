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
/// one-shot `sysinfo_once` command. `disks` is passed in (rather than refreshed
/// here) so the streamer can keep it alive across ticks — `Disk::usage()`
/// reports bytes *since the last refresh*, so a persistent, per-tick-refreshed
/// `Disks` is what turns those deltas into a real per-second I/O rate.
pub fn snapshot(
    dt: f64,
    nets: &sysinfo::Networks,
    disks: &sysinfo::Disks,
    sys: &sysinfo::System,
) -> Value {
    use sysinfo::{Components, System};
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

    if let Some(root) = disks.iter().find(|k| k.mount_point() == Path::new("/")) {
        let (t, a) = (root.total_space(), root.available_space());
        if t > 0 {
            d.insert(
                "disk".into(),
                json!({"u": t - a, "t": t, "p": (t - a) * 100 / t}),
            );
        }
    }
    // Disk I/O rate: sum read/written bytes since the last refresh across every
    // disk, converted to bytes-per-second. `{r, w}` matches the Python host's
    // `io` field so a HUD statusbar renders the same IO segment on either host.
    let (mut ior, mut iow) = (0u64, 0u64);
    for k in disks.iter() {
        let u = k.usage();
        ior += u.read_bytes;
        iow += u.written_bytes;
    }
    d.insert(
        "io".into(),
        json!({"r": (ior as f64 / dt) as u64, "w": (iow as f64 / dt) as u64}),
    );

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
    use sysinfo::{Disks, Networks, System};
    let mut sys = System::new();
    let mut nets = Networks::new_with_refreshed_list();
    let mut disks = Disks::new_with_refreshed_list();
    let mut pip = public_ip();
    let mut last = Instant::now();
    let mut ticks: u64 = 0;
    while !stop.load(Ordering::Relaxed) {
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        nets.refresh(true);
        disks.refresh(true);
        let dt = last.elapsed().as_secs_f64().max(0.1);
        last = Instant::now();

        let mut snap = snapshot(dt, &nets, &disks, &sys);
        if let Some(ref p) = pip {
            if let Some(obj) = snap.as_object_mut() {
                obj.insert("pip".into(), json!(p));
            }
        }
        let mut frame = json!({ "sys": snap });
        if !id.is_empty() {
            frame["id"] = json!(id);
        }
        // Log the pushed statusbar frame so the HUD HOST tab shows the (high
        // frequency) sysinfo stream that drives the tmux statusbar — these frames
        // go out via send_msg, not respond(), so they'd otherwise be invisible.
        crate::hostlog::record("rx", &json!({ "cmd": "sysinfo" }), &frame);
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

use crate::osops::local_ip;

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

/// `{"p": percent, "c": on-AC-or-charging}` for the primary battery, or `None`
/// when the machine has no battery (desktop/VM). Each OS reads its native power
/// source: `pmset` on macOS, sysfs on Linux, `GetSystemPowerStatus` on Windows.
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

/// Read the first `type == Battery` under `/sys/class/power_supply` for its
/// `capacity` and `status`; treat a charging/full battery, or any online
/// `Mains` adapter, as "on AC" so `c` matches the macOS semantics.
#[cfg(target_os = "linux")]
fn battery() -> Option<Value> {
    use std::fs;
    let dir = fs::read_dir("/sys/class/power_supply").ok()?;
    let mut pct: Option<i64> = None;
    let mut on_ac = false;
    for entry in dir.flatten() {
        let p = entry.path();
        let kind = fs::read_to_string(p.join("type")).unwrap_or_default();
        match kind.trim() {
            "Battery" => {
                if pct.is_none() {
                    pct = fs::read_to_string(p.join("capacity"))
                        .ok()
                        .and_then(|c| c.trim().parse().ok());
                }
                let status = fs::read_to_string(p.join("status")).unwrap_or_default();
                if matches!(status.trim(), "Charging" | "Full") {
                    on_ac = true;
                }
            }
            "Mains" if fs::read_to_string(p.join("online")).is_ok_and(|s| s.trim() == "1") => {
                on_ac = true;
            }
            _ => {}
        }
    }
    Some(json!({"p": pct?, "c": on_ac}))
}

/// `GetSystemPowerStatus` (kernel32) fills a `SYSTEM_POWER_STATUS`; `BatteryFlag`
/// bit 7 (128) means "no system battery", and 255 percent means "unknown".
#[cfg(target_os = "windows")]
fn battery() -> Option<Value> {
    #[repr(C)]
    struct SystemPowerStatus {
        ac_line_status: u8,
        battery_flag: u8,
        battery_life_percent: u8,
        system_status_flag: u8,
        battery_life_time: u32,
        battery_full_life_time: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemPowerStatus(status: *mut SystemPowerStatus) -> i32;
    }
    let mut s = SystemPowerStatus {
        ac_line_status: 0,
        battery_flag: 0,
        battery_life_percent: 0,
        system_status_flag: 0,
        battery_life_time: 0,
        battery_full_life_time: 0,
    };
    // SAFETY: `GetSystemPowerStatus` only writes the struct we hand it.
    if unsafe { GetSystemPowerStatus(&mut s) } == 0 {
        return None;
    }
    if s.battery_flag == 128 || s.battery_life_percent == 255 {
        return None; // no battery, or percent unknown
    }
    Some(json!({"p": s.battery_life_percent as i64, "c": s.ac_line_status == 1}))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn battery() -> Option<Value> {
    None
}
