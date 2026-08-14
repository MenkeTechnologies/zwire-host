//! The RENDERED PAGE as typed, dialable state on the suite bus.
//!
//! [`zbus`](crate::zbus) makes the browser's *actions* reachable (`App::open("zwire")->call("browser.newTab")`)
//! and [`suite`](crate::suite) lets the browser reach OUT to the other apps. Both move commands. This
//! module moves DATA the other way: it turns whatever the browser is rendering right now — after the
//! login, after the JavaScript, inside the session the user is actually in — into `state` any app on
//! the bus can read with the same three frames it uses for any other app:
//!
//! ```text
//!   → {"t":"verbs","id":1}                              ← …,"state":[{"id":"page.tables"},…]
//!   → {"t":"get","id":2,"state":"page.tables"}          ← the tables on the active tab, typed
//!   → {"t":"call","id":3,"verb":"page.extract","args":{"selector":"h2 a","attr":"href"}}
//! ```
//!
//! The page stops being a destination and becomes an INPUT: `zoffice` pulls the tables, `zreq` reads
//! the JSON an authenticated endpoint actually returned in the browser, a stryke one-liner pipes
//! `page.links` into anything. No consumer needs a browser client library, a debugger port, or a
//! JavaScript eval channel — it needs the bus it already speaks.
//!
//! ## Why this needs a rendezvous at all
//!
//! Nothing here can answer a query by itself: the DOM lives in the browser, one process away. The
//! host→browser direction has historically been FIRE AND FORGET (`zbus::run_command` publishes a
//! `browser.*` action and returns a delivery count, never a browser result), and that is exactly what
//! a `get` cannot be — a caller blocked on `{"t":"get"}` needs the value, not a receipt.
//!
//! So a query is correlated:
//!
//! ```text
//!   app ──get page.tables──▶ bus daemon ──[zwire-page.sock]──▶ browser-attached host
//!                                                                    │ publish zbus.query {qid}
//!                                                                    ▼
//!                                                             HUD service worker
//!                                                                    │ scripting.executeScript
//!                                                                    ▼
//!                                                             the live tab's DOM
//!   app ◀──── value ──────── bus daemon ◀───────────────────── {"cmd":"page_reply","qid",…}
//! ```
//!
//! [`ask`] allocates the `qid`, publishes, and blocks on a condvar until [`fulfil`] delivers that
//! exact `qid` or the deadline passes. Correlation is the whole point: several apps may be asking at
//! once, and a reply that arrived for someone else's question is worse than no reply.
//!
//! ## Why a SECOND endpoint
//!
//! Only ONE process is attached to the browser — the long-lived `connectNative` host whose port the
//! HUD service worker holds. That is almost never the process that owns the `zgui/zwire.sock` bus
//! (a dedicated `bus-daemon` singleton owns it; see [`crate::zbus::ensure_daemon`]). A publish in the
//! daemon therefore reaches no subscriber at all.
//!
//! So the browser-attached host binds its own endpoint, `zgui/zwire-page.sock`, speaking the SAME
//! bridge protocol, and any other host process simply FORWARDS a `page.*` request to it with the
//! existing bus client ([`crate::suite::call`]). One process publishes; every other process proxies;
//! nobody polls. If the browser is closed the socket has no listener, the dial fails immediately, and
//! the caller is told the browser is not attached instead of waiting out a timeout.
//!
//! ## What is deliberately NOT here
//!
//! No page WRITES. Filling a field, navigating, clicking — the browser already exposes those as
//! `browser.*` verbs with a compensation journal behind them, and adding a second, unjournaled write
//! path through here would be a way to mutate the browser that `txn_abort` cannot unwind. `page.*` is
//! read-only by construction, which is also why every verb in it classes `pure`.
//!
//! No page VALUES from form fields. [`STATES`] publishes a form's shape — action, method, field names
//! and types — and never what is typed into it. A password manager's autofill would otherwise leave
//! credentials readable by any process on the bus, which is not a trade a browser gets to make for
//! its user.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Bus topic the browser-attached host publishes queries on. The HUD service worker subscribes to it
/// over its persistent native port (`background.js` `startSysStream`).
pub const QUERY_TOPIC: &str = "zbus.query";

