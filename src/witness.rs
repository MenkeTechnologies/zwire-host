//! PREMISES: the facts a browser transaction was decided on, re-checked at commit.
//!
//! [`txn`](crate::txn) already gives a chain of browser-chrome mutations a commit/abort decision and
//! journaled inverses, and [`page`](crate::page) already lets a chain test what the browser rendered
//! *after* its steps ran (`page.assert`). Both look FORWARD. Neither closes the window that opens the
//! moment a chain reads the page at all:
//!
//! ```text
//!   t0  get page.tables          ← the chain reads "3 items in the cart"
//!   t1  browser.newTab · pin · move · zoom     ← it acts on that reading
//!   t2  txn_commit               ← …but at t1½ the user, a timer, a server push, or a second
//!                                   agent changed the cart. Nothing anywhere notices.
//! ```
//!
//! The read at `t0` is a PREMISE of everything after it, and between `t0` and `t2` the page is a
//! shared mutable object with other writers. A postcondition cannot cover this: it tests the page the
//! chain itself produced, not whether the page the chain *reasoned about* survived.
//!
//! So a chain DECLARES its premises — `page.witness` — and `txn_commit` re-checks every one of them
//! before it lets the chain's effects stand. A premise that no longer holds turns the commit into an
//! abort: the existing journal replays the inverses, and the browser ends up where it started.
//!
//! ## Declared, not inferred
//!
//! Read-set capture could be implicit (every `page.*` read inside the transaction becomes a premise),
//! and that is wrong here. A chain's OWN steps change the page — `browser.open`, `goBack`, `home`,
//! `reload` all navigate — so an inferred read set would conflict with itself on almost every real
//! chain and the feature would be noise. An explicit premise says something the author means: *this
//! reading is what the rest of the chain assumes; refuse the commit if it stopped being true.*
//!
//! ## Two ways a premise can be stated
//!
//! * **By content** (no `op`) — the projection must be byte-identical at commit. The strictest form,
//!   and the right one for "nothing about this table moved".
//! * **By predicate** (`op` + `value`, the same vocabulary [`page::ASSERT_OPS`](crate::page::ASSERT_OPS)
//!   uses) — the projection must still SATISFY the predicate. Survives an unrelated DOM tweak, which
//!   is what "the cart still has at least one item" actually means.
//!
//! A predicate premise is evaluated at declaration too, so a premise that is already false fails the
//! chain at the line that stated it rather than silently waiting to fail the commit.
//!
//! ## Why validation is ONE round trip
//!
//! Re-reading premises one at a time would make the validation itself non-atomic: the page can change
//! between validation reads, so a read set could "pass" in a state it was never simultaneously in.
//! [`revalidate`] therefore sends the whole premise set as a single `page.batch`, which the HUD worker
//! answers with one `chrome.scripting.executeScript` per target tab — a synchronous function body, so
//! every projection in it comes from one DOM turn. One injection, one IPC round trip, one snapshot,
//! regardless of how many premises there are.
//!
//! ## The safe direction
//!
//! A premise that cannot be RE-READ (browser closed, tab gone, origin denied) is not a premise that
//! held — it is one nobody could confirm. Those refuse the commit exactly like a violated one.
//! Committing on "we could not look" would make the guarantee worthless in precisely the situation it
//! exists for.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

/// One declared premise of a transaction.
#[derive(Debug, Clone)]
pub struct Premise {
    /// Monotonic id, unique across the process. Reported to the caller so a script can name the
    /// premise a violation refers to.
    pub id: u64,
    /// The projection this premise is about (`page.tables`, `page.extract`, …).
    pub state: String,
    /// The READ arguments it was projected with — targeting (`tabId`/`urls`) and, for
    /// `page.extract`, the selector. Never the predicate fields: those live beside it, so a premise
    /// re-reads exactly what it read the first time.
    pub args: Value,
    /// Content digest of the projected value at declaration time.
    pub digest: String,
    /// The predicate, when the premise was stated as one. `None` means "byte-identical".
    pub op: Option<String>,
    /// The predicate's operand.
    pub expected: String,
    /// Whether the predicate folds case.
    pub ignore_case: bool,
}

