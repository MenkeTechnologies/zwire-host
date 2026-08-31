//! Wire conformance for the hand-mirrored zgui-bridge NDJSON protocol.
//!
//! `zwire-host` speaks the bus protocol NATIVELY (`src/zbus.rs:7-10`) instead of depending on the
//! `zgui-bridge` crate, because zwire is MIT and that crate is UNLICENSED. The cost is that every
//! frame added to `zgui-bridge/src/proto.rs` has to be re-implemented here by hand, and a forgotten
//! frame degrades SILENTLY: the client gets `"unknown request kind"` and the capability is simply
//! absent, with nothing failing in either repo.
//!
//! This file pins the shared half of that contract. [`CORPUS`] is the frame corpus both
//! implementations must answer; the assertions below are the reply-shape invariants both must
//! satisfy. The counterpart test lives in `zgui-bridge/tests/roundtrip.rs` and feeds the SAME
//! corpus to `zgui-bridge`'s `serve_conn` — it cannot run here, because linking the crate is the
//! exact thing the license split forbids. Adding a frame kind on either side without adding it to
//! this corpus is the drift this file exists to catch.

use serde_json::{json, Value};
use std::io::Cursor;
use std::sync::{Mutex, MutexGuard, OnceLock};

use zwire_host::{txn, zbus};

/// Every request kind both implementations must answer with a well-formed `reply` frame.
///
/// Kept as the frame TEXT rather than as constructed values so a shape change (a renamed field, a
/// field moved out of `args`) shows up as a diff in this corpus and not just in the code.
const CORPUS: &[&str] = &[
    r#"{"t":"verbs","id":1}"#,
    r#"{"t":"get","id":2,"state":"hostinfo"}"#,
    r#"{"t":"call","id":3,"verb":"ping","args":{}}"#,
    r#"{"t":"sub","id":4,"event":"scheme"}"#,
    r#"{"t":"begin","id":5,"txn":9001}"#,
    r#"{"t":"call","id":6,"verb":"browser.pinTab","args":{},"txn":9001}"#,
    r#"{"t":"commit","id":7,"txn":9001}"#,
    r#"{"t":"begin","id":8,"args":{"txn":9002}}"#,
    r#"{"t":"abort","id":9,"args":{"txn":9002}}"#,
    r#"{"t":"undo","id":10,"args":{"seq":1}}"#,
];

/// Transactions are process-global (one `seq` clock across every connection, so a cross-app abort
/// has a total order). Serialize the tests that open one, and start each from a clean journal.
fn txn_guard() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    txn::reset();
    g
}

/// Redirect all host state into a throwaway directory before anything touches it, so a `browser.*`
/// forward (which stamps the action into the file-backed KV) never writes to a real `~/.zwire`.
fn hermetic() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("zwh-wire-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZWIRE_STATE", &dir);
        std::env::set_var("HOME", &dir);
        // Never let a test reach for the developer's live bus daemon.
        std::env::set_var("ZWIRE_BUS_NO_DAEMON", "1");
    });
}

/// Drive `serve_conn` with a batch of NDJSON request lines and collect the reply frames.
fn serve(lines: &[&str]) -> Vec<Value> {
    hermetic();
    let input = format!("{}\n", lines.join("\n"));
    // A shared buffer, not a borrowed Vec: the connection owns its sink for as long as it lives.
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    zbus::serve_conn(Cursor::new(input.into_bytes()), zbus::Capture(buf.clone()));
    let out = buf.lock().unwrap().clone();
    String::from_utf8(out)
        .expect("replies are utf-8")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("every reply line is one JSON object"))
        .collect()
}