/// The bus name of the page endpoint — a SECOND socket, bound by whichever host process the browser
/// is attached to. Deliberately not "zwire": that name belongs to the command bus, and a caller
/// dialing it must keep reaching the daemon.
pub const PAGE_APP: &str = "zwire-page";

/// Default ceiling on one query. Generous enough for a cold tab that has to inject the projection
/// script, short enough that a wedged browser cannot pin the caller's connection.
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
/// Hard ceiling, whatever the caller asks for.
const MAX_TIMEOUT_MS: u64 = 30_000;

/// The typed projections of a rendered page, as advertised on `surface()["state"]`.
///
/// Each id is answered by the same projection code in the browser
/// (`zwire/extensions/hud-internal/zpage-core.js`, `project()`); the two catalogues are pinned
/// against each other by that repo's `tests/pagestate.mjs`, so an id added on one side and forgotten
/// on the other fails a build rather than returning `null` to a script.
pub const STATES: &[(&str, &str)] = &[
    ("page.url", "Address of the rendered page"),
    ("page.title", "Document title"),
    ("page.text", "Rendered text of the page body"),
    ("page.links", "Every link: text + resolved href"),
    ("page.headings", "Heading outline: level + text"),
    ("page.tables", "Every table as rows of cells"),
    (
        "page.forms",
        "Form shape: action, method, field names + types — never values",
    ),
    (
        "page.meta",
        "Document metadata: description, canonical, og:*, JSON-LD",
    ),
    ("page.selection", "The user's current selection"),
];

/// Escape hatch VERB. A state id takes no arguments; this takes a selector (and optionally an
/// attribute), so a caller can pull something the fixed projections do not name.
pub const EXTRACT_VERB: &str = "page.extract";

/// The POSTCONDITION verb: project a piece of the rendered page and test it, in one call.
///
/// This is the one that makes the namespace worth more than a reader. A failed assertion answers
/// `ok:false`, which is exactly what every chain executor in the HUD already treats as a failed
/// step — so inside `txn_begin` … `txn_commit`, a chain whose page did not come out right unwinds
/// itself through the existing journal instead of committing. The commit decision becomes a fact
/// about what the browser actually rendered, and the caller needed no new failure plumbing to get
/// there. `pass` and `assert` are carried alongside `err` so "the assertion is false" stays
/// distinguishable from "the page could not be read at all".
pub const ASSERT_VERB: &str = "page.assert";

/// The PREMISE verb: declare that a projection of the rendered page is a fact the rest of the
/// transaction depends on, so `txn_commit` refuses to let the chain stand if it stopped being true.
///
/// The mirror image of [`ASSERT_VERB`]. An assertion looks FORWARD — it tests the page the chain
/// itself produced. A premise looks BACK — it pins the page the chain *reasoned about*, which
/// anything else with a handle on the browser (the user, a timer, a server push, a second agent) can
/// change while the chain is mid-flight. See [`crate::witness`] for what a premise is checked
/// against and why the check is one round trip.
pub const WITNESS_VERB: &str = "page.witness";

/// Project SEVERAL views in ONE injection: `{"reads":[{"state","args"},…]}` → one result per read,
/// in order, each `{ok:true,value}` or `{ok:false,err}`.
///
/// This exists because a premise set has to be re-read ATOMICALLY. Asking N times lets the page move
/// between the answers, so a set could validate in a state it was never simultaneously in. One batch
/// is one `chrome.scripting.executeScript` per target tab, and a synchronous function body observes
/// a single DOM turn — so the whole read set comes from one moment. It is also, incidentally, the
/// cheapest way for any caller to pull several projections: one publish, one wait, one reply.
pub const BATCH_VERB: &str = "page.batch";

/// Is `id` a page state or one of the page verbs — i.e. should [`request`] handle it?
pub fn is_page_request(id: &str) -> bool {
    id == EXTRACT_VERB
        || id == ASSERT_VERB
        || id == WITNESS_VERB
        || id == BATCH_VERB
        || STATES.iter().any(|(s, _)| *s == id)
}

/// The page VERBS, in the order `page_states` and the automation surface advertise them.
pub const VERBS: &[&str] = &[EXTRACT_VERB, ASSERT_VERB, WITNESS_VERB, BATCH_VERB];

