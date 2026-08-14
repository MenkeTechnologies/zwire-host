//! The PREMISE gate: `page.witness` + the commit-time re-check (`src/witness.rs`).
//!
//! `src/witness.rs`'s own unit tests cover the pure comparison — what counts as a violation, how a
//! premise set is deduped into a batch. What they cannot reach is the part that matters to a caller:
//! that a transaction whose premises stopped holding does not merely REPORT a conflict but is
//! actually unwound and closed, and that a transaction with no premises still takes the old path.
//! Those are properties of the `txn_commit` frame, so they are pinned here, at the wire.
//!
//! No browser is attached in a test process, so every re-read fails — which is exactly the
//! "unreadable" arm the module documents as a refusal. That makes the *safe direction* directly
//! testable here: a premise nobody could confirm must never commit.

use serde_json::{json, Value};
use std::io::Cursor;
use std::sync::{Mutex, MutexGuard, OnceLock};

use zwire_host::{page, txn, witness, zbus};

/// Transactions and premises are both process-global. Serialize the tests that open one, and start
/// each from a clean journal AND a clean ledger — a premise left behind by a neighbour would gate a
/// commit this test never asked to be gated.
fn guard() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    txn::reset();
    witness::reset();
    hermetic();
    g
}

/// Redirect host state into a throwaway directory: a journaled `browser.*` forward stamps the
/// file-backed KV, and no test may write to a real `~/.zwire`.
fn hermetic() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("zwh-premise-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ZWIRE_STATE", &dir);
        std::env::set_var("HOME", &dir);
        std::env::set_var("ZWIRE_BUS_NO_DAEMON", "1");
    });
}

