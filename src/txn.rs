//! Transactional compensation for the GUI Automation Bus.
//!
//! A stryke script wraps a chain of bus calls in `App::txn { … }`. Every call made while the
//! transaction is open is JOURNALED here in execution order; a failure aborts the transaction and
//! every journaled step is compensated in REVERSE order.
//!
//! What this module does and does NOT own:
//!
//! * It owns the ORDER (one monotonic `seq` clock across all open transactions, so a cross-app
//!   abort has a total order to unwind by) and the OPEN/CLOSED state of each transaction.
//! * It does NOT own the pre-state of a `browser.*` verb. Those verbs are fire-and-forget across
//!   the native port (see [`crate::zbus`]) — the forward reply carries a delivery count, not a
//!   browser result, so there is no undo handle to journal here. The browser-side journal lives in
//!   the extension service worker, which captures the pre-state at execution time and keys it by
//!   the same `(txn, seq)` pair this module hands out. An abort therefore forwards ONE
//!   `browser.undo` carrying the reversed step list; the worker replays the inverses.
//!
//! Reversibility classes gate what may enter a journal at all — see [`crate::zbus::rev`]. An
//! `irreversible` verb is rejected AT CALL TIME while a transaction is open, so a chain can never
//! be stranded half-undone at abort time. A `pure` verb runs but is not journaled: it changed
//! nothing, so compensating it would be a no-op at best.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

/// One journaled forward step, in execution order.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JournalEntry {
    /// Position in the global monotonic clock — the total order an abort unwinds by.
    pub seq: u64,
    /// The transaction this step belongs to.
    pub txn: u64,
    /// The bus verb that ran.
    pub verb: String,
    /// The arguments it ran with.
    pub args: Value,
}

/// Append-only journal of the open transactions. One monotonic `seq` clock is shared by ALL of
/// them so interleaved steps from different connections still unwind in a defined global order.
#[derive(Default)]
struct Journal {
    open: Mutex<HashMap<u64, Vec<JournalEntry>>>,
    seq: AtomicU64,
    next_txn: AtomicU64,
}

fn journal() -> &'static Journal {
    static J: OnceLock<Journal> = OnceLock::new();
    J.get_or_init(Journal::default)
}

/// Is ANY transaction currently open? Cheap gate: when false, `call` dispatch takes exactly the
/// path it took before transactions existed — no journaling, no reversibility check.
pub fn any_open() -> bool {
    !journal()
        .open
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
}

/// Is this specific transaction open?
pub fn is_open(txn: u64) -> bool {
    journal()
        .open
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&txn)
}

/// Open a transaction. `{"txn":N}` reuses a caller-chosen id (rejected if already open); otherwise
/// one is allocated. Reply: `{ok, txn}`.
pub fn begin(args: &Value) -> Value {
    let j = journal();
    let mut open = j.open.lock().unwrap_or_else(|e| e.into_inner());
    let txn = match args.get("txn").and_then(Value::as_u64) {
        Some(t) => {
            if open.contains_key(&t) {
                return json!({ "ok": false, "err": "txn already open", "txn": t });
            }
            j.next_txn.fetch_max(t + 1, Ordering::Relaxed);
            t
        }
        None => j.next_txn.fetch_add(1, Ordering::Relaxed) + 1,
    };
    open.insert(txn, Vec::new());
    json!({ "ok": true, "txn": txn })
}

/// Record a forward step against every open transaction the caller is inside. Returns the entry's
/// `seq`, or `None` when nothing was journaled (no open transaction).
///
/// `txn` selects one transaction; `None` records against every open one, which is what an
/// un-tagged `call` frame from a client that only sent `begin` does.
pub fn record(txn: Option<u64>, verb: &str, args: &Value) -> Option<u64> {
    let j = journal();
    let mut open = j.open.lock().unwrap_or_else(|e| e.into_inner());
    if open.is_empty() {
        return None;
    }
    let seq = j.seq.fetch_add(1, Ordering::Relaxed) + 1;
    let mut recorded = false;
    for (id, entries) in open.iter_mut() {
        if txn.is_some_and(|t| t != *id) {
            continue;
        }
        entries.push(JournalEntry {
            seq,
            txn: *id,
            verb: verb.to_string(),
            args: args.clone(),
        });
        recorded = true;
    }
    recorded.then_some(seq)
}

/// Close a transaction, discarding its journal. No compensation runs. Idempotent.
/// Reply: `{ok, txn, steps}` — `steps` is how many forward steps were dropped un-compensated.
pub fn commit(args: &Value) -> Value {
    let txn = match args.get("txn").and_then(Value::as_u64) {
        Some(t) => t,
        None => return json!({ "ok": false, "err": "no txn" }),
    };
    let dropped = journal()
        .open
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&txn);
    json!({
        "ok": true,
        "txn": txn,
        "steps": dropped.map(|e| e.len()).unwrap_or(0),
        "committed": true,
    })
}

/// Drain a transaction's entries in REVERSE `seq` order and close it.
///
/// The transaction is removed BEFORE the caller compensates, so a concurrent second abort on the
/// same id gets an empty list rather than compensating twice.
pub fn take_reversed(txn: u64) -> Vec<JournalEntry> {
    let mut entries = journal()
        .open
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&txn)
        .unwrap_or_default();
    entries.sort_by_key(|e| std::cmp::Reverse(e.seq));
    entries
}

/// Clear every open transaction. Test seam only — the journal is process-global, so a test that
/// leaves one open would change how the next test's `call` frames dispatch.
pub fn reset() {
    journal()
        .open
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}