/// Ceiling on one batch. Bounds the work a single query can ask a renderer to do synchronously; the
/// browser half enforces the identical cap (`zpage-core.js` `MAX_BATCH`).
pub const MAX_BATCH: usize = 32;

/// The catalogue in `surface()` shape.
pub fn states() -> Vec<Value> {
    STATES
        .iter()
        .map(|(id, label)| json!({ "id": *id, "label": *label }))
        .collect()
}

/* ------------------------------------------------------------- rendezvous */

/// Pending queries: `qid` → the answer, once it lands. An entry exists from [`ask`] publishing until
/// it returns; [`fulfil`] fills it and wakes the waiter.
type Pending = HashMap<u64, Option<Result<Value, String>>>;

fn pending() -> &'static (Mutex<Pending>, Condvar) {
    static P: OnceLock<(Mutex<Pending>, Condvar)> = OnceLock::new();
    P.get_or_init(|| (Mutex::new(HashMap::new()), Condvar::new()))
}

/// Monotonic query id. Never reused within a process, so a late reply to an abandoned query is
/// dropped by [`fulfil`] rather than being handed to whoever asked next.
fn next_qid() -> u64 {
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed) + 1
}

/// Whether THIS process bound the page endpoint (i.e. the browser is attached here). Set by
/// [`serve`], and the switch [`request`] uses to decide between asking and forwarding.
fn serving() -> &'static AtomicBool {
    static S: AtomicBool = AtomicBool::new(false);
    &S
}

/// Ask the attached browser one query and block until it answers.
///
/// `Err` when no subscriber is listening (the HUD worker is not on this host's bus), when the
/// browser reports a failure, or when the deadline passes.
pub fn ask(state: &str, args: &Value, timeout: Duration) -> Result<Value, String> {
    let qid = next_qid();
    {
        pending().0.lock().unwrap().insert(qid, None);
    }
    let frame = json!({ "qid": qid, "state": state, "args": args });
    let delivered = crate::bus::publish(QUERY_TOPIC, &frame);
    if delivered == 0 {
        pending().0.lock().unwrap().remove(&qid);
        return Err("the browser is not attached to this host".into());
    }

    let (lock, cv) = pending();
    let mut map = lock.lock().unwrap();
    let deadline = Instant::now() + timeout;
    loop {
        match map.get(&qid) {
            Some(Some(_)) => break,
            // Someone removed our slot — only possible if a second waiter on this qid took it, which
            // `next_qid` makes impossible. Treat as a lost answer rather than looping forever.
            None => return Err("query cancelled".into()),
            Some(None) => {}
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            map.remove(&qid);
            return Err(format!(
                "the browser did not answer in {}ms",
                timeout.as_millis()
            ));
        }
        let (m, _) = cv.wait_timeout(map, left).unwrap();
        map = m;
    }
    match map.remove(&qid) {
        Some(Some(r)) => r,
        _ => Err("query cancelled".into()),
    }
}

/// Deliver the browser's answer to query `qid`. Returns whether anyone was still waiting for it —
/// `false` for a duplicate, a reply that lost the race with a timeout, or a fabricated id.
pub fn fulfil(qid: u64, result: Result<Value, String>) -> bool {
    let (lock, cv) = pending();
    let mut map = lock.lock().unwrap();
    match map.get_mut(&qid) {
        Some(slot) if slot.is_none() => {
            *slot = Some(result);
            cv.notify_all();
            true
        }
        _ => false,
    }
}

/* ------------------------------------------------------------ the endpoint */

#[cfg(unix)]
mod endpoint {
    use std::io::BufReader;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;

    /// Same directory the command bus and the bus client use: `$XDG_RUNTIME_DIR/zgui`, else
    /// `$TMPDIR/zgui`, else `/tmp/zgui`.
    fn socket_dir() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("TMPDIR").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("zgui")
    }

    fn socket_path() -> PathBuf {
        socket_dir().join(format!("{}.sock", super::PAGE_APP))
    }

    /// Bind the page endpoint and serve it forever on a background thread.
    ///
    /// A stale path from a host that died with the browser is cleared first: unlike the command bus
    /// there is no adopt-an-existing-owner case here, because ownership follows the browser's port —
    /// when the service worker reconnects, Chrome has already torn the previous host down.
    pub fn bind() -> Result<String, String> {
        let dir = socket_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        let sock = socket_path();
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).map_err(|e| format!("{}: {e}", sock.display()))?;
        // The endpoint answers with page content, so it is owner-only exactly like the command bus.
        let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => {
                        std::thread::spawn(move || handle(stream));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(sock.display().to_string())
    }

    fn handle(stream: UnixStream) {
        let Ok(rclone) = stream.try_clone() else {
            return;
        };
        crate::zbus::serve_conn(BufReader::new(rclone), stream);
    }
}