/// Reply-shape invariants shared by both implementations: one `reply` per request, ids preserved
/// and in order, `ok` decides whether `value` or `error` is present, never both.
fn assert_reply_shape(reply: &Value, id: u64) {
    assert_eq!(reply["t"], json!("reply"), "frame kind for id {id}");
    assert_eq!(reply["id"], json!(id), "reply id");
    let ok = reply["ok"]
        .as_bool()
        .unwrap_or_else(|| panic!("id {id} has no bool `ok`"));
    if ok {
        assert!(
            reply.get("value").is_some(),
            "id {id}: ok reply carries `value`"
        );
        assert!(
            reply.get("error").is_none(),
            "id {id}: ok reply carries no `error`"
        );
    } else {
        assert!(
            reply["error"].as_str().is_some(),
            "id {id}: failed reply carries a string `error`"
        );
        assert!(
            reply.get("value").is_none(),
            "id {id}: failed reply carries no `value`"
        );
    }
}

/// The drift catcher. Every corpus frame must be UNDERSTOOD — not merely answered. An unmirrored
/// frame still gets a reply (the fallthrough), so asserting "a reply came back" would pass against
/// the exact bug this test exists to find; the assertion is therefore on the fallthrough's error
/// text being absent.
#[test]
fn every_mirrored_frame_kind_is_understood() {
    let _g = txn_guard();
    let replies = serve(CORPUS);
    assert_eq!(replies.len(), CORPUS.len(), "one reply per request frame");
    for (i, reply) in replies.iter().enumerate() {
        let req: Value = serde_json::from_str(CORPUS[i]).unwrap();
        let id = req["id"].as_u64().unwrap();
        assert_reply_shape(reply, id);
        assert_ne!(
            reply["error"].as_str(),
            Some("unknown request kind"),
            "frame kind {:?} is in the corpus but not mirrored in zbus::serve_conn",
            req["t"]
        );
    }
}

