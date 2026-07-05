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

### `[UNIVERSAL LOCAL HOST // SYSINFO · FS · EXEC · PTY · KV · OS]`

> *"One pipe. One binary. The whole machine — reachable from anywhere."*

`zwire-host` is a single self-contained Rust binary (~500 KB, no Python, no
`psutil`) that exposes the local machine to **any app** over one JSON message
protocol. It began as the Chrome **native-messaging host** for
[`zwire`](https://github.com/MenkeTechnologies/zwire)'s HUD; it is now a
**universal local endpoint** you can talk to from a browser extension *and* from
tmux, emacs, desktop apps, plugins, shell scripts, and any language — because it
also runs as a **Unix-socket daemon speaking newline-delimited JSON**, the one
protocol every tool already has.

It streams live **system stats** (`sysinfo`), runs **PTY terminals**
(`portable-pty`), crawls the **filesystem**, **execs** commands, keeps a
per-app **key/value store**, and does **clipboard / notify / open**. Every
capability is reachable over every transport, and the whole thing is also a
**Rust library** so sibling hosts (e.g. `zpwrchrome-host`) can embed it.

### [`zwire`](https://github.com/MenkeTechnologies/zwire) &middot; [`zpwrchrome`](https://github.com/MenkeTechnologies/zpwrchrome) &middot; [`strykelang`](https://github.com/MenkeTechnologies/strykelang)

---

## Table of Contents

- [\[0x00\] Overview](#0x00-overview)
- [\[0x01\] Transports](#0x01-transports)
- [\[0x02\] Protocol / Commands](#0x02-protocol--commands)
- [\[0x03\] CLI](#0x03-cli)
- [\[0x04\] Library use (embed as a dependency)](#0x04-library-use-embed-as-a-dependency)
- [\[0x05\] Chrome install](#0x05-chrome-install)
- [\[0x06\] Build · Cross-Platform · CI](#0x06-build--cross-platform--ci)
- [\[0x07\] License](#0x07-license)

---

## [0x00] Overview

Extensions, editors, and plugins can't read the machine or spawn a shell.
`zwire-host` does the privileged work once and hands it to everyone: a live
statusbar (cpu / mem / net / battery / temp …), an embedded terminal, a
filesystem crawler, a command runner, and a small state store. Shipping it as
one static Rust binary means the consuming bundle has **zero runtime
dependencies** — no system Python, no `pip install psutil`, nothing to break on
a fresh machine.

## [0x01] Transports

Both transports feed the **same dispatcher**, so every command below works over
either one.

| Transport | For | Framing |
|---|---|---|
| **Native messaging** (default) | Chrome / browser extensions | little-endian `u32` length + JSON body, on `stdin`/`stdout` |
| **Local-socket daemon** (`serve`) | tmux, emacs, desktop apps, plugins, any language | newline-delimited JSON (one object per line) |

The daemon uses each platform's native local IPC — a **Unix domain socket** on
macOS/Linux and a **named pipe** on Windows — so it runs everywhere your apps do:

- **macOS / Linux** — `$ZWIRE_HOST_SOCK`, else `$XDG_RUNTIME_DIR/zwire-host.sock`,
  else `~/.zwire/host.sock`. Created `0600` under a `0700` dir — owner-only,
  since it exposes `exec`/`fs`/`pty`.
- **Windows** — `$ZWIRE_HOST_SOCK`, else the per-user pipe
  `\\.\pipe\zwire-host-<user>`. (`--socket <name>` overrides the pipe name.)

Requests may carry an `id`; it is echoed on the matching reply so a client can
multiplex many in-flight requests, streams, and terminals over one connection.

## [0x02] Protocol / Commands

**Discovery & state**

| Message | Reply / effect |
|---|---|
| `{"cmd":"hello"}` | `{ok,host,version,os,arch,pid,caps:[…]}` — feature-test the host. |
| `{"cmd":"hostinfo"}` | one-shot machine facts: os, arch, kernel, hostname, user, cpus, mem, LAN ip. |
| `{"cmd":"kv_set","app":"myapp","key":"cfg","value":{…}}` | write `~/.myapp/kv/cfg.json`. |
| `{"cmd":"kv_get" / "kv_merge" / "kv_del" / "kv_keys",…}` | read / shallow-merge / delete / list keys. |

**System stats**

| Message | Reply / effect |
|---|---|
| `{"cmd":"sysinfo_once"}` | one `{sys:{…}}` snapshot. |
| `{"cmd":"sysinfo_start","interval_ms":2000}` | **stream** `{sys:{…}}` every interval — cpu · mem · swap · disk · net rate · load · uptime · battery · temp · host · LAN/WAN ip. |
| `{"cmd":"sysinfo_stop"}` | stop the stream. |

**Filesystem** (paths accept a leading `~`)

| Message | Reply / effect |
|---|---|
| `{"cmd":"fs_read","path":…}` | `{ok,b64,text?}`. |
| `{"cmd":"fs_write"/"fs_append","path":…,"text"\|"b64":…}` | write / append. |
| `{"cmd":"fs_list","path":…}` | one-level `{entries:[{name,dir,size}]}`. |
| `{"cmd":"fs_walk","path":…,"depth"?,"ext"?,"dirs_only"?,"contains"?}` | **recursive crawl** → `{count,truncated,entries:[{path,name,dir,size}]}`. |
| `{"cmd":"fs_stat" / "fs_mkdir" / "fs_rm","path":…}` | stat / mkdir -p / remove (`recursive` for dirs). |

**Exec & OS**

| Message | Reply / effect |
|---|---|
| `{"cmd":"exec","program":…,"args":[…],"cwd"?,"env"?,"stdin"?}` | run to completion → `{ok,code,stdout,stderr}` (base64). |
| `{"cmd":"open","target":…}` | open a path/URL with the OS default handler. |
| `{"cmd":"clipboard_get"}` / `{"cmd":"clipboard_set","text":…}` | read / write the clipboard. |
| `{"cmd":"notify","title":…,"body":…}` | desktop notification. |

**PTY terminals** (multiplexed by `id`)

| Message | Reply / effect |
|---|---|
| `{"cmd":"pty_spawn","id"?,"rows":R,"cols":C,"shell"?,"args"?,"cwd"?,"env"?}` | spawn a shell; stream `{ev:"output","b64":…}` (and `pty:id` when keyed). |
| `{"cmd":"pty_write","id"?,"data"\|"b64":…}` | feed input. |
| `{"cmd":"pty_resize","id"?,"rows":R,"cols":C}` / `{"cmd":"pty_kill","id"?}` | resize / kill; kill emits `{ev:"exit"}`. |

**Legacy zwire scheme/ui** (unchanged): `{"cmd":"get"}`, `{"scheme":"matrix"}`,
`{"ui":{…}}` bridge `~/.zwire/hud-scheme` + `~/.zwire/hud-ui.json`.

## [0x03] CLI

```sh
zwire-host serve &                                   # run the socket daemon
zwire-host call '{"cmd":"hostinfo"}'                 # one request, one reply
zwire-host call '{"cmd":"fs_walk","path":"~/src","ext":"rs"}'
echo '{"cmd":"exec","program":"git","args":["status"]}' | zwire-host call
zwire-host call --stream '{"cmd":"sysinfo_start"}'   # keep printing frames
```

From **any** tool that can write a line to the endpoint — no client library
needed. `zwire-host call` is the portable path; or connect to the socket/pipe
directly:

```sh
# macOS / Linux — raw Unix socket
printf '{"cmd":"sysinfo_once"}\n' | nc -U ~/.zwire/host.sock
# any platform — via the bundled client
zwire-host call '{"cmd":"sysinfo_once"}'
```

## [0x04] Library use (embed as a dependency)

The crate is a library too (`zwire_host`), so sibling hosts can pull it in to
crawl and exec without re-implementing anything:

```toml
[dependencies]
zwire-host = { git = "https://github.com/MenkeTechnologies/zwire-host" }
```

```rust
use zwire_host::api;

// crawl the filesystem
for e in api::walk("~/src", Some("rs")) {
    println!("{}", e.path.display());
}

// run a command, get bytes back
let out = api::exec("git", ["status", "--porcelain"]).unwrap();
println!("exit {:?}: {}", out.code, out.stdout_str());
```

Or drive the whole dispatcher yourself over any transport with
`zwire_host::{Peer, Session}`, or just delegate `main` to
`zwire_host::run(std::env::args().skip(1).collect())`.

## [0x05] Chrome install

Point a native-messaging host manifest's `path` at the binary and list the
allowed extension origins:

```json
{ "name": "com.zwire.hud", "type": "stdio",
  "path": "/abs/path/to/zwire-host",
  "allowed_origins": ["chrome-extension://<id>/"] }
```

Drop it in the browser's `NativeMessagingHosts/` directory (or the profile's).
`zwire`'s `scripts/localinstall.sh` builds this binary and wires the manifest
automatically when packaging the `.app`.

## [0x06] Build · Cross-Platform · CI

```sh
cargo build --release          # -> target/release/zwire-host (~500 KB)
cargo test                     # exercises the protocol over both transports
```

`sysinfo` and `portable-pty` abstract the OS, so the same source builds for
**macOS · Linux · Windows**. Both transports work on all three: native messaging
everywhere, and the `serve`/`call` daemon over Unix domain sockets on macOS/Linux
and named pipes on Windows (via `interprocess`, a Windows-only dependency).
Battery reporting is macOS-only (via `pmset`) until a native reader is added for
other platforms.

CI runs the four canonical polish gates on Ubuntu + macOS + Windows:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps                        # RUSTDOCFLAGS=-D warnings
cargo test
```

## [0x07] License

MIT © MenkeTechnologies