#[cfg(windows)]
mod endpoint {
    use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions, Stream};
    use std::io::BufReader;

    fn pipe_leaf() -> String {
        format!("{}.sock", super::PAGE_APP)
    }

    /// Bind the page endpoint as a named pipe and serve it forever on a background thread. The
    /// kernel refcounts the pipe object, so there is no stale path to clear — a second binder while
    /// the first is alive simply fails, which is the correct answer.
    pub fn bind() -> Result<String, String> {
        let leaf = pipe_leaf();
        let name = leaf
            .clone()
            .to_ns_name::<GenericNamespaced>()
            .map_err(|e| format!("bad pipe name {leaf}: {e}"))?;
        let listener = ListenerOptions::new()
            .name(name)
            .create_sync()
            .map_err(|e| format!(r"bind \\.\pipe\{leaf}: {e}"))?;
        std::thread::spawn(move || {
            while let Ok(stream) = listener.accept() {
                std::thread::spawn(move || handle(stream));
            }
        });
        Ok(format!(r"\\.\pipe\{leaf}"))
    }

    fn handle(stream: Stream) {
        let (recv, send) = stream.split();
        crate::zbus::serve_conn(BufReader::new(recv), send);
    }
}

#[cfg(not(any(unix, windows)))]
mod endpoint {
    pub fn bind() -> Result<String, String> {
        Err("no local IPC transport on this platform".into())
    }
}

/// Claim this process as the browser's page server: bind the endpoint so every other host process
/// can forward `page.*` here. Idempotent — the HUD sends it on every reconnect.
pub fn serve() -> Value {
    if serving().load(Ordering::Relaxed) {
        return json!({ "ok": true, "serving": true, "bound": false });
    }
    match endpoint::bind() {
        Ok(addr) => {
            serving().store(true, Ordering::Relaxed);
            json!({ "ok": true, "serving": true, "bound": true, "endpoint": addr })
        }
        Err(e) => json!({ "ok": false, "err": e }),
    }
}

/* ------------------------------------------------------------- predicates */

/// The predicates [`ASSERT_VERB`] can test, and what each means.
///
/// Deliberately NO regex. This crate has no regex dependency, and adding one to evaluate a pattern
/// the browser could evaluate natively would put a second, subtly different regex dialect on the
/// same page text — the triggers layer already matches JavaScript regexes against rendering text, so
/// that is where a pattern belongs. What is here is what a postcondition actually needs: is the
/// thing there, is it gone, is there enough of it.
pub const ASSERT_OPS: &[(&str, &str)] = &[
    ("contains", "the projection's text contains the value"),
    (
        "not_contains",
        "the projection's text does not contain the value",
    ),
    ("equals", "the projection's text is exactly the value"),
    ("empty", "the projection is empty (no text, no entries)"),
    ("nonempty", "the projection has any content at all"),
    (
        "count_at_least",
        "the projection is a list of at least N entries",
    ),
    (
        "count_at_most",
        "the projection is a list of at most N entries",
    ),
];

/// The text a predicate tests. A string projection is itself; anything structured (`page.links`,
/// `page.tables`) is compared as its JSON, so `contains` works uniformly over every projection
/// rather than being defined only for the two that happen to be strings.
fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// How many entries a projection has, for the counting predicates. `None` when the projection is
/// not a list — a count predicate against `page.title` is an authoring mistake, and answering
/// "0 entries, assertion failed" would hide it.
fn count_of(v: &Value) -> Option<usize> {
    match v {
        Value::Array(a) => Some(a.len()),
        Value::Object(o) => Some(o.len()),
        _ => None,
    }
}

