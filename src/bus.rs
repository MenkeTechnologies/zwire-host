//! A pub/sub event bus — the host as a coordination hub across apps.
//!
//! Any long-lived connection can `sub` to topics and receive `{"ev":"pub",…}`
//! frames whenever another connection `pub`s to the same topic. Because the
//! broker is process-global, a message published by the browser HUD reaches a
//! subscribed zmax, tmux widget, or plugin — they never talk to each other
//! directly, only through the host. The daemon also publishes on well-known
//! topics itself (e.g. `scheme` / `ui` when those change), so clients get live
//! theme sync for free.
//!
//! Delivery is best-effort fan-out: a publish pushes the frame to every current
//! subscriber of the topic on that subscriber's own connection sink.
use crate::proto::{send_msg, Out};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

struct Subscriber {
    out: Out,
    topics: HashSet<String>,
}

#[derive(Default)]
struct Broker {
    next_id: u64,
    subs: HashMap<u64, Subscriber>,
}

fn broker() -> &'static Mutex<Broker> {
    static B: OnceLock<Mutex<Broker>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(Broker::default()))
}

/// Register a connection's sink as a subscriber; returns its handle. Called
/// lazily on a connection's first `sub`.
pub fn register(out: &Out) -> u64 {
    let mut b = broker().lock().unwrap();
    b.next_id += 1;
    let id = b.next_id;
    b.subs.insert(
        id,
        Subscriber {
            out: out.clone(),
            topics: HashSet::new(),
        },
    );
    id
}

/// Drop a subscriber and all its topics (called when its connection closes).
pub fn unregister(id: u64) {
    broker().lock().unwrap().subs.remove(&id);
}

/// Add a topic to a subscriber.
pub fn subscribe(id: u64, topic: &str) {
    if let Some(s) = broker().lock().unwrap().subs.get_mut(&id) {
        s.topics.insert(topic.to_string());
    }
}

/// Remove a topic from a subscriber.
pub fn unsubscribe(id: u64, topic: &str) {
    if let Some(s) = broker().lock().unwrap().subs.get_mut(&id) {
        s.topics.remove(topic);
    }
}

/// Send a single `{"ev":"pub",…}` frame to ONE sink (not a fan-out). Used for
/// snapshot-on-subscribe: hand a brand-new subscriber the current value of a
/// topic immediately, in the exact frame shape it will see for live updates, so
/// it converges without a separate `get` or a poll.
pub fn send_one(out: &Out, topic: &str, data: &Value) {
    let frame = json!({ "ev": "pub", "topic": topic, "data": data });
    let _ = send_msg(out, &frame);
}

/// Publish `data` to `topic`; returns how many subscribers received it. Target
/// sinks are collected under the lock and written to outside it, so IO never
/// blocks the broker or deadlocks a publisher that is also a subscriber.
pub fn publish(topic: &str, data: &Value) -> usize {
    let targets: Vec<Out> = {
        let b = broker().lock().unwrap();
        b.subs
            .values()
            .filter(|s| s.topics.contains(topic))
            .map(|s| s.out.clone())
            .collect()
    };
    let frame = json!({ "ev": "pub", "topic": topic, "data": data });
    targets
        .iter()
        .filter(|out| send_msg(out, &frame).is_ok())
        .count()
}
