//! Subprocess bridge to the `stryke` interpreter for lifecycle hook scripts.
//!
//! zwire does **not** embed strykelang in-process — `VMHelper` is `!Send`/`!Sync`
//! and the runtime's dependency tree (cranelift JIT, a second `bundled` rusqlite)
//! makes in-process embedding both un-buildable and bloated. Instead each hook
//! spawns the standalone `stryke` binary, feeds the event payload as JSON on
//! stdin, and reads `{"actions":[...]}` back on stdout. Process isolation means a
//! panicking or runaway script can never take down the host, and stryke versions
//! independently.
//!
//! Ported verbatim from the Audio-Haxor engine (`src-tauri/src/stryke_runner.rs`)
//! into the shared zwire-host: only the app-specific env-var names change
//! (`AH_EVENT`/`AUDIO_HAXOR_HOOK`/`AUDIO_HAXOR_STRYKE` -> `ZWIRE_*`). The awk-style
//! `run_n`/`filter_keys`/`transform_records` row-pipeline helpers are a separate
//! feature and are intentionally not carried over here.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// The `App` package version bundled with zwire (matches `stryke-app/stryke.toml`).
const STRYKE_APP_VERSION: &str = "0.1.0";

/// Locate the bundled `stryke-app` package (`stryke.toml` + `lib/App.stk` + the cdylib), staged next
/// to the host executable at build time (a sibling `stryke-app/` dir, or `../Resources/stryke-app`
/// inside a macOS `.app`).
fn bundled_stryke_app_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [
        dir.join("stryke-app"),
        dir.join("../Resources/stryke-app"),
        dir.join("../stryke-app"),
    ]
    .into_iter()
    .find(|cand| cand.join("lib").join("App.stk").is_file())
}

/// Ensure the `App` package is present in the stryke store so `use App` resolves with NO user install
/// of stryke-app (or stryke). Copies the bundled package into
/// `$STRYKE_HOME/store/stryke-app@<ver>/` (default `~/.stryke/store/…`) once, on first run; a no-op
/// after that. Best-effort — a missing bundle just leaves the store untouched.
pub fn ensure_stryke_app() {
    let store = std::env::var_os("STRYKE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".stryke")))
        .map(|r| {
            r.join("store")
                .join(format!("stryke-app@{STRYKE_APP_VERSION}"))
        });
    let Some(dest) = store else { return };
    if dest.join("lib").join("App.stk").is_file() {
        return; // already extracted
    }
    let Some(src) = bundled_stryke_app_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(dest.join("lib"));
    for rel in [
        "stryke.toml",
        "lib/App.stk",
        "lib/libstryke_app.dylib",
        "lib/libstryke_app.so",
        "lib/stryke_app.dll",
    ] {
        let s = src.join(rel);
        if s.is_file() {
            let _ = std::fs::copy(&s, dest.join(rel));
        }
    }
}
use std::time::{Duration, Instant};

/// Cap on retained stdout/stderr bytes from a single hook script.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Poll interval while waiting for a hook child to exit.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

static STRYKE_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Resolve the `stryke` binary once and cache it for the process lifetime.
/// Order: `ZWIRE_STRYKE` override, the sibling next to the host executable, then
/// every `PATH` entry, then the common cargo / Homebrew install locations.
pub fn resolve_stryke() -> Option<PathBuf> {
    STRYKE_PATH
        .get_or_init(|| {
            let exe_name = if cfg!(windows) {
                "stryke.exe"
            } else {
                "stryke"
            };

            if let Some(p) = std::env::var_os("ZWIRE_STRYKE") {
                let pb = PathBuf::from(p);
                if pb.is_file() {
                    return Some(pb);
                }
            }
            // A `stryke` shipped next to the host executable, so a packaged zwire
            // uses its own bundled copy rather than whatever is on PATH.
            if let Ok(exe) = std::env::current_exe() {
                if let Some(sib) = exe.parent().map(|d| d.join(exe_name)) {
                    if sib.is_file() {
                        return Some(sib);
                    }
                }
            }
            if let Some(path) = std::env::var_os("PATH") {
                for dir in std::env::split_paths(&path) {
                    let cand = dir.join(exe_name);
                    if cand.is_file() {
                        return Some(cand);
                    }
                }
            }
            let mut fixed: Vec<PathBuf> = Vec::new();
            if let Some(home) = std::env::var_os("HOME") {
                fixed.push(PathBuf::from(home).join(".cargo/bin").join(exe_name));
            }
            fixed.push(PathBuf::from("/opt/homebrew/bin/stryke"));
            fixed.push(PathBuf::from("/usr/local/bin/stryke"));
            fixed.into_iter().find(|c| c.is_file())
        })
        .clone()
}

/// Outcome of running a hook script.
pub struct RunOutcome {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
    pub timed_out: bool,
}

/// Read a pipe into a `String`, stopping after `MAX_OUTPUT_BYTES` and lossily
/// decoding UTF-8 (hook output is data, not a terminal — ANSI is not expected).
fn read_capped<R: Read>(mut r: R) -> String {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let room = MAX_OUTPUT_BYTES.saturating_sub(buf.len());
                if room == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n.min(room)]);
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Run `stryke <script_path>` with `event_json` on stdin and `ZWIRE_EVENT` set to
/// `event_name`. Kills the child if it runs longer than `timeout`. Output is
/// truncated to `MAX_OUTPUT_BYTES`.
pub fn run_script(
    script_path: &Path,
    event_name: &str,
    event_json: &str,
    timeout: Duration,
) -> Result<RunOutcome, String> {
    let stryke = resolve_stryke().ok_or_else(|| "stryke binary not found on PATH".to_string())?;
    // Self-contained scripting: extract the bundled `App` package into the store on first run so
    // `use App` resolves without the user ever installing stryke-app (or stryke) themselves.
    ensure_stryke_app();

    let mut child = Command::new(&stryke)
        .arg(script_path)
        .env("ZWIRE_EVENT", event_name)
        .env("ZWIRE_HOOK", "1")
        // Let scripts/hooks resolve `App::here()` to this app over the automation bus.
        .env("ZGUI_APP", "zwire")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn stryke: {e}"))?;

    // Feed the payload, then drop stdin to signal EOF so the script's
    // `<>` / slurp returns.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(event_json.as_bytes());
    }

    // Drain stdout/stderr on dedicated threads so a chatty script can't
    // deadlock by filling a pipe buffer while we poll for exit.
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let out_t = std::thread::spawn(move || out_pipe.map(read_capped).unwrap_or_default());
    let err_t = std::thread::spawn(move || err_pipe.map(read_capped).unwrap_or_default());

    let start = Instant::now();
    let mut timed_out = false;
    let code;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                code = status.code();
                break;
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    code = None;
                    break;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => return Err(format!("wait stryke: {e}")),
        }
    }

    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    Ok(RunOutcome {
        stdout,
        stderr,
        code,
        timed_out,
    })
}