/// Evaluate one predicate against a projected value.
///
/// `Err` is a malformed assertion (an unknown op, a count that is not a number, a count predicate
/// against something uncountable) — always distinct from `Ok(false)`, which is the page genuinely
/// failing the test. Collapsing the two would make a typo in an op name look like a page that came
/// out wrong, and a self-reverting chain would unwind for the wrong reason.
pub fn evaluate(
    op: &str,
    value: &Value,
    expected: &str,
    ignore_case: bool,
) -> Result<bool, String> {
    let fold = |s: String| {
        if ignore_case {
            s.to_lowercase()
        } else {
            s
        }
    };
    let text = fold(as_text(value));
    let want = fold(expected.to_string());
    match op {
        "contains" => Ok(text.contains(&want)),
        "not_contains" => Ok(!text.contains(&want)),
        "equals" => Ok(text.trim() == want.trim()),
        "empty" => Ok(count_of(value).map_or_else(|| text.trim().is_empty(), |n| n == 0)),
        "nonempty" => Ok(count_of(value).map_or_else(|| !text.trim().is_empty(), |n| n > 0)),
        "count_at_least" | "count_at_most" => {
            let n = count_of(value)
                .ok_or_else(|| format!("{op}: the projection is not a list of entries"))?;
            let want: usize = expected
                .trim()
                .parse()
                .map_err(|_| format!("{op}: `{expected}` is not a count"))?;
            Ok(if op == "count_at_least" {
                n >= want
            } else {
                n <= want
            })
        }
        _ => Err(format!("unknown assertion: {op}")),
    }
}

/* --------------------------------------------------------------- dispatch */

/// Answer one `page.*` request: ask the browser if it is attached HERE, otherwise forward to the
/// process that is.
///
/// Shaped like every other `run_command` reply (`{ok, …}`) because it is reached the same three
/// ways: the bridge `get` frame, the bridge `call` frame, and the plain `page_get` host command.
pub fn request(id: &str, args: &Value) -> Value {
    if !is_page_request(id) {
        return json!({ "ok": false, "err": format!("unknown page state: {id}") });
    }
    // A malformed assertion is rejected HERE, before any IPC, on whichever process the caller
    // reached. Validating it only in the attached process would send a typo across two sockets to
    // come back as a failure indistinguishable from a page that did not match.
    if id == ASSERT_VERB {
        if let Err(e) = check_assert(args) {
            return json!({ "ok": false, "assert": true, "err": e });
        }
    }
    // A premise is filed against the TRANSACTION, and the transaction journal lives in the process
    // the caller reached — never necessarily the one the browser is attached to. So `page.witness`
    // is handled HERE and only its underlying read is allowed to travel; forwarding the whole verb
    // would ledger the premise in a process whose `txn_commit` nobody will ever call.
    if id == WITNESS_VERB {
        return witness_here(args);
    }
    // Bounded here as well as in the browser: an oversized batch must be refused on the caller's
    // process rather than after two socket hops, and the host half of the cap is what
    // `crate::witness::plan` can rely on when it sizes a premise set.
    if id == BATCH_VERB {
        let n = args["reads"].as_array().map_or(0, Vec::len);
        if n > MAX_BATCH {
            return json!({ "ok": false, "err": format!("too many reads in one batch: {n} > {MAX_BATCH}") });
        }
    }
    let ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .min(MAX_TIMEOUT_MS);
    if serving().load(Ordering::Relaxed) {
        if id == ASSERT_VERB {
            return assert_here(args, Duration::from_millis(ms));
        }
        return match ask(id, args, Duration::from_millis(ms)) {
            Ok(v) => json!({ "ok": true, "state": id, "value": v }),
            Err(e) => json!({ "ok": false, "err": e }),
        };
    }
    // Not the attached process: proxy to whichever one is. A refused dial means the browser is not
    // running at all, which is a different answer from "it did not reply in time" — keep them apart.
    match crate::suite::call(PAGE_APP, id, args.clone()) {
        Ok(v) => v,
        Err(e) if e.contains("not running on the bus") => {
            json!({ "ok": false, "err": "the browser is not attached (no page endpoint)" })
        }
        Err(e) => json!({ "ok": false, "err": e }),
    }
}

