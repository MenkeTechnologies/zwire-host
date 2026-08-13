//! The rendered page as typed state: the rendezvous, the correlation, and the full socket loop.
//!
//! `page.rs` cannot answer anything on its own — the DOM is a process away — so almost everything
//! that can go wrong here is a COORDINATION failure: an answer delivered to the wrong waiter, a
//! query that never returns, a caller told "the page says no" when the truth is "nobody looked".
//! Those are exactly the failures that look fine in a screenshot and are invisible in a log, so they
//! are what this file pins.
//!
//! The browser is played by a fake service worker: a bus subscriber on the query topic that answers
//! with an ECHO of the query it received (`{echo: <state>, qid, args}`). Echoing rather than
//! returning a canned value is what makes the correlation assertions mean something — a reply
//! handed to the wrong caller is visibly the wrong `echo`.
//!
//! Every test takes [`serialize`] first. The bus broker, the pending-query table and the
//! page-server flag are all process-global (they are the same singletons in production), so tests
//! running in parallel would answer each other's queries and the failures would be timing-shaped.

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zwire_host::proto::Peer;
use zwire_host::{bus, page, suite, zbus};

/// One test at a time — see the module note. Poisoning is ignored on purpose: a panicking test has
/// already failed, and letting it poison the lock would turn one failure into a whole-file failure.
fn serialize() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/* ------------------------------------------------------------- fake worker */

/// A `Write` sink that parses the NDJSON frames the bus writes to a subscriber and forwards each
/// decoded frame to a channel — the test's stand-in for the HUD service worker's native port.
struct Sink {
    tx: Sender<Value>,
    buf: Vec<u8>,
}

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(b);
        while let Some(i) = self.buf.iter().position(|c| *c == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            if let Ok(v) = serde_json::from_slice::<Value>(&line[..line.len() - 1]) {
                let _ = self.tx.send(v);
            }
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A subscribed fake browser. Dropping it unsubscribes, so a worker never outlives its test.
struct Worker {
    id: u64,
    rx: Receiver<Value>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        bus::unregister(self.id);
    }
}

impl Worker {
    /// Subscribe to the query topic exactly the way `background.js` does over its persistent port.
    fn attach() -> Worker {
        let (tx, rx) = channel();
        let out = Peer::ndjson(Box::new(Sink {
            tx,
            buf: Vec::new(),
        }));
        let id = bus::register(&out);
        bus::subscribe(id, page::QUERY_TOPIC);
        Worker { id, rx }
    }

    /// Wait for the next published query frame.
    fn next_query(&self, within: Duration) -> Value {
        let frame = self
            .rx
            .recv_timeout(within)
            .expect("the browser never received the query");
        assert_eq!(frame["ev"], json!("pub"), "unexpected frame: {frame}");
        assert_eq!(frame["topic"], json!(page::QUERY_TOPIC));
        frame["data"].clone()
    }

    /// Answer a query the way the worker does: the projection it was asked for, echoed back under
    /// the same `qid`.
    fn answer(q: &Value) -> bool {
        let qid = q["qid"].as_u64().expect("every query carries a qid");
        page::fulfil(
            qid,
            Ok(json!({ "echo": q["state"], "qid": qid, "args": q["args"] })),
        )
    }
}

/// Claim this process as the browser-attached one, on a scratch socket directory.
///
/// `page::serve` flips a process-global flag, and cargo gives no test ORDER — so every test that
/// reaches `page::request` (rather than `page::ask` directly) claims the endpoint itself instead of
/// depending on whichever test happened to run first. Idempotent, and the scratch directory keeps
/// it off the developer's real bus, where their own browser may well be listening. The
/// not-yet-attached behaviour needs a process where this has NOT run, which is why it lives in its
/// own test binary (`tests/page_cold.rs`).
fn attached() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        // Short path: macOS caps a Unix socket path at 104 bytes.
        let dir = std::env::temp_dir().join(format!("zwp{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", &dir);
        let served = page::serve();
        assert_eq!(served["ok"], json!(true), "endpoint did not bind: {served}");
    });
}

