# zwire-host

The native-messaging host for [zwire](https://github.com/MenkeTechnologies/zwire) — a
single self-contained Rust binary (no Python, no psutil) that the browser's
`hud-internal` extension talks to over Chrome native messaging.

It does three jobs on one stdio pipe (length-prefixed JSON, little-endian u32 + body):

| Message in | Effect / reply |
|---|---|
| `{"cmd":"get"}` | reply `{"ok":true,"scheme":…,"ui":{…}}` — the shared color scheme + visual-effect prefs from `~/.zwire/`. |
| `{"scheme":"matrix"}` | write `~/.zwire/hud-scheme` (drives the compiled color mixer). |
| `{"ui":{…}}` | merge into `~/.zwire/hud-ui.json` (light/scanlines/glow/… shared with the new-tab). |
| `{"cmd":"sysinfo_start"}` | **stream** `{"sys":{…}}` every 2 s — cpu · mem · swap · disk · net rate · load · uptime · battery · temp · host · LAN/WAN ip. Backs the HUD statusbar. |
| `{"cmd":"pty_spawn","rows":R,"cols":C}` | spawn a login shell in a PTY and relay bytes: `{"cmd":"pty_write","data":…}` / `{"cmd":"pty_resize",…}` / `{"cmd":"pty_kill"}` in, `{"ev":"output","b64":…}` / `{"ev":"exit"}` out. Backs the embedded terminal. |

## Why Rust

The host ships **inside** `zwire.app` and must run with zero system dependencies.
`sysinfo` + `portable-pty` give the same data as psutil + a PTY as one ~500 KB
binary that also builds for Linux and Windows.

## Build

```sh
cargo build --release        # -> target/release/zwire-host (~500 KB)
cargo test                   # exercises the native-messaging protocol
```

Point a Chrome native-messaging host manifest's `path` at the binary and list the
allowed extension origins. `scripts/localinstall.sh` in the zwire repo does this
automatically when packaging the `.app`.

## License

MIT © MenkeTechnologies