/// The rollback path documented in the module header: a host that predates a frame answers with a
/// clean error instead of hanging. Removing the fallthrough would strand every such client.
#[test]
fn unmirrored_frame_kind_gets_a_clean_error() {
    let _g = txn_guard();
    let replies = serve(&[r#"{"t":"no-such-kind","id":77}"#]);
    assert_eq!(replies.len(), 1);
    assert_reply_shape(&replies[0], 77);
    assert_eq!(replies[0]["ok"], json!(false));
    assert_eq!(replies[0]["error"], json!("unknown request kind"));
}

/// Compensation order is the whole point of the feature and it fails SILENTLY when wrong, so assert
/// the exact sequence rather than the set: three journaled steps must come back newest-first.
#[test]
fn abort_unwinds_in_reverse_call_order() {
    let _g = txn_guard();
    let replies = serve(&[
        r#"{"t":"begin","id":1,"txn":4200}"#,
        r#"{"t":"call","id":2,"verb":"browser.pinTab","args":{"n":1},"txn":4200}"#,
        r#"{"t":"call","id":3,"verb":"browser.muteTab","args":{"n":2},"txn":4200}"#,
        r#"{"t":"call","id":4,"verb":"browser.moveTabFirst","args":{"n":3},"txn":4200}"#,
        r#"{"t":"abort","id":5,"txn":4200}"#,
    ]);
    let abort = &replies[4]["value"];
    assert_eq!(abort["ok"], json!(true));
    assert_eq!(
        abort["steps"],
        json!(3),
        "all three inverse calls were journaled"
    );

    assert_eq!(
        abort["undo"]["action"],
        json!("undo"),
        "abort forwards a single browser.undo action"
    );

    // A `browser.*` forward is fire-and-forget (it returns a delivery count, not a browser result),
    // so the ORDER is asserted on the payload it stamped for the HUD worker, not on the reply.
    let forwarded = zwire_host::store::kv_get("zwire", "__zbus_action");
    let verbs: Vec<String> = forwarded["steps"]
        .as_array()
        .expect("the forwarded undo carries its step list")
        .iter()
        .map(|s| s["verb"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        verbs,
        vec!["browser.moveTabFirst", "browser.muteTab", "browser.pinTab"],
        "steps compensate newest-first"
    );
}

/// An `irreversible` verb inside an open transaction is refused AT CALL TIME. If it were allowed to
/// run, the chain would only discover it at abort time — stranded half-undone, which is the failure
/// mode the reversibility classes exist to make impossible.
#[test]
fn irreversible_verb_is_refused_while_a_transaction_is_open() {
    let _g = txn_guard();
    let replies = serve(&[
        r#"{"t":"begin","id":1,"txn":4300}"#,
        r#"{"t":"call","id":2,"verb":"browser.clearHistory","args":{},"txn":4300}"#,
        r#"{"t":"call","id":3,"verb":"ping","args":{},"txn":4300}"#,
        r#"{"t":"abort","id":4,"txn":4300}"#,
    ]);
    assert_eq!(replies[1]["ok"], json!(false));
    assert_eq!(
        replies[1]["error"],
        json!("verb not reversible: browser.clearHistory")
    );
    assert_eq!(
        replies[2]["ok"],
        json!(true),
        "a `pure` verb still runs inside a transaction"
    );
    assert_eq!(
        replies[3]["value"]["steps"],
        json!(0),
        "neither the refused verb nor the pure one was journaled"
    );
}

/// The same verb outside a transaction is untouched by the class system — transactions gate
/// transactions, never ordinary automation.
#[test]
fn irreversible_verb_runs_normally_outside_a_transaction() {
    let _g = txn_guard();
    let replies = serve(&[r#"{"t":"call","id":1,"verb":"browser.clearHistory","args":{}}"#]);
    assert_eq!(replies[0]["ok"], json!(true));
    assert_eq!(replies[0]["value"]["action"], json!("clearHistory"));
}

/// A second abort of the same transaction must unwind NOTHING. `take_reversed` removes the
/// transaction before compensating precisely so a racing abort cannot double-compensate.
#[test]
fn double_abort_compensates_once() {
    let _g = txn_guard();
    let replies = serve(&[
        r#"{"t":"begin","id":1,"txn":4400}"#,
        r#"{"t":"call","id":2,"verb":"browser.pinTab","args":{},"txn":4400}"#,
        r#"{"t":"abort","id":3,"txn":4400}"#,
        r#"{"t":"abort","id":4,"txn":4400}"#,
    ]);
    assert_eq!(replies[2]["value"]["steps"], json!(1));
    assert_eq!(replies[3]["value"]["steps"], json!(0));
    assert_eq!(
        replies[3]["value"]["undo"],
        Value::Null,
        "nothing is forwarded when there is nothing to compensate"
    );
}

/// `commit` discards the journal without compensating, and closes the transaction so a later call
/// is no longer gated.
#[test]
fn commit_discards_without_compensating() {
    let _g = txn_guard();
    let replies = serve(&[
        r#"{"t":"begin","id":1,"txn":4500}"#,
        r#"{"t":"call","id":2,"verb":"browser.pinTab","args":{},"txn":4500}"#,
        r#"{"t":"commit","id":3,"txn":4500}"#,
        r#"{"t":"call","id":4,"verb":"browser.clearHistory","args":{}}"#,
    ]);
    assert_eq!(replies[2]["value"]["steps"], json!(1));
    assert_eq!(replies[2]["value"]["committed"], json!(true));
    assert_eq!(
        replies[3]["ok"],
        json!(true),
        "after commit the irreversible gate is off again"
    );
}

/// `surface()` must publish a class for EVERY advertised verb, and the default must be the strict
/// one — an un-classified verb silently defaulting to `inverse` would let an un-undoable step into
/// a journal.
#[test]
fn surface_publishes_a_reversibility_class_for_every_verb() {
    let _g = txn_guard();
    let replies = serve(&[r#"{"t":"verbs","id":1}"#]);
    let verbs = replies[0]["value"]["verbs"].as_array().expect("verb list");
    assert!(!verbs.is_empty());
    for v in verbs {
        let id = v["id"].as_str().expect("verb id");
        let class = v["rev"]
            .as_str()
            .unwrap_or_else(|| panic!("{id} has no `rev`"));
        assert!(
            matches!(class, "inverse" | "pure" | "irreversible"),
            "{id} has an unknown rev class {class:?}"
        );
        assert_eq!(
            class,
            zbus::rev(id),
            "surface disagrees with rev() for {id}"
        );
    }
    assert_eq!(zbus::rev("browser.nothing-like-this"), "irreversible");
}

/// The host and the HUD worker split transactional compensation down the middle: the host owns the
/// ORDER (the `seq` clock), the worker owns the PRE-STATE (only it can read the live browser). The
/// two halves are joined by exactly one thing — the `(txn, seq)` key stamped onto the forwarded
/// action. If the stamp is missing, the worker cannot tell a transacted action from an ordinary one,
/// journals nothing, and every `browser.undo` then finds an empty journal and compensates NOTHING
/// while still reporting `ok` — a silent, total loss of the compensation the REV table advertises.
#[test]
fn a_journaled_action_is_forwarded_with_its_journal_key() {
    let _g = txn_guard();

    // Outside a transaction there is no journal entry to key, so nothing is stamped and the
    // forwarded payload is byte-for-byte what it always was.
    serve(&[r#"{"t":"call","id":1,"verb":"browser.pinTab","args":{"n":1}}"#]);
    let plain = zwire_host::store::kv_get("zwire", "__zbus_action");
    assert_eq!(plain["a"], json!("pinTab"));
    assert!(
        plain.get("_seq").is_none(),
        "an un-transacted action is not journaled, so it must carry no journal key"
    );

    let replies = serve(&[
        r#"{"t":"begin","id":1,"txn":4300}"#,
        r#"{"t":"call","id":2,"verb":"browser.closeTab","args":{"n":1},"txn":4300}"#,
    ]);
    assert_eq!(replies[1]["ok"], json!(true));
    let stamped = zwire_host::store::kv_get("zwire", "__zbus_action");
    assert_eq!(stamped["a"], json!("closeTab"));
    assert_eq!(
        stamped["n"],
        json!(1),
        "the caller's own args still ride along"
    );
    assert_eq!(stamped["_txn"], json!(4300));
    let seq = stamped["_seq"]
        .as_u64()
        .expect("a journaled action carries the seq the worker files its pre-state under");

    // The abort's step list must name the SAME seq. These are the two ends of the key: the worker
    // stores under the forward stamp and looks up by the abort's `steps[].seq`, so a mismatch here
    // is a journal that can never be read back.
    let abort = serve(&[r#"{"t":"abort","id":3,"args":{"txn":4300}}"#]);
    assert_eq!(abort[0]["value"]["steps"], json!(1));
    let undo = zwire_host::store::kv_get("zwire", "__zbus_action");
    assert_eq!(undo["a"], json!("undo"));
    assert_eq!(
        undo["steps"][0]["seq"].as_u64(),
        Some(seq),
        "the abort unwinds the same seq the forward call was stamped with"
    );
}

/// Every forwarded action carries a unique stamp, because the worker uses it to execute an action
/// exactly once across the several transports that deliver it. Two identical actions issued inside
/// one millisecond must NOT collide: the worker would treat the second as an already-seen duplicate
/// and silently drop it, turning a 40-step chain into a 39-step one.
#[test]
fn each_forwarded_action_gets_a_distinct_nonce() {
    let _g = txn_guard();
    let mut seen = Vec::new();
    for _ in 0..8 {
        let replies = serve(&[r#"{"t":"call","id":1,"verb":"browser.newTab","args":{}}"#]);
        seen.push(
            replies[0]["value"]["nonce"]
                .as_u64()
                .expect("a forwarded action reports the nonce it stamped"),
        );
        let kv = zwire_host::store::kv_get("zwire", "__zbus_action");
        assert_eq!(
            kv["_n"].as_u64(),
            seen.last().copied(),
            "the kv path carries the same stamp as the reply"
        );
    }
    let mut sorted = seen.clone();
    sorted.dedup();
    assert_eq!(sorted.len(), seen.len(), "stamps collided: {seen:?}");
    assert!(
        seen.windows(2).all(|w| w[1] > w[0]),
        "stamps must rise monotonically: {seen:?}"
    );
}

/// A bus connection holds a REAL session, so a capability that answers on its own schedule reaches
/// the caller instead of dying with the frame that started it.
///
/// `sub` is the cheapest proof and the one that used to be a lie: the old dispatcher acknowledged a
/// subscription and registered nothing, because the session it registered on was a throwaway that
/// was dropped before the next line was read. Here the same connection subscribes, publishes, and
/// must see its own message come back as an `event` frame — the delivery count in the `pub` reply
/// says the subscriber existed, and the frame says it was actually written to this socket.
#[test]
fn a_subscription_on_a_bus_connection_receives_later_publishes() {
    // These verbs are irreversible, so a transaction left open by a test running in parallel would
    // have them refused at the gate. The guard both serialises against those tests and resets the
    // journal, exactly as every other txn-sensitive test here does.
    let _g = txn_guard();
    let frames = serve(&[
        r#"{"t":"sub","id":1,"topic":"demo"}"#,
        r#"{"t":"call","id":2,"verb":"pub","args":{"topic":"demo","data":{"hi":1}}}"#,
    ]);
    let published = frames
        .iter()
        .find(|f| f["id"] == json!(2))
        .expect("the pub is answered");
    assert_eq!(
        published["value"]["delivered"],
        json!(1),
        "the subscription registered a live subscriber: {published}"
    );
    let event = frames
        .iter()
        .find(|f| f["t"] == json!("event"))
        .expect("the published message arrives as an event frame");
    assert_eq!(event["value"]["ev"], json!("pub"), "event shape: {event}");
    assert_eq!(event["value"]["topic"], json!("demo"));
    assert_eq!(event["value"]["data"], json!({"hi": 1}));
    // An event is not a reply: it correlates to nothing, and a client that matched it to a pending
    // id would resolve the wrong call.
    assert!(event.get("id").is_none(), "events carry no id: {event}");
}

/// An observer started on a bus connection BELONGS to it — the proof that the connection owns a
/// real session rather than a throwaway per frame.
///
/// `fs_watch` registers its watcher on the session; `watch_list` reads that same map back. Under the
/// old dispatcher the watcher was registered on a session that was dropped before the next line was
/// parsed, so this listing was always empty and the stream had nowhere to go.
#[test]
fn an_observer_started_on_the_bus_belongs_to_that_connection() {
    // These verbs are irreversible, so a transaction left open by a test running in parallel would
    // have them refused at the gate. The guard both serialises against those tests and resets the
    // journal, exactly as every other txn-sensitive test here does.
    let _g = txn_guard();
    let dir = std::env::temp_dir().join(format!("zwh-watch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.display().to_string();
    let frames = serve(&[
        &format!(r#"{{"t":"call","id":1,"verb":"fs_watch","args":{{"id":"w1","path":"{path}"}}}}"#),
        r#"{"t":"call","id":2,"verb":"watch_list","args":{}}"#,
        r#"{"t":"call","id":3,"verb":"watch_stop","args":{"id":"w1"}}"#,
        r#"{"t":"call","id":4,"verb":"watch_list","args":{}}"#,
    ]);
    // The watcher named itself `w1`, so its own reply comes back under that key — the same
    // correlation rule the NDJSON daemon uses.
    assert!(
        frames.iter().any(|f| f["id"] == json!("w1")),
        "the start is answered under the key it named: {frames:?}"
    );
    let listed = frames.iter().find(|f| f["id"] == json!(2)).unwrap();
    assert_eq!(
        listed["value"]["watchers"],
        json!(["w1"]),
        "the watcher outlived the frame that started it: {listed}"
    );
    let after = frames.iter().find(|f| f["id"] == json!(4)).unwrap();
    assert_eq!(
        after["value"]["watchers"],
        json!([]),
        "and stopping it on the same session removed it: {after}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