/// Is this assertion well-formed — does it name a projection that exists and a predicate this host
/// knows? Both are authoring mistakes, and both must be caught before the page is ever read.
fn check_assert(args: &Value) -> Result<(), String> {
    let state = args["state"].as_str().unwrap_or("page.text");
    if !STATES.iter().any(|(s, _)| *s == state) && state != EXTRACT_VERB {
        return Err(format!("cannot assert on {state}"));
    }
    let op = args["op"].as_str().unwrap_or("contains");
    if !ASSERT_OPS.iter().any(|(id, _)| *id == op) {
        return Err(format!("unknown assertion: {op}"));
    }
    Ok(())
}

/* ---------------------------------------------------------------- premises */

/// Declare one premise of the open transaction: project it now, prove it, and ledger it for
/// [`crate::witness::revalidate`] to re-check at commit.
///
/// Three refusals, all of them before anything is ledgered:
///
/// * **No open transaction.** A premise with nothing to gate is an authoring bug — the chain would
///   run believing it was protected and commit unconditionally. Failing loudly is the only honest
///   answer; a silent no-op here is a guarantee that quietly does not exist.
/// * **A projection or predicate this host does not know.** Same reason [`check_assert`] runs before
///   any IPC: a typo must not come back looking like a page that changed.
/// * **A predicate that is ALREADY false.** The premise never held, so the chain should fail at the
///   line that stated it, not at a commit whose conflict report would blame concurrency for an
///   assumption that was wrong from the start.
fn witness_here(args: &Value) -> Value {
    let asked = args.get("txn").and_then(Value::as_u64);
    let txns: Vec<u64> = match asked {
        Some(t) if crate::txn::is_open(t) => vec![t],
        Some(t) => {
            return json!({ "ok": false, "witness": true, "err": format!("transaction {t} is not open") })
        }
        // Un-tagged, exactly like an un-tagged `call`: the premise belongs to every transaction the
        // caller is inside, because any of them may be the one that commits on this reading.
        None => crate::txn::open_ids(),
    };
    if txns.is_empty() {
        return json!({
            "ok": false,
            "witness": true,
            "err": "page.witness needs an open transaction — a premise with nothing to gate would be silently ignored",
        });
    }
    if let Err(e) = check_assert(args) {
        return json!({ "ok": false, "witness": true, "err": e });
    }
    let state = args["state"].as_str().unwrap_or("page.text").to_string();
    // Absent `op` is a CONTENT premise; `check_assert` defaults a missing op to `contains` for an
    // assertion, which would be the wrong default here — a premise nobody wrote a predicate for
    // means "unchanged", not "contains the empty string".
    let op = args.get("op").and_then(Value::as_str).map(str::to_string);
    let expected = args["value"].as_str().unwrap_or("").to_string();
    let ignore_case = args["ignore_case"] == json!(true);

    let mut read = crate::witness::read_args(args);
    if let (Some(o), Some(ms)) = (read.as_object_mut(), args.get("timeout_ms")) {
        // The timeout rides the read but is stripped from the LEDGERED args, so the premise's stored
        // read is byte-identical to the one the batch will replay.
        o.insert("timeout_ms".into(), ms.clone());
    }
    let reply = request(&state, &read);
    if reply["ok"] != json!(true) {
        return json!({
            "ok": false,
            "witness": true,
            "err": reply["err"].as_str().unwrap_or("the page could not be read").to_string(),
        });
    }
    let value = reply.get("value").cloned().unwrap_or(Value::Null);
    if let Some(op) = &op {
        match evaluate(op, &value, &expected, ignore_case) {
            Ok(true) => {}
            Ok(false) => {
                return json!({
                    "ok": false,
                    "witness": true,
                    "pass": false,
                    "state": state,
                    "op": op,
                    "err": format!("premise does not hold: {state} {op} {expected:?}"),
                })
            }
            Err(e) => return json!({ "ok": false, "witness": true, "err": e }),
        }
    }
    let digest = crate::witness::digest(&value);
    let id = crate::witness::record(
        &txns,
        crate::witness::Premise {
            id: 0,
            state: state.clone(),
            args: crate::witness::read_args(args),
            digest: digest.clone(),
            op: op.clone(),
            expected,
            ignore_case,
        },
    );
    json!({
        "ok": true,
        "witness": id,
        "state": state,
        "op": op,
        "digest": digest,
        "txns": txns,
    })
}

