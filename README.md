```
███████╗██╗    ██╗██╗██████╗ ███████╗    ██╗  ██╗ ██████╗ ███████╗████████╗
╚══███╔╝██║    ██║██║██╔══██╗██╔════╝    ██║  ██║██╔═══██╗██╔════╝╚══██╔══╝
  ███╔╝ ██║ █╗ ██║██║██████╔╝█████╗█████╗███████║██║   ██║███████╗   ██║
 ███╔╝  ██║███╗██║██║██╔══██╗██╔══╝╚════╝██╔══██║██║   ██║╚════██║   ██║
███████╗╚███╔███╔╝██║██║  ██║███████╗    ██║  ██║╚██████╔╝███████║   ██║
╚══════╝ ╚══╝╚══╝ ╚═╝╚═╝  ╚═╝╚══════╝    ╚═╝  ╚═╝ ╚═════╝ ╚══════╝   ╚═╝
```

[![CI](https://github.com/MenkeTechnologies/zwire-host/actions/workflows/ci.yml/badge.svg)](https://github.com/MenkeTechnologies/zwire-host/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![platforms](https://img.shields.io/badge/platforms-macOS%20%C2%B7%20Linux%20%C2%B7%20Windows-05d9e8?style=flat-square)](https://github.com/MenkeTechnologies/zwire-host)

### `[NATIVE-MESSAGING HOST // SCHEME · SYSINFO · PTY]`

> *"One pipe. One binary. The whole machine."*

`zwire-host` is the Chrome **native-messaging host** for
[`zwire`](https://github.com/MenkeTechnologies/zwire) — a single self-contained
Rust binary (~500 KB, no Python, no `psutil`) that the `hud-internal` extension
talks to over one length-prefixed-JSON stdio pipe. It does three jobs: bridges the
shared color **scheme** + visual-effect prefs to/from `~/.zwire`, **streams live
system stats** (`sysinfo`) for the HUD statusbar, and runs a **PTY terminal**
(`portable-pty`) for the embedded terminal. Cross-platform (macOS · Linux · Windows).

 ┌──────────────────────────────────────────────────────────────┐
 │ STATUS: ONLINE &nbsp;&nbsp; THREAT LEVEL: NEON &nbsp;&nbsp; SIGNAL: ████████░░ │
 └──────────────────────────────────────────────────────────────┘

### [`zwire`](https://github.com/MenkeTechnologies/zwire) &middot; [`zpwrchrome`](https://github.com/MenkeTechnologies/zpwrchrome) &middot; [`strykelang`](https://github.com/MenkeTechnologies/strykelang)

---

## Table of Contents

- [\[0x00\] Overview](#0x00-overview)
- [\[0x01\] Protocol](#0x01-protocol)
- [\[0x02\] Build](#0x02-build)
- [\[0x03\] Install](#0x03-install)
- [\[0x04\] Cross-Platform](#0x04-cross-platform)
- [\[0x05\] Development & CI](#0x05-development--ci)
- [\[0x06\] License](#0x06-license)

---

## [0x00] Overview

Chrome extensions cannot read the machine or spawn a shell. `zwire`'s HUD needs
both — a live statusbar (cpu / mem / net / battery / temp …) and an embedded
terminal — so a **native-messaging host** does the privileged work and streams it
back to the extension. Shipping that host as one static Rust binary means the
whole browser bundle (`zwire.app`) has **zero runtime dependencies**: no system
Python, no `pip install psutil`, nothing to break on a fresh machine.

## [0x01] Protocol

Chrome native messaging: each message is a little-endian `u32` byte length
followed by a UTF-8 JSON body, on `stdin`/`stdout`.

| Message in | Effect / reply |
|---|---|
| `{"cmd":"get"}` | reply `{"ok":true,"scheme":…,"ui":{…}}` from `~/.zwire/`. |
| `{"scheme":"matrix"}` | write `~/.zwire/hud-scheme` (drives the compiled color mixer). |
| `{"ui":{…}}` | merge into `~/.zwire/hud-ui.json` (light / scanlines / glow / …). |
| `{"cmd":"sysinfo_start"}` | **stream** `{"sys":{…}}` every 2 s — cpu · mem · swap · disk · net rate · load · uptime · battery · temp · host · LAN/WAN ip. |
| `{"cmd":"pty_spawn","rows":R,"cols":C}` | spawn a login shell in a PTY, then relay `pty_write` / `pty_resize` / `pty_kill` in and `{"ev":"output","b64":…}` / `{"ev":"exit"}` out. |

The first message selects the mode: `sysinfo_start` and `pty_spawn` take over the
pipe for the life of the connection; everything else is request/reply.

## [0x02] Build

```sh
cargo build --release          # -> target/release/zwire-host (~500 KB)
cargo test                     # exercises the native-messaging protocol
```

## [0x03] Install

Point a Chrome native-messaging host manifest's `path` at the binary and list the
allowed extension origins:

```json
{ "name": "com.zwire.hud", "type": "stdio",
  "path": "/abs/path/to/zwire-host",
  "allowed_origins": ["chrome-extension://<id>/"] }
```

Drop it in the browser's `NativeMessagingHosts/` directory (or the profile's).
`zwire`'s `scripts/localinstall.sh` builds this binary and wires the manifest
automatically when packaging the `.app`.

## [0x04] Cross-Platform

`sysinfo` and `portable-pty` abstract the OS, so the same source builds for
**macOS · Linux · Windows**. The only platform-specific path is battery reporting
(macOS reads `pmset`; other platforms return none until a native reader is added).

## [0x05] Development & CI

CI runs the four canonical polish gates on Ubuntu + macOS:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps                        # RUSTDOCFLAGS=-D warnings
cargo test
```

## [0x06] License

MIT © MenkeTechnologies
