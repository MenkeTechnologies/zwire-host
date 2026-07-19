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
    let mut out: Vec<u8> = Vec::new();
    zbus::serve_conn(Cursor::new(input.into_bytes()), &mut out);
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
    let ok = reply["ok"].as_bool().unwrap_or_else(|| panic!("id {id} has no bool `ok`"));
    if ok {
        assert!(reply.get("value").is_some(), "id {id}: ok reply carries `value`");
        assert!(reply.get("error").is_none(), "id {id}: ok reply carries no `error`");
    } else {
        assert!(
            reply["error"].as_str().is_some(),
            "id {id}: failed reply carries a string `error`"
        );
        assert!(reply.get("value").is_none(), "id {id}: failed reply carries no `value`");
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
    assert_eq!(abort["steps"], json!(3), "all three inverse calls were journaled");

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
    assert_eq!(replies[2]["ok"], json!(true), "a `pure` verb still runs inside a transaction");
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
        let class = v["rev"].as_str().unwrap_or_else(|| panic!("{id} has no `rev`"));
        assert!(
            matches!(class, "inverse" | "pure" | "irreversible"),
            "{id} has an unknown rev class {class:?}"
        );
        assert_eq!(class, zbus::rev(id), "surface disagrees with rev() for {id}");
    }
    assert_eq!(zbus::rev("browser.nothing-like-this"), "irreversible");
}
