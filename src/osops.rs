//! OS-integration capabilities: open, clipboard, notify, hostinfo.
//!
//! These shell out to the platform's standard tools rather than pulling in
//! native-API crates, keeping the binary small and dependency-light. When a
//! platform tool is missing the reply is `{"ok":false,"err":…}` rather than a
//! crash, so callers can degrade gracefully.
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

/// Dispatch an OS command. `id` stamping is handled by the caller.
pub fn handle(cmd: &str, req: &Value) -> Value {
    match cmd {
        "open" => open(req),
        "clipboard_get" => clipboard_get(),
        "clipboard_set" => clipboard_set(req),
        "notify" => notify(req),
        "hostinfo" => hostinfo(),
        _ => json!({"ok": false, "err": "unknown_cmd"}),
    }
}

fn ok_err(res: std::io::Result<std::process::ExitStatus>) -> Value {
    match res {
        Ok(s) if s.success() => json!({"ok": true}),
        Ok(s) => json!({"ok": false, "err": format!("exit {:?}", s.code())}),
        Err(e) => json!({"ok": false, "err": e.to_string()}),
    }
}

/// Run `program args...`, feeding `input` on stdin, and report success.
fn run_with_stdin(program: &str, args: &[&str], input: &[u8]) -> Value {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "err": e.to_string()}),
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(input);
    }
    ok_err(child.wait())
}

/// Capture stdout of `program args...` as a UTF-8 string.
fn capture(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        None
    }
}

/* ---- open a path / url with the default handler ---- */
fn open(req: &Value) -> Value {
    let Some(target) = req["target"].as_str() else {
        return json!({"ok": false, "err": "no_target"});
    };
    #[cfg(target_os = "macos")]
    let res = Command::new("open").arg(target).status();
    #[cfg(target_os = "linux")]
    let res = Command::new("xdg-open").arg(target).status();
    #[cfg(target_os = "windows")]
    let res = Command::new("cmd")
        .args(["/C", "start", "", target])
        .status();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let res: std::io::Result<std::process::ExitStatus> = Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "unsupported",
    ));
    ok_err(res)
}

/* ---- clipboard ---- */
fn clipboard_get() -> Value {
    #[cfg(target_os = "macos")]
    let text = capture("pbpaste", &[]);
    #[cfg(target_os = "linux")]
    let text = capture("wl-paste", &["-n"])
        .or_else(|| capture("xclip", &["-selection", "clipboard", "-o"]))
        .or_else(|| capture("xsel", &["-b"]));
    #[cfg(target_os = "windows")]
    let text = capture("powershell", &["-NoProfile", "-Command", "Get-Clipboard"]);
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let text: Option<String> = None;

    match text {
        Some(t) => json!({"ok": true, "text": t}),
        None => json!({"ok": false, "err": "no_clipboard_tool"}),
    }
}

fn clipboard_set(req: &Value) -> Value {
    let text = req["text"].as_str().unwrap_or("");
    #[cfg(target_os = "macos")]
    {
        run_with_stdin("pbcopy", &[], text.as_bytes())
    }
    #[cfg(target_os = "linux")]
    {
        let via_wl = run_with_stdin("wl-copy", &[], text.as_bytes());
        if via_wl["ok"] == json!(true) {
            via_wl
        } else {
            run_with_stdin("xclip", &["-selection", "clipboard"], text.as_bytes())
        }
    }
    #[cfg(target_os = "windows")]
    {
        run_with_stdin("clip", &[], text.as_bytes())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        json!({"ok": false, "err": "unsupported"})
    }
}

/* ---- desktop notification ---- */
fn notify(req: &Value) -> Value {
    let title = req["title"].as_str().unwrap_or("zwire");
    let body = req["body"].as_str().unwrap_or("");
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification {} with title {}",
            applescript_quote(body),
            applescript_quote(title)
        );
        ok_err(Command::new("osascript").args(["-e", &script]).status())
    }
    #[cfg(target_os = "linux")]
    {
        ok_err(Command::new("notify-send").args([title, body]).status())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (title, body);
        json!({"ok": false, "err": "unsupported"})
    }
}

#[cfg(target_os = "macos")]
fn applescript_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The route-to-the-internet local IP (no packets are actually sent). Lives
/// here (always compiled) so it's available with or without the sysinfo caps.
pub fn local_ip() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

/// This host's short name, from `sysinfo` when compiled in, else the environment.
pub fn hostname() -> Option<String> {
    #[cfg(feature = "sysinfo-caps")]
    let h = sysinfo::System::host_name();
    #[cfg(not(feature = "sysinfo-caps"))]
    let h = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok());
    h.map(|h| h.split('.').next().unwrap_or(&h).to_string())
}

/* ---- one-shot machine facts ---- */
fn hostinfo() -> Value {
    let env = |k: &str| std::env::var(k).ok();
    #[cfg_attr(not(feature = "sysinfo-caps"), allow(unused_mut))]
    let mut v = json!({
        "ok": true,
        "host_version": crate::VERSION,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "family": std::env::consts::FAMILY,
        "hostname": hostname(),
        "user": env("USER").or_else(|| env("USERNAME")),
        "home": env("HOME").or_else(|| env("USERPROFILE")),
        "shell": env("SHELL"),
        "cpus": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "lan_ip": local_ip(),
        "pid": std::process::id(),
    });
    #[cfg(feature = "sysinfo-caps")]
    if let Some(obj) = v.as_object_mut() {
        use sysinfo::System;
        obj.insert("kernel".into(), json!(System::kernel_version()));
        obj.insert("os_version".into(), json!(System::long_os_version()));
        obj.insert("mem_total".into(), json!(System::new_all().total_memory()));
    }
    v
}
