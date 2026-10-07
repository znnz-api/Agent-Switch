//! Incremental Chat Completions -> Anthropic Messages SSE conversion.
use anyhow::{Result, bail};
use bytes::Bytes;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};

const MAX_EVENT: usize = 2 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct ChatToMessages {
    buffer: Vec<u8>,
    started: bool,
    finished: bool,
    failed: bool,
    text_index: Option<usize>,
    next_index: usize,
    tools: BTreeMap<usize, Tool>,
    finish_reason: Option<String>,
    usage: Option<Value>,
}

#[derive(Default)]
struct Tool {
    id: String,
    name: String,
    arguments: String,
    output_index: Option<usize>,
}

impl ChatToMessages {
    pub(crate) fn push(&mut self, bytes: &[u8], pending: &mut VecDeque<Bytes>) -> Result<()> {
        // Bound each incomplete event without imposing a limit on total output.
        for &byte in bytes {
            if self.finished {
                break;
            }
            self.buffer.push(byte);
            if self.buffer.len() > MAX_EVENT {
                bail!("Upstream SSE event is too large");
            }
            if self.buffer.ends_with(b"\n\n") || self.buffer.ends_with(b"\r\n\r\n") {
                let event = std::mem::take(&mut self.buffer);
                self.event(&event, pending)?;
            }
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self, pending: &mut VecDeque<Bytes>) -> Result<()> {
        if !self.buffer.is_empty() {
            bail!("Upstream SSE ended inside an event");
        }
        if !self.finished {
            // Some compatible providers omit [DONE], but a stop reason is still
            // required; an interrupted stream must never become a success.
            if self.finish_reason.is_none() {
                bail!("Upstream SSE ended without a completion event");
            }
            self.complete(pending)?;
        }
        Ok(())
    }

    fn event(&mut self, event: &[u8], pending: &mut VecDeque<Bytes>) -> Result<()> {
        let event = std::str::from_utf8(event)?;
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|line| line.strip_prefix(' ').unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || self.finished {
            return Ok(());
        }
        if data.trim() == "[DONE]" {
            return self.complete(pending);
        }
        let value: Value = serde_json::from_str(&data)?;
        if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
            pending.push_back(frame(json!({"type":"error","error":{
                "type":"api_error","message":error.get("message").and_then(Value::as_str).unwrap_or("Upstream stream failed")
            }})));
            self.finished = true;
            self.failed = true;
            return Ok(());
        }
        if let Some(usage) = value.get("usage").filter(|usage| usage.is_object()) {
            self.usage = Some(usage.clone());
        }
        let Some(choice) = value.pointer("/choices/0") else {
            return Ok(());
        };
        if !self.started {
            pending.push_back(frame(json!({"type":"message_start","message":{
                "id":value.get("id").and_then(Value::as_str).unwrap_or("msg_agent_switch"),
                "type":"message","role":"assistant",
                "model":value.get("model").and_then(Value::as_str).unwrap_or("unknown"),
                "content":[],"stop_reason":null,"stop_sequence":null,
                "usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}
            }})));
            self.started = true;
        }
        if let Some(text) = choice
            .pointer("/delta/content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            let index = match self.text_index {
                Some(index) => index,
                None => {
                    let index = self.next_index;
                    self.next_index += 1;
                    self.text_index = Some(index);
                    pending.push_back(frame(json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}})));
                    index
                }
            };
            pending.push_back(frame(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}})));
        }
        if let Some(calls) = choice
            .pointer("/delta/tool_calls")
            .and_then(Value::as_array)
        {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if index >= 128 {
                    bail!("Too many upstream tool calls");
                }
                let tool = self.tools.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    tool.id.push_str(id);
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    tool.name.push_str(name);
                }
                if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                    tool.arguments.push_str(args);
                }
                if tool.arguments.len() > MAX_EVENT {
                    bail!("Upstream tool arguments are too large");
                }
                if tool.output_index.is_none() && !tool.id.is_empty() && !tool.name.is_empty() {
                    let output_index = self.next_index;
                    self.next_index += 1;
                    tool.output_index = Some(output_index);
                    pending.push_back(frame(
                        json!({"type":"content_block_start","index":output_index,"content_block":{
                            "type":"tool_use","id":tool.id,"name":tool.name,"input":{}
                        }}),
                    ));
                }
                if let Some(index) = tool.output_index
                    && !tool.arguments.is_empty()
                {
                    pending.push_back(frame(json!({"type":"content_block_delta","index":index,"delta":{
                        "type":"input_json_delta","partial_json":std::mem::take(&mut tool.arguments)
                    }})));
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_owned());
            // Usage usually arrives in a separate event after finish_reason.
            // Wait for [DONE]/EOF before sending message_delta/message_stop.
        }
        Ok(())
    }

    fn complete(&mut self, pending: &mut VecDeque<Bytes>) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        if !self.started {
            bail!("Upstream stream has no message");
        }
        if self.tools.values().any(|tool| tool.output_index.is_none()) {
            bail!("Upstream tool call has no ID or name");
        }
        let mut indices = self
            .tools
            .values()
            .filter_map(|tool| tool.output_index)
            .collect::<Vec<_>>();
        indices.extend(self.text_index);
        indices.sort_unstable();
        for index in indices {
            pending.push_back(frame(json!({"type":"content_block_stop","index":index})));
        }
        let reason = match self.finish_reason.as_deref() {
            Some("length") => "max_tokens",
            Some("tool_calls" | "function_call") => "tool_use",
            _ if !self.tools.is_empty() => "tool_use",
            _ => "end_turn",
        };
        let usage = crate::protocol::convert_response(
            json!({
                "choices":[{"message":{"content":""}}],"usage":self.usage.clone().unwrap_or_else(|| json!({}))
            }),
            crate::protocol::WireProtocol::ChatCompletions,
            crate::protocol::WireProtocol::Messages,
        );
        let mut usage = usage["usage"].clone();
        if usage.get("output_tokens").is_none() {
            usage["output_tokens"] = json!(0);
        }
        pending.push_back(frame(json!({"type":"message_delta","delta":{"stop_reason":reason,"stop_sequence":null},"usage":usage})));
        pending.push_back(frame(json!({"type":"message_stop"})));
        self.finished = true;
        Ok(())
    }

    pub(crate) fn succeeded(&self) -> bool {
        self.finished && !self.failed
    }
    pub(crate) fn ended(&self) -> bool {
        self.finished
    }
    pub(crate) fn abort(&mut self) {
        self.buffer.clear();
        self.finished = true;
        self.failed = true;
    }
}

