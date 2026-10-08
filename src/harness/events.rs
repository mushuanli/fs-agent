//! Bounded replay buffer and pending native interaction requests.
use crate::core::error::Error;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};

const MAX_EVENTS: usize = 1024;
const MAX_BYTES: usize = 8 * 1024 * 1024;
pub struct Events {
    pub epoch: String,
    pub runtime: Option<std::sync::Arc<crate::projects::runtime::ProjectRuntime>>,
    next: u64,
    bytes: usize,
    entries: VecDeque<(u64, Value, usize, i64)>,
    pub requests: HashMap<String, Value>,
}

impl Events {
    pub fn new() -> Self {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).expect("random unavailable");
        Self {
            epoch: bytes.iter().map(|b| format!("{b:02x}")).collect(),
            runtime: None,
            next: 0,
            bytes: 0,
            entries: VecDeque::new(),
            requests: HashMap::new(),
        }
    }
    pub fn push(&mut self, message: Value) -> Result<(), Error> {
        let size = message.to_string().len();
        if size > MAX_BYTES {
            return Err(Error::too_large("EFBIG"));
        }
        self.track(&message)?;
        if let Some(runtime) = &self.runtime {
            runtime.event(&message);
        }
        self.next += 1;
        self.bytes += size;
        self.entries.push_back((
            self.next,
            message,
            size,
            chrono::Utc::now().timestamp_millis(),
        ));
        while self.entries.len() > MAX_EVENTS || self.bytes > MAX_BYTES {
            if let Some((_, _, size, _)) = self.entries.pop_front() {
                self.bytes -= size;
            }
        }
        Ok(())
    }
    fn track(&mut self, message: &Value) -> Result<(), Error> {
        if message.get("method").is_some() && message.get("id").is_some() {
            let retained: usize = self
                .requests
                .values()
                .map(|value| value.to_string().len())
                .sum();
            if retained + message.to_string().len() > MAX_BYTES {
                return Err(Error::too_large("EFBIG"));
            }
            if self.requests.len() >= 128 {
                return Err(Error::too_many("HARNESS_REQUEST_LIMIT"));
            }
            self.requests
                .insert(message["id"].to_string(), message.clone());
        }
        if message["method"] == "serverRequest/resolved" {
            self.requests
                .remove(&message["params"]["requestId"].to_string());
        }
        Ok(())
    }
    pub fn page(&self, epoch: Option<&str>, after: u64) -> Value {
        let first = self.entries.front().map_or(self.next + 1, |e| e.0);
        let reset = epoch.is_some_and(|e| e != self.epoch) || after > self.next;
        let gap = reset || after.saturating_add(1) < first;
        let events: Vec<_> = self
            .entries
            .iter()
            .filter(|e| reset || e.0 > after)
            .take(128)
            .map(|(seq, message, _, timestamp)| json!({"seq": seq, "message": message, "timestamp": timestamp}))
            .collect();
        let cursor = events
            .last()
            .and_then(|e| e["seq"].as_u64())
            .unwrap_or(self.next);
        json!({"epoch":self.epoch,"cursor":cursor,"gap":gap,"events":events,
            "requests":self.requests.values().collect::<Vec<_>>()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reports_replay_gaps_and_epoch_reset() {
        let mut events = Events::new();
        for _ in 0..MAX_EVENTS + 1 {
            events
                .push(json!({"method":"item/delta","params":{}}))
                .unwrap();
        }
        let page = events.page(Some(&events.epoch), 0);
        assert_eq!(page["gap"], true);
        assert_eq!(page["events"][0]["seq"], 2);
        assert_eq!(page["cursor"], 129);
        assert_eq!(events.page(Some("old"), 1025)["gap"], true);
    }
    #[test]
    fn preserves_pending_requests_after_replay_eviction_and_resolves_them() {
        let mut events = Events::new();
        events
            .push(json!({"method":"approval","id":"pending","params":{}}))
            .unwrap();
        for _ in 0..MAX_EVENTS {
            events.push(json!({"method":"delta"})).unwrap();
        }
        assert_eq!(
            events.page(None, 0)["requests"].as_array().unwrap().len(),
            1
        );
        events
            .push(json!({"method":"serverRequest/resolved","params":{"requestId":"pending"}}))
            .unwrap();
        assert!(events.requests.is_empty());
    }
    #[test]
    fn bounds_pending_request_bytes_independently_from_replay() {
        let mut events = Events::new();
        events
            .push(json!({"method":"approval","id":1,"params":{"text":"x".repeat(MAX_BYTES / 2)}}))
            .unwrap();
        assert_eq!(
            events
                .push(
                    json!({"method":"approval","id":2,"params":{"text":"x".repeat(MAX_BYTES / 2)}})
                )
                .unwrap_err()
                .code,
            "EFBIG"
        );
        assert_eq!(events.requests.len(), 1);
    }
}
