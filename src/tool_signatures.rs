//! Retain provider tool metadata that Responses/Messages clients cannot echo.
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
pub(crate) struct Cache(VecDeque<(String, String, Value)>);

impl Cache {
    fn remember(&mut self, configuration: &str, id: &str, extra: &Value) {
        let Some(signature) = extra
            .pointer("/google/thought_signature")
            .and_then(Value::as_str)
        else {
            return;
        };
        if signature.is_empty() || signature.len() > 65536 {
            return;
        }
        self.0
            .retain(|(config, call, _)| config != configuration || call != id);
        self.0
            .push_back((configuration.to_owned(), id.to_owned(), extra.clone()));
        while self.0.len() > 512 {
            self.0.pop_front();
        }
    }

    pub(crate) fn restore(&self, configuration: &str, request: &mut Value) {
        if let Some(messages) = request["messages"].as_array_mut() {
            for message in messages {
                if let Some(calls) = message["tool_calls"].as_array_mut() {
                    for call in calls {
                        if let Some((_, _, extra)) = self.0.iter().rev().find(|(config, id, _)| {
                            config == configuration && call["id"].as_str() == Some(id.as_str())
                        }) {
                            call["extra_content"] = extra.clone();
                        }
                    }
                }
            }
        }
    }
}

pub(crate) struct Tracker {
    cache: Arc<Mutex<Cache>>,
    configuration: String,
    buffer: Vec<u8>,
    call_ids: BTreeMap<u64, String>,
}

impl Tracker {
    pub(crate) fn new(cache: Arc<Mutex<Cache>>, configuration: String) -> Self {
        Self {
            cache,
            configuration,
            buffer: Vec::new(),
            call_ids: BTreeMap::new(),
        }
    }

    pub(crate) fn json(&mut self, bytes: &[u8]) {
        if let Ok(value) = serde_json::from_slice(bytes) {
            self.event(&value);
        }
    }

    pub(crate) fn sse(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.buffer.push(byte);
            if self.buffer.len() > 2 * 1024 * 1024 {
                self.buffer.clear();
            }
            if byte == b'\n' {
                let line = std::mem::take(&mut self.buffer);
                if let Some(data) = line.strip_prefix(b"data:") {
                    self.json(data);
                }
            }
        }
    }

    fn event(&mut self, value: &Value) {
        let calls = value
            .pointer("/choices/0/delta/tool_calls")
            .or_else(|| value.pointer("/choices/0/message/tool_calls"))
            .and_then(Value::as_array);
        if let Some(calls) = calls {
            for (position, call) in calls.iter().enumerate() {
                let index = call["index"].as_u64().unwrap_or(position as u64);
                if index >= 128 {
                    continue;
                }
                if let Some(id) = call["id"].as_str() {
                    self.call_ids.insert(index, id.to_owned());
                }
                if let Some(id) = self.call_ids.get(&index) {
                    if let Ok(mut cache) = self.cache.lock() {
                        cache.remember(&self.configuration, id, &call["extra_content"]);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fragmented_signatures_are_restored_only_for_matching_configuration_and_call() {
        let cache = Arc::new(Mutex::new(Cache::default()));
        let events = b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\"},{\"index\":1,\"id\":\"call_b\"}]}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"extra_content\":{\"google\":{\"thought_signature\":\"signature_b\"}}},{\"index\":0,\"extra_content\":{\"google\":{\"thought_signature\":\"signature_a\"}}}]}}]}\n\ndata: [DONE]\n\n";
        let mut tracker = Tracker::new(cache.clone(), "provider_a".into());
        for byte in events {
            tracker.sse(&[*byte]);
        }
        let original = json!({"messages":[{"role":"assistant","tool_calls":[{"id":"call_a"},{"id":"call_b"},{"id":"unknown"}]}]});
        let mut request = original.clone();
        cache.lock().unwrap().restore("provider_a", &mut request);
        assert_eq!(
            request["messages"][0]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
            "signature_a"
        );
        assert_eq!(
            request["messages"][0]["tool_calls"][1]["extra_content"]["google"]["thought_signature"],
            "signature_b"
        );
        assert!(
            request["messages"][0]["tool_calls"][2]
                .get("extra_content")
                .is_none()
        );
        let mut other = original.clone();
        cache.lock().unwrap().restore("provider_b", &mut other);
        assert_eq!(other, original);
    }

    #[test]
    fn cache_bounds_memory_and_handles_nonstreaming_tool_metadata() {
        let cache = Arc::new(Mutex::new(Cache::default()));
        let mut tracker = Tracker::new(cache.clone(), "config".into());
        for index in 0..514 {
            tracker.json(&serde_json::to_vec(&json!({"choices":[{"message":{"tool_calls":[{
                "id":format!("call_{index}"),"extra_content":{"google":{"thought_signature":"signature"}}
            }]}}]})).unwrap());
        }
        assert_eq!(cache.lock().unwrap().0.len(), 512);
        assert_eq!(cache.lock().unwrap().0.front().unwrap().1, "call_2");
    }
}