/* ------------------------------------------------------------------ tests */

#[test]
fn a_query_reaches_the_browser_and_its_answer_comes_back() {
    let _g = serialize();
    let w = Worker::attach();
    let h = std::thread::spawn(move || {
        let q = w.next_query(Duration::from_secs(5));
        assert!(Worker::answer(&q), "nobody was waiting for the query");
        q
    });

    let v = page::ask(
        "page.tables",
        &json!({ "selector": "table.results" }),
        Duration::from_secs(5),
    )
    .expect("the browser answered");
    assert_eq!(v["echo"], json!("page.tables"));

    let q = h.join().unwrap();
    // The query frame is the contract with background.js: it must carry the correlation id, the
    // projection asked for, and the caller's args (a selector, a timeout, a url filter) untouched.
    assert!(q["qid"].as_u64().unwrap() > 0);
    assert_eq!(q["state"], json!("page.tables"));
    assert_eq!(q["args"]["selector"], json!("table.results"));
}

#[test]
fn two_queries_in_flight_are_answered_to_the_right_caller() {
    let _g = serialize();
    let w = Worker::attach();
    // Collect BOTH queries before answering either, then answer them newest-first. If correlation
    // were by arrival order rather than by qid, the two callers would swap projections — and each
    // would still get a perfectly well-formed reply, which is what makes this failure so quiet.
    let h = std::thread::spawn(move || {
        let a = w.next_query(Duration::from_secs(5));
        let b = w.next_query(Duration::from_secs(5));
        assert!(Worker::answer(&b));
        assert!(Worker::answer(&a));
    });

    let (tx, rx) = channel();
    let t2 = std::thread::spawn(move || {
        // Start second and give the first a head start, so "answered in reverse" is a real reversal.
        std::thread::sleep(Duration::from_millis(50));
        let v = page::ask("page.links", &json!({}), Duration::from_secs(5));
        let _ = tx.send(v);
    });
    let first = page::ask("page.tables", &json!({}), Duration::from_secs(5)).expect("first");
    let second = rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("second");

    assert_eq!(
        first["echo"],
        json!("page.tables"),
        "answers crossed callers"
    );
    assert_eq!(
        second["echo"],
        json!("page.links"),
        "answers crossed callers"
    );
    h.join().unwrap();
    t2.join().unwrap();
}

#[test]
fn a_browser_that_never_answers_times_out_instead_of_pinning_the_caller() {
    let _g = serialize();
    // Attached (so the fast "not attached" path is NOT what is being tested here) but mute.
    let w = Worker::attach();
    let started = Instant::now();
    let err = page::ask("page.text", &json!({}), Duration::from_millis(150)).unwrap_err();
    assert!(err.contains("did not answer"), "unexpected error: {err}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the deadline did not fire: {:?}",
        started.elapsed()
    );
    // The abandoned query must leave nothing behind: the browser answering after the deadline finds
    // no waiter, rather than filling a slot that would be handed to the next caller.
    let q = w.next_query(Duration::from_secs(2));
    assert!(!Worker::answer(&q), "a late answer was accepted");
}

#[test]
fn a_second_answer_to_the_same_query_is_dropped() {
    let _g = serialize();
    let w = Worker::attach();
    let h = std::thread::spawn(move || {
        let q = w.next_query(Duration::from_secs(5));
        // The worker is reachable over more than one transport; a doubly-delivered query would
        // otherwise leave a stale answer sitting in the table under a live qid.
        (Worker::answer(&q), Worker::answer(&q))
    });
    let v = page::ask("page.title", &json!({}), Duration::from_secs(5)).expect("answered");
    assert_eq!(v["echo"], json!("page.title"));
    let (first, second) = h.join().unwrap();
    assert!(first, "the first answer was refused");
    assert!(!second, "a duplicate answer was accepted");
}