/// Re-read a set of projections in one batch. One entry per read, in order.
///
/// A batch that fails as a WHOLE (no browser, a refused dial) becomes one `Err` per read rather than
/// a single error, so [`crate::witness::check`] never has to distinguish "the batch broke" from "this
/// read broke" — both mean the same thing to a premise: nobody could confirm it.
pub fn batch(reads: &[Value], timeout_ms: Option<u64>) -> Vec<Result<Value, String>> {
    let mut args = json!({ "reads": reads });
    if let (Some(o), Some(ms)) = (args.as_object_mut(), timeout_ms) {
        o.insert("timeout_ms".into(), json!(ms));
    }
    let reply = request(BATCH_VERB, &args);
    let spread = |e: String| -> Vec<Result<Value, String>> {
        reads.iter().map(|_| Err(e.clone())).collect()
    };
    if reply["ok"] != json!(true) {
        return spread(
            reply["err"]
                .as_str()
                .unwrap_or("the page batch failed")
                .to_string(),
        );
    }
    let Some(list) = reply.get("value").and_then(Value::as_array) else {
        return spread("the browser did not answer with a batch".into());
    };
    reads
        .iter()
        .enumerate()
        .map(|(i, _)| match list.get(i) {
            Some(e) if e["ok"] == json!(true) => Ok(e.get("value").cloned().unwrap_or(Value::Null)),
            Some(e) => Err(e["err"].as_str().unwrap_or("this read failed").to_string()),
            None => Err("no answer for this read".into()),
        })
        .collect()
}

/// Project + test, in the process the browser is attached to.
///
/// The projection is fetched with the SAME [`ask`] every read uses, so an assertion can never be
/// evaluated against a cached or re-derived page: the value it tests is the one the tab was
/// rendering at the moment the chain asked.
fn assert_here(args: &Value, timeout: Duration) -> Value {
    let state = args["state"].as_str().unwrap_or("page.text");
    let op = args["op"].as_str().unwrap_or("contains");
    let expected = args["value"].as_str().unwrap_or("");
    let ignore_case = args["ignore_case"] == json!(true);
    let value = match ask(state, args, timeout) {
        Ok(v) => v,
        // The page could not be read at all. NOT an assertion result: `pass` is absent so a caller
        // never records "the page failed the test" when the truth is "nobody could look".
        Err(e) => return json!({ "ok": false, "assert": true, "err": e }),
    };
    match evaluate(op, &value, expected, ignore_case) {
        Ok(true) => json!({ "ok": true, "assert": true, "pass": true, "state": state, "op": op }),
        Ok(false) => json!({
            "ok": false,
            "assert": true,
            "pass": false,
            "state": state,
            "op": op,
            "err": format!("assertion failed: {state} {op} {expected:?}"),
        }),
        Err(e) => json!({ "ok": false, "assert": true, "err": e }),
    }
}

