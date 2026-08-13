//! What a page request does when NO browser is attached.
//!
//! This is a whole test binary for one behaviour because the behaviour is defined by the ABSENCE of
//! process-global state: `page::serve` flips a flag that never flips back (in production it means
//! "the browser's host is this process"), so a process where anything has claimed the endpoint can
//! no longer observe the cold path. Cargo gives no ordering between tests in a binary, so the only
//! reliable way to test "before anything attached" is a process where nothing does.
//!
//! The failure this guards against is the quiet one: a `get page.text` answering `""` — an empty
//! page, indistinguishable from a page with no text — when the truth is that no browser is running.
//! A script that files what it reads would file nothing and report success.

#![cfg(unix)]

use serde_json::json;
use zwire_host::page;

#[test]
fn with_no_browser_attached_a_page_request_says_so_instead_of_answering_empty() {
    // A scratch socket directory: without it this would dial the developer's REAL page endpoint,
    // and the test would pass or fail depending on whether their browser happens to be open.
    let dir = std::env::temp_dir().join(format!("zwpcold{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::env::set_var("XDG_RUNTIME_DIR", &dir);

    for state in ["page.text", "page.tables", page::EXTRACT_VERB] {
        let r = page::request(state, &json!({ "selector": "table" }));
        assert_eq!(r["ok"], json!(false), "{state}: {r}");
        assert!(
            r["err"].as_str().unwrap_or("").contains("not attached"),
            "{state}: {r}"
        );
        assert!(r.get("value").is_none(), "{state} answered a value: {r}");
    }

    // A postcondition is the dangerous one: `pass` must be ABSENT, because a chain that reads
    // `pass:false` reverts itself over a page nobody could read.
    let a = page::request(
        page::ASSERT_VERB,
        &json!({ "state": "page.text", "op": "contains", "value": "anything" }),
    );
    assert_eq!(a["ok"], json!(false), "{a}");
    assert!(a.get("pass").is_none(), "a verdict with no browser: {a}");

    let _ = std::fs::remove_dir_all(&dir);
}