/// Drive `serve_conn` with NDJSON request lines and collect the reply frames.
fn serve(lines: &[&str]) -> Vec<Value> {
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

/// A premise the browser will never be able to confirm, filed straight into the ledger.
///
/// Declaring one through `page.witness` needs a live browser to project the value; the ledger entry
/// is the same either way, so planting it is what lets the COMMIT half be tested at all.
fn plant(txn: u64, state: &str) -> u64 {
    witness::record(
        &[txn],
        witness::Premise {
            id: 0,
            state: state.into(),
            args: json!({}),
            digest: witness::digest(&json!("whatever the page said")),
            op: None,
            expected: String::new(),
            ignore_case: false,
        },
    )
}

#[test]
fn a_premise_with_no_transaction_to_gate_is_refused_rather_than_ignored() {
    let _g = guard();
    let r = page::request(
        page::WITNESS_VERB,
        &json!({ "state": "page.title", "op": "contains", "value": "Cart" }),
    );
    assert_eq!(r["ok"], json!(false));
    assert!(
        r["err"]
            .as_str()
            .unwrap_or_default()
            .contains("needs an open transaction"),
        "unexpected error: {r}"
    );
    // The refusal is the whole point: a premise that quietly did nothing would leave the author
    // believing the chain was gated when the commit is in fact unconditional.
    assert!(r.get("digest").is_none(), "nothing was ledgered");
}

#[test]
fn a_premise_naming_a_transaction_that_is_not_open_is_refused() {
    let _g = guard();
    zbus::txn_command("txn_begin", &json!({ "txn": 7100 }));
    let r = page::request(
        page::WITNESS_VERB,
        &json!({ "state": "page.title", "txn": 7101 }),
    );
    assert_eq!(r["ok"], json!(false));
    assert!(r["err"].as_str().unwrap_or_default().contains("not open"));
    assert_eq!(witness::count(7100), 0, "it filed against nobody");
    zbus::txn_command("txn_abort", &json!({ "txn": 7100 }));
}

#[test]
fn a_malformed_premise_is_refused_before_any_page_is_read() {
    let _g = guard();
    zbus::txn_command("txn_begin", &json!({ "txn": 7200 }));
    for (args, want) in [
        (
            json!({ "state": "hostinfo", "txn": 7200 }),
            "cannot assert on",
        ),
        (
            json!({ "state": "page.title", "op": "cnotains", "value": "x", "txn": 7200 }),
            "unknown assertion",
        ),
    ] {
        let r = page::request(page::WITNESS_VERB, &args);
        assert_eq!(r["ok"], json!(false), "for {args}");
        assert!(
            r["err"].as_str().unwrap_or_default().contains(want),
            "for {args}: {r}"
        );
    }
    assert_eq!(witness::count(7200), 0);
    zbus::txn_command("txn_abort", &json!({ "txn": 7200 }));
}

#[test]
fn a_commit_with_no_premises_is_the_pre_premise_path() {
    let _g = guard();
    let replies = serve(&[
        r#"{"t":"begin","id":1,"txn":7300}"#,
        r#"{"t":"commit","id":2,"txn":7300}"#,
    ]);
    let c = &replies[1]["value"];
    assert_eq!(c["ok"], json!(true));
    assert_eq!(c["committed"], json!(true));
    // No premises means no browser round trip and no new fields: a chain that never opted in must
    // pay nothing and see nothing new.
    assert!(c.get("conflict").is_none(), "{c}");
    assert!(c.get("validated").is_none(), "{c}");
    assert!(!txn::is_open(7300));
}

#[test]
fn a_commit_whose_premise_cannot_be_confirmed_is_refused_and_unwound() {
    let _g = guard();
    // One journaled step, then a premise nobody can re-read.
    let replies = serve(&[
        r#"{"t":"begin","id":1,"txn":7400}"#,
        r#"{"t":"call","id":2,"verb":"browser.pinTab","args":{},"txn":7400}"#,
    ]);
    assert_eq!(replies[1]["ok"], json!(true), "the step ran");
    plant(7400, "page.tables");

    let c = zbus::txn_command("txn_commit", &json!({ "txn": 7400, "timeout_ms": 50 }));
    assert_eq!(c["ok"], json!(false), "a refused commit is not a success");
    assert_eq!(c["committed"], json!(false));
    assert_eq!(c["conflict"], json!(true));
    assert_eq!(c["premises"], json!(1));
    assert_eq!(c["violations"][0]["reason"], json!("unreadable"));
    assert_eq!(c["violations"][0]["state"], json!("page.tables"));
    // …and the chain's effect is UNWOUND, not merely reported on. One journaled step went back.
    assert_eq!(c["aborted"], json!(true));
    assert_eq!(c["steps"], json!(1));
    assert_eq!(
        c["undo"]["ok"],
        json!(true),
        "the inverse was forwarded: {c}"
    );
    // The transaction is closed either way — a refused commit must not leave a chain half-open,
    // still journaling steps that nobody will ever compensate.
    assert!(!txn::is_open(7400));
    assert_eq!(witness::count(7400), 0);
}

#[test]
fn a_violated_premise_does_not_invent_steps_to_unwind() {
    let _g = guard();
    zbus::txn_command("txn_begin", &json!({ "txn": 7500 }));
    plant(7500, "page.text");
    let c = zbus::txn_command("txn_commit", &json!({ "txn": 7500, "timeout_ms": 50 }));
    assert_eq!(c["conflict"], json!(true));
    // Nothing was journaled, so nothing is compensated and no `browser.undo` is forwarded. A
    // conflict that reported a phantom unwind would be worse than no gate: the author would believe
    // a rollback happened.
    assert_eq!(c["steps"], json!(0));
    assert_eq!(c["undo"], Value::Null);
}

#[test]
fn an_abort_drops_the_premises_so_a_reused_transaction_id_starts_clean() {
    let _g = guard();
    zbus::txn_command("txn_begin", &json!({ "txn": 7600 }));
    plant(7600, "page.links");
    let a = zbus::txn_command("txn_abort", &json!({ "txn": 7600 }));
    assert_eq!(a["ok"], json!(true));
    assert_eq!(a["premises"], json!(1));
    assert_eq!(witness::count(7600), 0);

    // The same id, opened again: its commit must not inherit the previous chain's premises.
    zbus::txn_command("txn_begin", &json!({ "txn": 7600 }));
    let c = zbus::txn_command("txn_commit", &json!({ "txn": 7600, "timeout_ms": 50 }));
    assert_eq!(c["ok"], json!(true), "{c}");
    assert!(c.get("conflict").is_none(), "{c}");
}

#[test]
fn an_oversized_batch_is_refused_before_any_ipc() {
    let _g = guard();
    let reads: Vec<Value> = (0..=page::MAX_BATCH)
        .map(|_| json!({ "state": "page.title", "args": {} }))
        .collect();
    let started = std::time::Instant::now();
    let r = page::request(page::BATCH_VERB, &json!({ "reads": reads }));
    assert_eq!(r["ok"], json!(false));
    assert!(
        r["err"]
            .as_str()
            .unwrap_or_default()
            .contains("too many reads"),
        "{r}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "it went to the browser instead of refusing locally"
    );
}

#[test]
fn a_batch_that_fails_as_a_whole_fails_every_read_in_it() {
    let _g = guard();
    // No browser is attached, so the batch cannot be answered at all. Every read must come back as
    // its own failure: `witness::check` treats a missing answer as a violation, and collapsing the
    // whole batch into one error would leave the other premises silently unjudged.
    let reads = vec![
        json!({ "state": "page.title", "args": {} }),
        json!({ "state": "page.links", "args": {} }),
    ];
    let out = page::batch(&reads, Some(50));
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(Result::is_err), "{out:?}");
}

#[test]
fn the_premise_verbs_are_on_the_surface_and_class_pure() {
    let _g = guard();
    let s = zbus::surface();
    let verbs = s["verbs"].as_array().expect("verbs");
    for id in [page::WITNESS_VERB, page::BATCH_VERB] {
        let entry = verbs
            .iter()
            .find(|v| v["id"] == json!(id))
            .unwrap_or_else(|| panic!("{id} is not advertised"));
        // A premise declaration and a batch read change nothing in the browser, so both must stay
        // callable inside a transaction. Classing either `irreversible` would make the gate
        // impossible to use from the very chains it exists for.
        assert_eq!(entry["rev"], json!("pure"), "{id}");
    }
    let states = page::command("page_states", &json!({})).expect("page_states");
    assert_eq!(
        states["verbs"],
        json!(page::VERBS),
        "the catalogue and the surface list the same page verbs"
    );
}