#[test]
fn the_browser_reporting_a_failure_reaches_the_caller_as_that_failure() {
    let _g = serialize();
    let w = Worker::attach();
    let h = std::thread::spawn(move || {
        let q = w.next_query(Duration::from_secs(5));
        let qid = q["qid"].as_u64().unwrap();
        // What the worker sends when a tab refuses injection or the origin is denied. Collapsing it
        // into a timeout would tell the caller to retry something that will never work.
        page::command(
            "page_reply",
            &json!({ "qid": qid, "ok": false, "err": "origin denied by policy" }),
        )
    });
    let err = page::ask("page.text", &json!({}), Duration::from_secs(5)).unwrap_err();
    assert_eq!(err, "origin denied by policy");
    assert_eq!(h.join().unwrap().unwrap()["delivered"], json!(true));
}

/// THE DEADLOCK GUARD. In the attached process the query and its answer travel the SAME stdio
/// connection: the HUD sends `page_get` down its persistent native port, and the worker sends
/// `page_reply` back down that same port. `transport::stdio` reads it with one loop, so a
/// `Session::handle` that waits inline for the answer is waiting on a message only it could read.
/// The symptom is not a crash — it is the browser freezing for the whole timeout and then reporting
/// that it did not answer itself.
#[test]
fn a_page_read_answers_on_its_own_thread_instead_of_blocking_the_connection() {
    let _g = serialize();
    attached();
    let w = Worker::attach();
    let (tx, rx) = channel();
    let out = Peer::ndjson(Box::new(Sink {
        tx,
        buf: Vec::new(),
    }));
    let mut sess = zwire_host::Session::new();

    let started = Instant::now();
    sess.handle(
        &out,
        &json!({ "cmd": "page_get", "id": "r1", "state": "page.text", "args": { "timeout_ms": 4000 } }),
    );
    // Nothing has answered yet — the browser has not even been asked to look. If this call waited
    // for the value, the reply below could never arrive, because the thread that would deliver it is
    // this one.
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "handle() blocked for {:?}",
        started.elapsed()
    );

    let q = w.next_query(Duration::from_secs(5));
    assert!(Worker::answer(&q));
    let reply = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the deferred reply never arrived");
    // Deferred, but still correlated: the caller matches replies by `id`, so answering late is only
    // safe because the id rides along.
    assert_eq!(reply["id"], json!("r1"));
    assert_eq!(reply["ok"], json!(true));
    assert_eq!(reply["value"]["echo"], json!("page.text"));
}

