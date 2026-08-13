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
[![docs](https://img.shields.io/badge/docs-HUD-ff2e97?style=flat-square)](https://menketechnologies.github.io/zwire-host/)

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
(`portable-pty`), crawls and **watches/tails the filesystem**, **execs**
commands, runs **background jobs** that notify on completion, lists/kills
**processes**, brokers a **pub/sub event bus** that **federates across a mesh of
peered hosts**, keeps a per-app **key/value store**, and does
**clipboard / notify / open**. Every capability is reachable over every
transport, and the whole thing is also a **Rust library** so sibling hosts (e.g.
`zpwrchrome-host`) can embed it.

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
filesystem crawler, a command runner, a small state store, and a client onto the
GUI Automation Bus so one app can call another's typed verbs. Shipping it as
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
| `{"cmd":"sysinfo_start","interval_ms":2000}` | **stream** `{sys:{…}}` every interval — cpu · mem · swap · disk · net rate · disk I/O rate · load · uptime · battery · temp · host · LAN/WAN ip. |
| `{"cmd":"sysinfo_stop"}` | stop the stream. |

**Filesystem** (paths accept a leading `~`)

| Message | Reply / effect |
|---|---|
| `{"cmd":"fs_read","path":…}` | `{ok,b64,text?}`. |
| `{"cmd":"fs_write"/"fs_append","path":…,"text"\|"b64":…}` | write / append. |
| `{"cmd":"fs_list","path":…}` | one-level `{entries:[{name,dir,size}]}`. |
| `{"cmd":"fs_walk","path":…,"depth"?,"ext"?,"dirs_only"?,"contains"?}` | **recursive crawl** → `{count,truncated,entries:[{path,name,dir,size}]}`. |
| `{"cmd":"fs_stat" / "fs_mkdir" / "fs_rm","path":…}` | stat / mkdir -p / remove (`recursive` for dirs). |

**File browser ops** — the wider surface behind a graphical file manager (`src/fsx.rs`).
These reply `{ok:true,data:…}` / `{ok:false,err:…}`; argument names are snake_case.

| Message | Reply / effect |
|---|---|
| `{"cmd":"fs_list_dir","dir_path":…,"include_hidden"?}` | `{entries:[{name,path,isDir,size,sizeFormatted,modified,created,ext}],path}`, directories first. |
| `{"cmd":"fs_list_subdirs","dir_path":…,"include_hidden"?}` | `[{name,path}]` — directories only, for a tree pane. |
| `{"cmd":"fs_get_info","path":…}` | kind · recursive size + item count · mtime/ctime/atime · mode octal + `ls -l` string · uid/gid · symlink target. |
| `{"cmd":"fs_folder_size","folder_path":…,"timeout_ms"?}` | `{bytes,files}` — bounded recursive walk. |
| `{"cmd":"fs_disk_usage","path":…}` | `{total,available,used,usedPct,mount}` for the mount holding the path (needs `sysinfo-caps`). |
| `{"cmd":"fs_xattrs","path":…}` | `[{name,size}]` extended attributes (Unix). |
| `{"cmd":"fs_git_status","dir_path":…}` | `{<abs path>: "<XY>"}` porcelain codes; empty outside a repo. |
| `{"cmd":"fs_hash","path":…,"algos"?}` | `{path,size,digests:{sha256}}` — streamed SHA-256. |
| `{"cmd":"fs_grep","root":…,"needle":…,"case_insensitive"?,"max_results"?}` | `[{path,line,text}]`; skips dotdirs, binaries and files > 4 MiB. |
| `{"cmd":"fs_find_duplicates","dir":…,"recursive"?,"min_size_bytes"?}` | `[{hash,size,paths}]` — grouped by content, biggest reclaim first. |
| `{"cmd":"fs_compare_dirs","dir_a":…,"dir_b":…}` | `{onlyInA,onlyInB,different}`; equal-size files are confirmed by hash. |
| `{"cmd":"fs_diff","path_a":…,"path_b":…}` | `[{tag,aLineStart,aLineEnd,bLineStart,bLineEnd,text}]` unified text diff. |
| `{"cmd":"fs_compress","paths":[…],"archive_path":…}` | write a deflate `.zip`. |
| `{"cmd":"fs_extract","archive_path":…,"dest_dir":…}` | read `.zip` / `.tar` / `.tar.gz` / `.tgz` into a NEW directory (zip-slip guarded). |
| `{"cmd":"fs_read_file_base64" / "fs_read_head" / "fs_read_head_bytes","file_path":…,"max_bytes"?}` | capped whole-file base64 / head as text / head as raw bytes. |
| `{"cmd":"fs_create_dir" / "fs_create_file","dir_path"\|"file_path":…}` | create; refuses an existing path. |
| `{"cmd":"fs_copy_path","src":…,"dest":…}` / `{"cmd":"fs_duplicate","path":…}` | copy a file or tree / make the next free `… copy` sibling. |
| `{"cmd":"fs_rename_file","old_path":…,"new_path":…}` | rename / move. |
| `{"cmd":"fs_delete_file","file_path":…}` | delete a file, or a directory recursively. |
| `{"cmd":"fs_move_to_trash","file_path":…}` | recoverable delete via the OS trash. |
| `{"cmd":"fs_secure_delete","file_path":…}` | zero the bytes, fsync, then unlink. Refuses directories. |
| `{"cmd":"fs_touch","file_path":…}` | create if absent, then set atime + mtime to now. |
| `{"cmd":"fs_chmod","path":…,"mode_octal":…}` | set permission bits (Unix). |
| `{"cmd":"fs_symlink_retarget","path":…,"new_target":…}` | repoint an existing symlink. |
| `{"cmd":"fs_home_dir"}` | the home directory `~` expands to. |

`fs_git_status` shells out to `git` — the one op here that calls an external
program, because git already answers that question exactly and a second
implementation would be a second, drifting answer. Everything else is in-process.

**File watching** (streaming observers, keyed by `id`)

| Message | Reply / effect |
|---|---|
| `{"cmd":"fs_watch","id"?,"path":…,"recursive"?,"interval_ms"?}` | **stream** `{"ev":"fs","kind":"created\|modified\|removed","path":…}` on change. |
| `{"cmd":"fs_tail","id"?,"path":…,"from"?:"start"}` | **stream** `{"ev":"line","data":…}` as lines are appended (`tail -f`; survives rotation). |
| `{"cmd":"watch_stop","id"?}` / `{"cmd":"watch_list"}` | stop an observer / list active ones. |

**Exec & OS**

| Message | Reply / effect |
|---|---|
| `{"cmd":"exec","program":…,"args":[…],"cwd"?,"env"?,"stdin"?}` | run to completion → `{ok,code,stdout,stderr}` (base64). |
| `{"cmd":"open","target":…}` | open a path/URL with the OS default handler. |
| `{"cmd":"clipboard_get"}` / `{"cmd":"clipboard_set","text":…}` | read / write the clipboard. |
| `{"cmd":"notify","title":…,"body":…}` | desktop notification. |

**Background jobs** (long-running commands; run in the daemon, survive the connection)

| Message | Reply / effect |
|---|---|
| `{"cmd":"job_start","program":…,"args":[…],"label"?,"notify"?}` | spawn a background job → `{ok,job:<id>}` immediately; fires a desktop notification on completion (`notify`, default true). |
| `{"cmd":"job_list"}` | non-destructive status of every job → `[{id,label,running,code}]`. |
| `{"cmd":"job_result","id":N}` | fetch+remove one finished job → `{code,stdout,stderr}` (base64). |
| `{"cmd":"job_poll"}` | drain **all** finished jobs at once. |

**Process tools**

| Message | Reply / effect |
|---|---|
| `{"cmd":"ps","filter"?,"limit"?}` | processes by memory → `[{pid,name,mem,cpu}]`. |
| `{"cmd":"kill","pid":N,"signal"?}` | signal a process (`term` default, or `kill`). |
| `{"cmd":"which","program":…}` | resolve a program to its `$PATH` location → `{path}`. |

**Pub/sub event bus** (the host as a coordination hub across apps)

| Message | Reply / effect |
|---|---|
| `{"cmd":"sub","topic":…}` | subscribe this connection; thereafter receive `{"ev":"pub","topic":…,"data":…}` frames. |
| `{"cmd":"unsub","topic":…}` | stop receiving a topic. |
| `{"cmd":"pub","topic":…,"data":…}` | fan a message out to every subscriber → `{ok,delivered:N}`. |

The daemon itself publishes on `scheme` / `ui` whenever those change, so a
subscribed app (a HUD, an editor) gets **live theme sync** without polling.

**Transactional automation** (a chain of automation-bus calls that unwinds itself on failure)

| Message | Reply / effect |
|---|---|
| `{"cmd":"txn_begin","txn"?:N}` | open a transaction → `{ok,txn}`. While one is open, every reversible call made on the bus is journaled. |
| `{"cmd":"txn_commit","txn":N}` | close it, discarding the journal — nothing is compensated → `{ok,txn,steps}`. |
| `{"cmd":"txn_abort","txn":N}` | compensate every journaled step in **reverse** order, then close → `{ok,txn,steps,undo}`. Fires the `txn-aborted` hook event. |

Each verb declares a reversibility class, published as `rev` on the automation
surface: `inverse` (a compensation exists), `pure` (reads only — runs but is not
journaled), or `irreversible` (the default). Calling an `irreversible` verb while
a transaction is open is **refused at call time** with `verb not reversible: <id>`,
so a chain fails fast at the top instead of stranding itself half-undone at abort
time.

**Most of the surface is `irreversible`, deliberately.** The bus is scriptable
everywhere; it is transactional only where a compensation genuinely exists.

* **No host command is ever `inverse`.** The journal records a step's verb and
  args, never its pre-state, so there is nothing to restore from — `fs_write`,
  `kv_set`, `clipboard_set`, the `hooks_*` writers and `exec` are all
  irreversible however obvious their opposite looks on paper. A host verb can
  only be `pure`, and only when it neither writes, spawns, publishes, nor leaves
  an OS-visible artifact.
* **A `browser.*` verb is `inverse` only when the HUD journal can see it.** That
  journal captures pre-state by observing real `chrome.tabs` / `chrome.windows`
  events — created, removed, moved, detached, pinned, muted, url, activated,
  zoomed, window created — and replays a matching set of inverse ops. Verbs whose
  effects fall outside it (window state and bounds, tab groups, downloads,
  bookmarks, the reading list, browsing data, extension management) journal
  nothing, so classing one `inverse` would produce an abort that reports a clean
  revert having restored nothing.

Every verb that is deliberately left irreversible is listed with its reason in
`tests/rev_coverage.rs`, and the test there fails if a verb is added to the
surface without being classified or written down — so the table cannot quietly
fall behind the surface it describes.

Compensation for `browser.*` verbs is replayed by the HUD service worker, because
only it can read the live browser — a `browser.*` forward call is fire-and-forget
across the native port and its reply carries a delivery count, not a browser
result. The two halves are joined by one thing: the host stamps `_txn` and `_seq`
onto every journaled action it forwards, and the worker files that step's
pre-state under the same key. An abort then forwards a **single** `browser.undo`
frame carrying the whole reversed step list, so an N-step unwind is one
native-messaging round trip rather than N.

Every forwarded action also carries a unique, monotonic `_n`. The same action
reaches the worker over more than one transport (the `zbus.action` subscription
and the kv the `stryke_run` reply piggybacks), and the worker runs each `_n`
exactly once — so a chain is never dropped when only one transport is live, and
never doubled when both are.

**Suite bus client** (`src/suite.rs` — calling the OTHER apps on the GUI Automation Bus)

The automation bus above makes this host *reachable* as `App::open("zwire")`. These
four commands are the mirror leg: they dial **another** running app's socket
(`$XDG_RUNTIME_DIR/zgui/<app>.sock`, else `$TMPDIR/zgui`, else `/tmp/zgui`; the named
pipe `\\.\pipe\<app>.sock` on Windows) and speak the same NDJSON frames from the client
side. That is what lets the browser drive the rest of the suite — a page trigger, a ⌘K
step or a pane pipeline naming a verb in `zcite` / `zreq` / `zpdf` / … and getting its
return value back.

| Message | Reply / effect |
|---|---|
| `{"cmd":"suite_list"}` | `{ok,apps:[…],probed:N}` — the apps actually **running**, each proven by a dial. `probed` counts socket entries seen, so "nothing running" is distinguishable from "nothing installed". |
| `{"cmd":"suite_verbs","app":"zcite"}` | `{ok,result:{app,verbs,state,events}}` — that app's typed surface, including each verb's `rev` class where it publishes one. |
| `{"cmd":"suite_call","app":"zcite","verb":"item.add","args":{…}}` | `{ok,result:<value>}` — invoke a verb and return its value; `{ok:false,err}` if the app is not running or refuses. |
| `{"cmd":"suite_get","app":"zcite","state":"selection"}` | `{ok,result:<value>}` — read one of that app's state queries. |

A socket **file** is not a running app: the socket directory keeps entries from
processes that died without unlinking, so enumeration dials every candidate rather than
listing the directory. A bus name containing a path separator or `..` is refused before
any dial, because the name becomes a filename. One connection per exchange — the peer's
bridge journals a transaction against a *held* connection, so a shared long-lived
connection would silently enlist unrelated calls in whatever transaction a previous
caller left open.

`suite_list` / `suite_verbs` / `suite_get` are `pure`; **`suite_call` is
`irreversible`** and is refused inside an open transaction. The write lands in another
process with its own journal, and this host records a verb and its args, never the
peer's pre-state — so an "inverse" here would be a guess. Cross-app rollback is a real
thing with an existing owner: the suite's saga coordinator enlists each participant
under that participant's own transaction and fans `abort` back out, so every app
compensates through the inverse it declared. A chain that needs all-or-nothing across
apps asks for it **through** `suite_call` instead of having this host invent a second
coordinator.

**stryke hooks & scripting** (runs [`stryke`](https://github.com/MenkeTechnologies/strykelang) via a bundled sidecar — the browser never spawns it directly)

| Message | Reply / effect |
|---|---|
| `{"cmd":"hooks_events"}` | lifecycle-event catalog + action verbs → `{events:[…],actions:[…]}`. |
| `{"cmd":"hooks_save","hook":{name,event,enabled,timeout_ms?}}` | create/update a hook (scaffolds a starter `<id>.st`) → `{ok,hook}`. |
| `{"cmd":"hooks_list" / "hooks_delete" / "hooks_set_enabled" / "hooks_get_script" / "hooks_set_script" / "hooks_script_path",…}` | manage hooks + their stryke scripts. |
| `{"cmd":"hook_fire","event":…,"payload":{…}}` | run every enabled hook bound to `event`; each script's `{actions:[…]}` is dispatched (notify/open/exec/pub). |
| `{"cmd":"hooks_test_run","id":…,"sample":{…}}` | dry-run a hook (parses actions, does **not** dispatch). |
| `{"cmd":"stryke_run","code":"p 1+1","stdin"?}` | run inline stryke (`stryke -E`) → `{ok,stdout,stderr,code,timedOut}`. |
| `{"cmd":"stryke_lsp_start" / "stryke_lsp_send" / "stryke_lsp_stop",…}` | drive a per-connection `stryke --lsp` server; frames arrive as `{"ev":"stryke-lsp-rx","message":…}`. |

`stryke` is resolved via `ZWIRE_STRYKE` → the sibling next to this host (the
bundled sidecar) → `$PATH` → cargo/Homebrew, so an installed zwire needs no
system stryke.

**Host-to-host peering** (a mesh of daemons across machines)

Run daemons with TCP peering and the bus **federates across machines** — a
publish (or a `scheme`/`ui` change) on one host reaches subscribers on every
peer — and you can run a request on another host:

```sh
# machine A: listen for peers
zwire-host serve --tcp 0.0.0.0:7420 --token SECRET --name laptop
# machine B: listen, and dial A
zwire-host serve --tcp 0.0.0.0:7420 --token SECRET --name desktop --peer A.local:7420
```

| Message | Reply / effect |
|---|---|
| `{"cmd":"peers"}` | `{self, peers:[…]}` — connected peers. |
| `{"cmd":"peer_connect","addr":"host:port"}` | dial a new peer at runtime. |
| `{"cmd":"remote","peer":"host:port","request":{…}}` | run a request on another host → `{reply:…}`. |

Inbound TCP is gated by a shared `--token` (or `$ZWIRE_HOST_TOKEN`): a connection
must `auth` / `peer_hello` with it before anything privileged. Local Unix-socket
clients are trusted and never need it. Federation is single-hop (a forwarded
event is delivered locally but not re-forwarded), which covers star and
fully-meshed topologies without loops.

**PTY terminals** (multiplexed by `id`)

| Message | Reply / effect |
|---|---|
| `{"cmd":"pty_spawn","id"?,"rows":R,"cols":C,"shell"?,"args"?,"cwd"?,"env"?}` | spawn a shell; stream `{ev:"output","b64":…}` (and `pty:id` when keyed). |
| `{"cmd":"pty_write","id"?,"data"\|"b64":…}` | feed input. |
| `{"cmd":"pty_resize","id"?,"rows":R,"cols":C}` / `{"cmd":"pty_kill","id"?}` | resize / kill; kill emits `{ev:"exit"}`. |

**Legacy zwire scheme/ui** (unchanged): `{"cmd":"get"}` (replies with `version` +
`scheme` + `ui`), `{"scheme":"matrix"}`, `{"ui":{…}}` bridge `~/.zwire/hud-scheme`
+ `~/.zwire/hud-ui.json`.

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
Battery reporting is native on every platform: `pmset` on macOS,
`/sys/class/power_supply` on Linux, and `GetSystemPowerStatus` on Windows —
absent on machines with no battery (desktops, VMs), where the segment is omitted.

CI runs the four canonical polish gates on Ubuntu + macOS + Windows:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps                        # RUSTDOCFLAGS=-D warnings
cargo test
```

## [0x07] License

MIT © MenkeTechnologies