/// Run inline stryke code (`stryke -E <code>`) with an optional `stdin_data`
/// string; same drain-on-threads + timeout + output-cap discipline as
/// [`run_script`]. Backs the command-wizard "stryke script" step.
pub fn run_code(code: &str, stdin_data: &str, timeout: Duration) -> Result<RunOutcome, String> {
    let stryke = resolve_stryke().ok_or_else(|| "stryke binary not found on PATH".to_string())?;
    ensure_stryke_app();

    let mut child = Command::new(&stryke)
        .arg("-E")
        .arg(code)
        .env("ZGUI_APP", "zwire")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn stryke: {e}"))?;

    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(stdin_data.as_bytes());
    }

    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let out_t = std::thread::spawn(move || out_pipe.map(read_capped).unwrap_or_default());
    let err_t = std::thread::spawn(move || err_pipe.map(read_capped).unwrap_or_default());

    let start = Instant::now();
    let mut timed_out = false;
    let code_out;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                code_out = status.code();
                break;
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    code_out = None;
                    break;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => return Err(format!("wait stryke: {e}")),
        }
    }

    Ok(RunOutcome {
        stdout: out_t.join().unwrap_or_default(),
        stderr: err_t.join().unwrap_or_default(),
        code: code_out,
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_capped_truncates_at_limit() {
        let big = vec![b'x'; MAX_OUTPUT_BYTES + 4096];
        let s = read_capped(&big[..]);
        assert_eq!(s.len(), MAX_OUTPUT_BYTES);
    }

    #[test]
    fn read_capped_passes_small_input() {
        let s = read_capped(&b"hello"[..]);
        assert_eq!(s, "hello");
    }

    #[test]
    fn run_script_errors_without_binary() {
        // Only asserts the Err path shape when stryke is genuinely absent, so CI
        // without stryke still passes.
        if resolve_stryke().is_none() {
            let r = run_script(
                Path::new("/nonexistent.st"),
                "test",
                "{}",
                Duration::from_secs(1),
            );
            assert!(r.is_err());
        }
    }

    #[test]
    fn run_code_runs_inline_when_stryke_present() {
        // Only asserts behavior when stryke is available; CI without it passes.
        if resolve_stryke().is_some() {
            let r = run_code("p 6 * 7", "", Duration::from_secs(5)).expect("run");
            assert!(!r.timed_out);
            assert!(r.stdout.contains("42"), "stdout: {:?}", r.stdout);
        }
    }
}
