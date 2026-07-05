//! The three ways to reach the dispatcher.
//!
//!   * [`stdio`]  — Chrome native messaging on `stdin`/`stdout` (the default when
//!     the browser launches the binary).
//!   * [`serve`]  — a local-socket daemon speaking NDJSON, so tmux, emacs,
//!     desktop apps, plugins, and any language can connect and use every
//!     capability. Each connection gets its own [`Session`]. Backed by a Unix
//!     domain socket on macOS/Linux and a named pipe on Windows.
//!   * [`call`]   — a one-line client: connect, send one request, print the
//!     reply. Lets a shell script or editor talk to the daemon trivially.
use crate::proto::{read_native, Peer};
use crate::session::Session;
use std::io::{self, BufRead, Write};
use std::path::Path;

/// Chrome native-messaging loop: read `u32`-framed requests from `stdin`, drive
/// one [`Session`], until EOF (browser closed the port).
pub fn stdio() {
    let out = Peer::native(Box::new(io::stdout()));
    let mut sess = Session::new();
    let mut stdin = io::stdin();
    while let Some(msg) = read_native(&mut stdin) {
        sess.handle(&out, &msg);
    }
}

/// Emit a fatal error to `stderr` and exit non-zero.
fn die(msg: &str) -> ! {
    let _ = writeln!(io::stderr(), "zwire-host: {msg}");
    std::process::exit(1);
}

/// Drive one accepted NDJSON connection to completion. Shared by every platform:
/// the OS-specific code only has to hand over a reader and a writer half.
#[cfg(any(unix, windows))]
fn serve_conn(reader: impl BufRead, writer: Box<dyn Write + Send>) {
    let out = Peer::ndjson(writer);
    let mut sess = Session::new();
    let mut reader = reader;
    while let Some(msg) = crate::proto::read_ndjson(&mut reader) {
        sess.handle(&out, &msg);
    }
    // Connection closed: `sess` drops here, tearing down its PTYs/streams.
}

/// Client half: write the request, then relay reply frames to `stdout`. One
/// frame by default; every frame until EOF when `follow` is set (streams).
#[cfg(any(unix, windows))]
fn run_client(reader: impl BufRead, mut writer: impl Write, request: &str, follow: bool) -> ! {
    let line = format!("{}\n", request.trim());
    if let Err(e) = writer
        .write_all(line.as_bytes())
        .and_then(|()| writer.flush())
    {
        die(&format!("send: {e}"));
    }
    let mut reader = reader;
    let mut stdout = io::stdout();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if stdout
                    .write_all(line.as_bytes())
                    .and_then(|()| stdout.flush())
                    .is_err()
                {
                    break;
                }
                if !follow {
                    break; // one reply is all an RPC produces
                }
            }
        }
    }
    std::process::exit(0);
}

/* ---- Unix domain socket (macOS / Linux) ---- */
#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    /// Run the NDJSON daemon on `sock`, one thread per connection, forever.
    pub fn serve(sock: &Path) -> ! {
        if let Some(parent) = sock.parent() {
            let _ = std::fs::create_dir_all(parent);
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        // A stale socket file from a previous run blocks bind(); clear it.
        let _ = std::fs::remove_file(sock);
        let listener = match UnixListener::bind(sock) {
            Ok(l) => l,
            Err(e) => die(&format!("bind {}: {e}", sock.display())),
        };
        // Owner-only: the socket exposes exec/fs/pty, so no other local user may
        // connect.
        let _ = std::fs::set_permissions(sock, std::fs::Permissions::from_mode(0o600));
        let _ = writeln!(io::stderr(), "zwire-host: listening on {}", sock.display());

        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    std::thread::spawn(move || handle(stream));
                }
                Err(e) => {
                    let _ = writeln!(io::stderr(), "zwire-host: accept: {e}");
                }
            }
        }
        std::process::exit(0);
    }

    fn handle(stream: UnixStream) {
        let Ok(rclone) = stream.try_clone() else {
            return;
        };
        serve_conn(io::BufReader::new(rclone), Box::new(stream));
    }

    /// Connect to the daemon and run one request/reply exchange.
    pub fn call(sock: &Path, request: &str, follow: bool) -> ! {
        let stream = match UnixStream::connect(sock) {
            Ok(s) => s,
            Err(e) => die(&format!(
                "connect {}: {e} (is the daemon running? `zwire-host serve`)",
                sock.display()
            )),
        };
        let rclone = match stream.try_clone() {
            Ok(r) => r,
            Err(e) => die(&format!("clone: {e}")),
        };
        run_client(io::BufReader::new(rclone), stream, request, follow);
    }
}

/* ---- named pipe (Windows) ---- */
#[cfg(windows)]
mod platform {
    use super::*;
    use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions, Stream};

    /// Derive the pipe's namespaced name from the endpoint's leaf token. On
    /// Windows this maps to `\\.\pipe\<name>`.
    fn pipe_name(sock: &Path) -> String {
        sock.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "zwire-host".to_string())
    }

    pub fn serve(sock: &Path) -> ! {
        let raw = pipe_name(sock);
        let name = match raw.clone().to_ns_name::<GenericNamespaced>() {
            Ok(n) => n,
            Err(e) => die(&format!("bad pipe name {raw}: {e}")),
        };
        let listener = match ListenerOptions::new().name(name).create_sync() {
            Ok(l) => l,
            Err(e) => die(&format!(r"bind \\.\pipe\{raw}: {e}")),
        };
        let _ = writeln!(io::stderr(), r"zwire-host: listening on \\.\pipe\{raw}");

        loop {
            match listener.accept() {
                Ok(stream) => {
                    std::thread::spawn(move || handle(stream));
                }
                Err(e) => {
                    let _ = writeln!(io::stderr(), "zwire-host: accept: {e}");
                }
            }
        }
    }

    fn handle(stream: Stream) {
        let (recv, send) = stream.split();
        serve_conn(io::BufReader::new(recv), Box::new(send));
    }

    pub fn call(sock: &Path, request: &str, follow: bool) -> ! {
        let raw = pipe_name(sock);
        let name = match raw.clone().to_ns_name::<GenericNamespaced>() {
            Ok(n) => n,
            Err(e) => die(&format!("bad pipe name {raw}: {e}")),
        };
        let stream = match Stream::connect(name) {
            Ok(s) => s,
            Err(e) => die(&format!(
                r"connect \\.\pipe\{raw}: {e} (is the daemon running? `zwire-host serve`)"
            )),
        };
        let (recv, send) = stream.split();
        run_client(io::BufReader::new(recv), send, request, follow);
    }
}

#[cfg(any(unix, windows))]
pub use platform::{call, serve};

/* ---- platforms with neither Unix sockets nor named pipes ---- */
#[cfg(not(any(unix, windows)))]
/// Socket transport is unavailable here; the native-messaging stdio transport
/// still works.
pub fn serve(_sock: &Path) -> ! {
    die("`serve` is not supported on this platform");
}

#[cfg(not(any(unix, windows)))]
/// See [`serve`].
pub fn call(_sock: &Path, _request: &str, _follow: bool) -> ! {
    die("`call` is not supported on this platform");
}