/// Dispatch a `page_*` host command. `None` when `cmd` is not one of ours, so the caller's match arm
/// stays one line and unknown commands keep falling through to the normal error.
pub fn command(cmd: &str, msg: &Value) -> Option<Value> {
    match cmd {
        "page_states" => Some(json!({
            "ok": true,
            "states": states(),
            "verbs": VERBS,
            "ops": ASSERT_OPS.iter().map(|(id, doc)| json!({ "id": *id, "doc": *doc })).collect::<Vec<_>>(),
            "serving": serving().load(Ordering::Relaxed),
        })),
        "page_get" => {
            let id = msg["state"].as_str().unwrap_or("");
            let args = msg.get("args").cloned().unwrap_or_else(|| json!({}));
            Some(request(id, &args))
        }
        // The browser answering a query it was published. `ok:false` carries the browser's own
        // reason (a chrome:// tab that refuses injection, a denied origin) through to the caller
        // instead of collapsing every failure into a timeout.
        "page_reply" => {
            let qid = msg["qid"].as_u64().unwrap_or(0);
            let result = if msg["ok"] == json!(false) {
                Err(msg["err"]
                    .as_str()
                    .unwrap_or("page query failed")
                    .to_string())
            } else {
                Ok(msg.get("value").cloned().unwrap_or(Value::Null))
            };
            Some(json!({ "ok": true, "delivered": fulfil(qid, result) }))
        }
        "page_serve" => Some(serve()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_with_no_browser_subscriber_fails_immediately_rather_than_waiting() {
        // Nothing is subscribed to the query topic in a unit-test process, so `publish` delivers to
        // zero sinks. Waiting out the timeout there would make every `get` on a closed browser cost
        // five seconds; the delivery count is the cheap, exact signal.
        let started = Instant::now();
        let err = ask("page.text", &json!({}), Duration::from_secs(30)).unwrap_err();
        assert!(err.contains("not attached"), "unexpected error: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "took {:?} — it waited instead of failing fast",
            started.elapsed()
        );
    }

    #[test]
    fn fulfilling_an_id_nobody_is_waiting_for_is_refused() {
        assert!(!fulfil(u64::MAX, Ok(json!("late"))));
    }

    #[test]
    fn every_state_id_is_namespaced_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for (id, label) in STATES {
            assert!(id.starts_with("page."), "{id} is not namespaced");
            assert!(!label.is_empty(), "{id} has no label");
            assert!(seen.insert(*id), "{id} is listed twice");
        }
        assert!(is_page_request(EXTRACT_VERB));
        assert!(!is_page_request("page.nope"));
    }

    #[test]
    fn a_malformed_assertion_is_never_reported_as_a_failing_page() {
        // The distinction the whole gate rests on: `Err` means the assertion itself is broken,
        // `Ok(false)` means the page came out wrong. A self-reverting chain unwinds on the second;
        // reporting the first as the second would unwind a chain because someone typo'd an op name.
        assert!(evaluate("cnotains", &json!("x"), "x", false).is_err());
        assert!(evaluate("count_at_least", &json!("not a list"), "1", false).is_err());
        assert!(evaluate("count_at_least", &json!([1, 2]), "many", false).is_err());
        assert_eq!(evaluate("contains", &json!("x"), "y", false), Ok(false));
    }

    #[test]
    fn predicates_read_structured_projections_as_well_as_text() {
        let links = json!([{"text": "Order confirmed", "href": "/receipt"}]);
        // `contains` over a list works because the value is compared as its JSON — otherwise every
        // predicate would have to be defined per projection, and most would simply not exist.
        assert_eq!(
            evaluate("contains", &links, "Order confirmed", false),
            Ok(true)
        );
        assert_eq!(evaluate("count_at_least", &links, "1", false), Ok(true));
        assert_eq!(evaluate("count_at_least", &links, "2", false), Ok(false));
        assert_eq!(evaluate("count_at_most", &links, "1", false), Ok(true));
        assert_eq!(evaluate("nonempty", &links, "", false), Ok(true));
        assert_eq!(evaluate("empty", &json!([]), "", false), Ok(true));
        // An EMPTY LIST is empty even though its JSON `[]` is not an empty string. Reading the count
        // rather than the text is what keeps "no results on the page" from testing as "nonempty".
        assert_eq!(evaluate("nonempty", &json!([]), "", false), Ok(false));
        assert_eq!(evaluate("empty", &json!("   "), "", false), Ok(true));
        assert_eq!(
            evaluate("equals", &json!(" Checkout "), "Checkout", false),
            Ok(true)
        );
        assert_eq!(
            evaluate("contains", &json!("CHECKOUT"), "checkout", true),
            Ok(true)
        );
        assert_eq!(
            evaluate("contains", &json!("CHECKOUT"), "checkout", false),
            Ok(false)
        );
        assert_eq!(
            evaluate("not_contains", &json!("all good"), "error", false),
            Ok(true)
        );
    }

    #[test]
    fn an_assertion_on_something_that_is_not_a_projection_is_refused() {
        let r = request(
            ASSERT_VERB,
            &json!({ "state": "hostinfo", "op": "contains", "value": "x" }),
        );
        assert_eq!(r["ok"], json!(false));
        assert!(r["err"].as_str().unwrap().contains("cannot assert on"));
        assert!(
            r.get("pass").is_none(),
            "a refused assertion must not report a verdict"
        );
    }

    #[test]
    fn an_unknown_page_state_is_refused_before_any_ipc() {
        let r = request("page.nope", &json!({}));
        assert_eq!(r["ok"], json!(false));
        assert!(r["err"].as_str().unwrap().contains("unknown page state"));
    }
}
