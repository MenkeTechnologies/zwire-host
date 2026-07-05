//! Exercises the native-messaging protocol against the real binary.
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

fn send(w: &mut impl Write, v: &Value) {
    let d = serde_json::to_vec(v).unwrap();
    w.write_all(&(d.len() as u32).to_le_bytes()).unwrap();
    w.write_all(&d).unwrap();
    w.flush().unwrap();
}
fn recv(r: &mut impl Read) -> Option<Value> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).ok()?;
    let n = u32::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

#[test]
fn get_returns_scheme_and_ui() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zwire-host"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    send(&mut si, &json!({"cmd": "get"}));
    let resp = recv(&mut so).expect("a reply");
    assert_eq!(resp["ok"], json!(true));
    assert!(resp["scheme"].is_string(), "scheme present: {resp}");
    assert!(resp["ui"].is_object(), "ui present: {resp}");
    drop(si); // EOF -> host exits
    let _ = child.wait();
}

#[test]
fn sysinfo_stream_has_core_fields() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zwire-host"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut si = child.stdin.take().unwrap();
    let mut so = child.stdout.take().unwrap();
    send(&mut si, &json!({"cmd": "sysinfo_start"}));
    let m = recv(&mut so).expect("a sys frame");
    let sys = &m["sys"];
    for k in ["cpu", "mem", "uptime", "load"] {
        assert!(!sys[k].is_null(), "missing {k}: {m}");
    }
    let _ = child.kill();
    let _ = child.wait();
}