#[test]
fn the_surface_advertises_every_projection_as_state_and_verb_and_classes_it_pure() {
    let _g = serialize();
    let s = zbus::surface();
    let states: Vec<&str> = s["state"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["id"].as_str())
        .collect();
    let verbs: Vec<&str> = s["verbs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v["id"].as_str())
        .collect();
    for (id, _) in page::STATES {
        // `get` names a state; only `call` carries args. A projection missing from either list is
        // reachable one way and invisible the other.
        assert!(states.contains(id), "{id} is not advertised as state");
        assert!(verbs.contains(id), "{id} is not advertised as a verb");
        assert_eq!(zbus::rev(id), "pure", "{id}");
    }
    for v in [page::EXTRACT_VERB, page::ASSERT_VERB] {
        assert!(verbs.contains(&v), "{v} is not advertised");
        assert_eq!(zbus::rev(v), "pure", "{v}");
    }
    // Reading a page is safe inside a transaction; the plumbing that WRITES the answer back is not
    // (see the ledger in rev_coverage.rs), and neither is claiming the endpoint.
    assert_eq!(zbus::rev("page_get"), "pure");
    assert_eq!(zbus::rev("page_states"), "pure");
    assert_eq!(zbus::rev("page_reply"), "irreversible");
    assert_eq!(zbus::rev("page_serve"), "irreversible");
}

/// The whole loop, over a real socket: another app dials the page endpoint with an ordinary bridge
/// frame, the endpoint publishes the query, the fake browser answers, and the value comes back down
/// the socket. This is the only test that proves the two halves are actually connected — the
/// rendezvous tests above would all still pass with the endpoint unbound.
#[test]
fn another_app_dials_the_page_endpoint_and_gets_the_live_projection() {
    let _g = serialize();
    attached();
    let w = Worker::attach();
    // Claiming an endpoint this process already holds must be a no-op, not a rebind: the HUD sends
    // `page_serve` on every worker reconnect, and re-binding would unlink the live socket out from
    // under whatever app is mid-query on it.
    assert_eq!(
        page::serve()["bound"],
        json!(false),
        "rebound a live endpoint"
    );

    let answers = std::thread::spawn(move || {
        for _ in 0..2 {
            let q = w.next_query(Duration::from_secs(5));
            assert!(Worker::answer(&q));
        }
    });

    // Exactly what another app runs: `App::open("zwire-page")->call("page.tables", {…})`. The
    // forwarding path in `page::request` is this same client call, so proving it here proves the
    // proxy a non-attached host process uses.
    let v = suite::call(
        page::PAGE_APP,
        "page.tables",
        json!({ "selector": "table" }),
    )
    .expect("the endpoint answered");
    assert_eq!(v["ok"], json!(true), "{v}");
    assert_eq!(v["value"]["echo"], json!("page.tables"));

    // …and the postcondition verb over the same socket, evaluated host-side against the live value.
    let a = suite::call(
        page::PAGE_APP,
        page::ASSERT_VERB,
        json!({ "state": "page.title", "op": "contains", "value": "page.title" }),
    )
    .expect("the endpoint answered");
    assert_eq!(a["ok"], json!(true), "{a}");
    assert_eq!(a["pass"], json!(true));

    answers.join().unwrap();
}

/// A failed assertion must be a FAILED STEP — `ok:false` — because that is what every chain
/// executor already treats as "stop and, inside a transaction, unwind". If a false postcondition
/// answered `ok:true` with a `pass:false` field, every existing chain would sail past it and commit.
#[test]
fn a_false_postcondition_is_a_failed_step_and_a_broken_one_is_not_a_verdict() {
    let _g = serialize();
    static SEEN: AtomicU64 = AtomicU64::new(0);
    let w = Worker::attach();
    let h = std::thread::spawn(move || {
        let q = w.next_query(Duration::from_secs(5));
        SEEN.fetch_add(1, Ordering::Relaxed);
        let qid = q["qid"].as_u64().unwrap();
        page::fulfil(qid, Ok(json!("Payment declined")));
    });
    // `page::request` needs this process to be the attached one; the loop test above bound the
    // endpoint, but test order is not guaranteed, so drive `ask` + `evaluate` the way `assert_here`
    // composes them and assert on the composition instead of on global state.
    let v = page::ask("page.text", &json!({}), Duration::from_secs(5)).expect("answered");
    assert_eq!(
        page::evaluate("contains", &v, "Order confirmed", false),
        Ok(false),
        "the page did not say what the postcondition claims"
    );
    h.join().unwrap();
    assert_eq!(
        SEEN.load(Ordering::Relaxed),
        1,
        "the page was read exactly once"
    );

    // A malformed assertion never reaches the browser at all: no query is published, and the reply
    // carries no `pass`, so a chain cannot mistake an authoring error for a page that came out wrong.
    let bad = page::request(
        page::ASSERT_VERB,
        &json!({ "state": "page.text", "op": "contians", "value": "x" }),
    );
    assert_eq!(bad["ok"], json!(false));
    assert!(
        bad.get("pass").is_none(),
        "a broken assertion returned a verdict"
    );
    assert!(bad["err"].as_str().unwrap().contains("unknown assertion"));
}