/// The premise ledger, keyed by transaction. Process-global for the same reason the journal is: a
/// chain may declare a premise on one connection and commit on another, and a premise filed
/// somewhere the commit cannot see is a guarantee that silently does nothing.
#[derive(Default)]
struct Ledger {
    open: Mutex<HashMap<u64, Vec<Premise>>>,
    next: AtomicU64,
}

fn ledger() -> &'static Ledger {
    static L: OnceLock<Ledger> = OnceLock::new();
    L.get_or_init(Ledger::default)
}

/// Content digest of a projected value: FNV-1a (64-bit) over its JSON.
///
/// Not a cryptographic hash and not trying to be — nothing here defends against a chosen-collision
/// attacker, because the only writer of the input is the browser the caller already trusts with the
/// page. What it must be is STABLE: the same projection must digest the same way at declaration and
/// at commit. `serde_json`'s default map is ordered, so a value's JSON does not depend on the order
/// its keys arrived in; `a_digest_does_not_depend_on_key_order` pins that, and turns a future
/// `preserve_order` feature into a test failure instead of a validation that reports drift on a page
/// that never changed.
pub fn digest(v: &Value) -> String {
    let bytes = serde_json::to_vec(v).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The argument keys that belong to the PREMISE rather than to the read. Stripped so a
/// re-projection at commit asks the identical question the declaration did — a `timeout_ms` or a
/// leftover `op` riding along would make the two read args differ and defeat the batch's dedup.
const PREMISE_KEYS: &[&str] = &[
    "state",
    "op",
    "value",
    "ignore_case",
    "txn",
    "timeout_ms",
    "_txn",
    "_seq",
];

/// The read arguments hiding inside a `page.witness` call: targeting and selector only.
pub fn read_args(args: &Value) -> Value {
    let mut o = serde_json::Map::new();
    if let Some(src) = args.as_object() {
        for (k, v) in src {
            if !PREMISE_KEYS.contains(&k.as_str()) {
                o.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(o)
}

/// File a premise against every transaction in `txns`. Returns its id.
pub fn record(txns: &[u64], mut premise: Premise) -> u64 {
    let l = ledger();
    let id = l.next.fetch_add(1, Ordering::Relaxed) + 1;
    premise.id = id;
    let mut open = l.open.lock().unwrap_or_else(|e| e.into_inner());
    for t in txns {
        open.entry(*t).or_default().push(premise.clone());
    }
    id
}

/// How many premises a transaction has declared.
pub fn count(txn: u64) -> usize {
    ledger()
        .open
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&txn)
        .map_or(0, Vec::len)
}

/// Drain a transaction's premises. Both `txn_commit` and `txn_abort` take them, so a transaction
/// that ended can never leave premises behind for a later one that reuses its id.
pub fn take(txn: u64) -> Vec<Premise> {
    ledger()
        .open
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&txn)
        .unwrap_or_default()
}

/// Clear every ledgered premise. Test seam only — the ledger is process-global.
pub fn reset() {
    ledger()
        .open
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/* ------------------------------------------------------------- validation */

/// Compare each premise against what the page shows NOW, and report the ones that no longer hold.
///
/// Pure: `observed[i]` is what the re-projection of `premises[i]` came back as. Keeping the compare
/// separate from the IPC is what lets every verdict — drift, a predicate that flipped, a page nobody
/// could read, an operand that stopped being countable — be tested without a browser.
///
/// An empty result means the commit may stand.
pub fn check(premises: &[Premise], observed: &[Result<Value, String>]) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, p) in premises.iter().enumerate() {
        let violated = |reason: &str, detail: String| {
            json!({
                "witness": p.id,
                "state": p.state,
                "op": p.op,
                "reason": reason,
                "err": detail,
            })
        };
        let value = match observed.get(i) {
            Some(Ok(v)) => v,
            // No answer at all — including a batch that came back short. "Could not confirm" is not
            // "still true"; see the module docs on the safe direction.
            Some(Err(e)) => {
                out.push(violated("unreadable", e.clone()));
                continue;
            }
            None => {
                out.push(violated("unreadable", "no answer for this premise".into()));
                continue;
            }
        };
        match &p.op {
            Some(op) => match crate::page::evaluate(op, value, &p.expected, p.ignore_case) {
                Ok(true) => {}
                Ok(false) => out.push(violated(
                    "predicate",
                    format!("premise no longer holds: {} {op} {:?}", p.state, p.expected),
                )),
                // The predicate was well-formed when it was declared, so reaching here means the
                // projection changed SHAPE under it (a list premise against something that is no
                // longer a list). That is a change, and it is not a passing premise.
                Err(e) => out.push(violated("predicate", e)),
            },
            None => {
                let now = digest(value);
                if now != p.digest {
                    out.push(violated(
                        "changed",
                        format!("{} changed: {} → {}", p.state, p.digest, now),
                    ));
                }
            }
        }
    }
    out
}

/// The batch to send for a premise set: the DISTINCT reads, and which read each premise reads from.
///
/// Distinct `(state, args)` pairs are asked for once each even when several premises share them, so
/// two predicates over the same table cost one projection rather than two — and, more importantly,
/// are judged against the SAME observation, which two separate reads could not guarantee.
pub fn plan(premises: &[Premise]) -> (Vec<Value>, Vec<usize>) {
    let mut reads: Vec<Value> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut slot: Vec<usize> = Vec::with_capacity(premises.len());
    for p in premises {
        let key = format!("{}\u{1}{}", p.state, p.args);
        match keys.iter().position(|k| *k == key) {
            Some(i) => slot.push(i),
            None => {
                keys.push(key);
                reads.push(json!({ "state": p.state, "args": p.args }));
                slot.push(reads.len() - 1);
            }
        }
    }
    (reads, slot)
}

/// Re-read every premise in ONE batch and check them.
pub fn revalidate(premises: &[Premise], timeout_ms: Option<u64>) -> Vec<Value> {
    if premises.is_empty() {
        return Vec::new();
    }
    let (reads, slot) = plan(premises);
    let answers = crate::page::batch(&reads, timeout_ms);
    let observed: Vec<Result<Value, String>> = slot
        .iter()
        .map(|i| match answers.get(*i) {
            Some(a) => a.clone(),
            None => Err("no answer for this premise".into()),
        })
        .collect();
    check(premises, &observed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn premise(state: &str, digest_of: &Value) -> Premise {
        Premise {
            id: 0,
            state: state.into(),
            args: json!({}),
            digest: digest(digest_of),
            op: None,
            expected: String::new(),
            ignore_case: false,
        }
    }

    #[test]
    fn a_digest_does_not_depend_on_key_order() {
        // The whole content-premise form rests on this. If the same projection could digest two ways
        // depending on how its keys were inserted, a commit would be refused on a page that never
        // changed — the worst possible failure for a gate whose job is to be believed.
        let a: Value = serde_json::from_str(r#"{"b":1,"a":[2,3],"c":{"z":1,"y":2}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"c":{"y":2,"z":1},"a":[2,3],"b":1}"#).unwrap();
        assert_eq!(digest(&a), digest(&b));
        // …and it is still sensitive to CONTENT, including list order, which is not a key order.
        assert_ne!(digest(&json!([1, 2])), digest(&json!([2, 1])));
        assert_ne!(digest(&json!("")), digest(&Value::Null));
    }

    #[test]
    fn a_content_premise_is_violated_by_any_change_and_by_none() {
        let rows = json!([{"caption": "Cart", "rows": [["Widget", "1"]]}]);
        let p = [premise("page.tables", &rows)];
        assert!(check(&p, &[Ok(rows.clone())]).is_empty());
        let moved = json!([{"caption": "Cart", "rows": [["Widget", "2"]]}]);
        let v = check(&p, &[Ok(moved)]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["reason"], json!("changed"));
        assert_eq!(v[0]["state"], json!("page.tables"));
    }

    #[test]
    fn a_predicate_premise_survives_an_unrelated_change_that_a_content_premise_would_not() {
        // This is the difference between the two forms, and the reason both exist. The list gained a
        // row: the digest moved, but "at least one entry" is still true.
        let before = json!([{"text": "Widget", "href": "/w"}]);
        let after = json!([{"text": "Widget", "href": "/w"}, {"text": "Gizmo", "href": "/g"}]);
        let mut p = premise("page.links", &before);
        assert_eq!(check(&[p.clone()], &[Ok(after.clone())]).len(), 1);
        p.op = Some("count_at_least".into());
        p.expected = "1".into();
        assert!(check(&[p.clone()], &[Ok(after)]).is_empty());
        // …and it does fail when the thing it names actually goes away.
        let v = check(&[p], &[Ok(json!([]))]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["reason"], json!("predicate"));
    }

    #[test]
    fn a_premise_nobody_could_re_read_is_a_violation_not_a_pass() {
        let p = [premise("page.text", &json!("checkout"))];
        let v = check(
            &p,
            &[Err("the browser is not attached to this host".into())],
        );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["reason"], json!("unreadable"));
        // A batch that came back SHORT is the same answer, not an accidental pass on a missing index.
        let v = check(&p, &[]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["reason"], json!("unreadable"));
    }

    #[test]
    fn a_predicate_whose_projection_changed_shape_is_a_violation_not_a_crash() {
        // `count_at_least` against something that is no longer a list. `page::evaluate` calls that
        // malformed, and at DECLARATION time it is — but by commit time it means the projection
        // stopped being the kind of thing the premise was about, which is a change.
        let mut p = premise("page.links", &json!([]));
        p.op = Some("count_at_least".into());
        p.expected = "1".into();
        let v = check(&[p], &[Ok(json!("a string now"))]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["reason"], json!("predicate"));
    }

    #[test]
    fn the_read_arguments_of_a_premise_exclude_the_premise_itself() {
        // The re-projection at commit must ask the identical question. Carrying `op`/`value` into the
        // read args would also break the batch's dedup: two predicates over one table would look
        // like two different reads and be compared against two different observations.
        let a = read_args(&json!({
            "state": "page.extract", "selector": "h2 a", "attr": "href",
            "tabId": 7, "op": "contains", "value": "x", "ignore_case": true,
            "txn": 3, "timeout_ms": 1000,
        }));
        assert_eq!(a, json!({ "selector": "h2 a", "attr": "href", "tabId": 7 }));
    }

    #[test]
    fn premises_are_filed_per_transaction_and_drained_once() {
        reset();
        let p = premise("page.title", &json!("Cart"));
        let id = record(&[41, 42], p);
        assert!(id > 0);
        assert_eq!(count(41), 1);
        assert_eq!(count(42), 1);
        assert_eq!(take(41).len(), 1);
        // Draining one transaction must not drain the other — interleaved chains are the case the
        // whole per-transaction ledger exists for.
        assert_eq!(count(41), 0);
        assert_eq!(count(42), 1);
        assert_eq!(take(42).len(), 1);
        assert!(take(42).is_empty());
        reset();
    }

    #[test]
    fn premises_sharing_one_read_are_asked_for_once_and_judged_together() {
        // Dedup is not only about bytes on the wire: two premises over the same projection must be
        // judged against ONE observation, or they can disagree about a page that changed between two
        // reads and the commit decision stops being a fact about a single moment.
        let rows = json!([["a"]]);
        let mut same_a = premise("page.tables", &rows);
        same_a.op = Some("nonempty".into());
        let mut same_b = premise("page.tables", &rows);
        same_b.op = Some("count_at_least".into());
        same_b.expected = "1".into();
        let mut other = premise("page.tables", &rows);
        other.args = json!({ "tabId": 9 });
        let set = [same_a, same_b, other];
        let (reads, slot) = plan(&set);
        assert_eq!(reads.len(), 2, "one read per distinct (state, args)");
        assert_eq!(
            slot,
            vec![0, 0, 1],
            "the shared premises read the same slot"
        );
        // Three premises, two reads: every premise still gets a verdict, and the two that share a
        // read share its answer rather than one of them silently going unjudged.
        let observed = [Ok(json!([["a"]])), Err("no tab matches this read".into())];
        let observed: Vec<_> = slot.iter().map(|i| observed[*i].clone()).collect();
        let v = check(&set, &observed);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["reason"], json!("unreadable"));
    }
}