fn frame(value: Value) -> Bytes {
    Bytes::from(format!(
        "event: {}\ndata: {}\n\n",
        value["type"].as_str().unwrap_or("error"),
        value
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pending: &mut VecDeque<Bytes>) -> Vec<Value> {
        pending
            .drain(..)
            .map(|bytes| {
                let text = std::str::from_utf8(&bytes).unwrap();
                serde_json::from_str(
                    text.lines()
                        .find_map(|line| line.strip_prefix("data: "))
                        .unwrap(),
                )
                .unwrap()
            })
            .collect()
    }

    fn wire(value: Value) -> String {
        format!("data: {value}\r\n\r\n")
    }

    #[test]
    fn text_sequence_closes_blocks_and_preserves_late_usage_at_every_byte_boundary() {
        let wire = [
            wire(json!({"id":"chat-1","model":"gemini-test","choices":[{"delta":{"role":"assistant"}}]})),
            wire(json!({"choices":[{"delta":{"content":"中文 hello"}}]})),
            wire(json!({"choices":[{"delta":{},"finish_reason":"length"}]})),
            wire(json!({"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":50}}})),
            "data: [DONE]\r\n\r\n".to_owned(),
        ].concat();
        for chunk_size in [1, 2, 17, wire.len()] {
            let mut converter = ChatToMessages::default();
            let mut pending = VecDeque::new();
            for chunk in wire.as_bytes().chunks(chunk_size) {
                converter.push(chunk, &mut pending).unwrap();
            }
            converter.finish(&mut pending).unwrap();
            let events = values(&mut pending);
            let types = events
                .iter()
                .map(|event| event["type"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                types,
                [
                    "message_start",
                    "content_block_start",
                    "content_block_delta",
                    "content_block_stop",
                    "message_delta",
                    "message_stop"
                ]
            );
            assert_eq!(events[2]["delta"]["text"], "中文 hello");
            assert_eq!(events[4]["delta"]["stop_reason"], "max_tokens");
            assert_eq!(events[4]["usage"]["output_tokens"], 9);
            assert_eq!(events[4]["usage"]["input_tokens"], 70);
            assert_eq!(events[4]["usage"]["cache_read_input_tokens"], 50);
            assert!(converter.succeeded());
        }
    }

    #[test]
    fn fragmented_parallel_tools_keep_distinct_indices_and_valid_arguments() {
        let events = [
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_0","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_1","function":{"name":"weather","arguments":"{}"}},{"index":0,"function":{"arguments":"\"a.txt\"}"}}]}}]}),
            json!({"choices":[{"delta":{"content":"checking"},"finish_reason":null}]}),
            json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let mut converter = ChatToMessages::default();
        let mut pending = VecDeque::new();
        for event in events {
            converter
                .push(wire(event).as_bytes(), &mut pending)
                .unwrap();
        }
        converter.push(b"data: [DONE]\n\n", &mut pending).unwrap();
        let events = values(&mut pending);
        let mut arguments = BTreeMap::<u64, String>::new();
        let mut open = BTreeMap::new();
        for event in &events {
            match event["type"].as_str().unwrap() {
                "content_block_start" => {
                    assert!(
                        open.insert(event["index"].as_u64().unwrap(), true)
                            .is_none()
                    );
                }
                "content_block_delta" => {
                    let index = event["index"].as_u64().unwrap();
                    assert_eq!(open.get(&index), Some(&true));
                    if let Some(partial) = event["delta"]["partial_json"].as_str() {
                        arguments.entry(index).or_default().push_str(partial);
                    }
                }
                "content_block_stop" => {
                    *open.get_mut(&event["index"].as_u64().unwrap()).unwrap() = false;
                }
                _ => {}
            }
        }
        assert_eq!(open.len(), 3);
        assert!(open.values().all(|value| !value));
        assert_eq!(
            serde_json::from_str::<Value>(&arguments[&0]).unwrap(),
            json!({"path":"a.txt"})
        );
        assert_eq!(events[events.len() - 2]["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn errors_and_truncated_streams_do_not_become_successful_messages() {
        let mut converter = ChatToMessages::default();
        let mut pending = VecDeque::new();
        converter
            .push(
                wire(json!({"error":{"message":"upstream failure"}})).as_bytes(),
                &mut pending,
            )
            .unwrap();
        converter.finish(&mut pending).unwrap();
        assert!(!converter.succeeded());
        let events = values(&mut pending);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "error");

        let mut converter = ChatToMessages::default();
        converter
            .push(
                wire(json!({"choices":[{"delta":{"content":"partial"}}]})).as_bytes(),
                &mut pending,
            )
            .unwrap();
        assert!(converter.finish(&mut pending).is_err());
        assert!(
            !values(&mut pending)
                .iter()
                .any(|event| event["type"] == "message_stop")
        );
        assert!(
            converter
                .push(b"data: malformed\n\n", &mut pending)
                .is_err()
        );
    }
}
