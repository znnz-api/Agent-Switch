//! Small, conservative protocol adapters used by the local gateway.
//!
//! The adapter deliberately converts only the common request/response fields.
//! Requests whose model does not clearly belong to another protocol are left
//! untouched by the gateway. This keeps same-family traffic byte compatible.

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireProtocol {
    Responses,
    ChatCompletions,
    Messages,
    Gemini,
}

pub fn protocol_for_path(path: &str) -> Option<WireProtocol> {
    let path = path.trim_end_matches('/');
    if path.ends_with("/responses") || path == "/responses" {
        Some(WireProtocol::Responses)
    } else if path.ends_with("/messages") || path == "/messages" {
        Some(WireProtocol::Messages)
    } else if path.ends_with("/chat/completions") {
        Some(WireProtocol::ChatCompletions)
    } else if path.contains("generateContent") || path.contains("streamGenerateContent") {
        Some(WireProtocol::Gemini)
    } else {
        None
    }
}

pub fn target_for_request(
    source: WireProtocol,
    model: Option<&str>,
    base_url: &str,
) -> WireProtocol {
    // Google's /v1beta/openai endpoint speaks Chat Completions, while
    // /v1beta speaks native Gemini. Select the endpoint before model family.
    if let Ok(url) = url::Url::parse(base_url)
        && url.host_str() == Some("generativelanguage.googleapis.com")
    {
        return if url.path().trim_end_matches('/') == "/v1beta/openai" {
            WireProtocol::ChatCompletions
        } else {
            WireProtocol::Gemini
        };
    }
    let lower = format!("{} {}", model.unwrap_or_default(), base_url).to_ascii_lowercase();
    if lower.contains("anthropic.com") || lower.contains("/messages") || lower.contains("claude") {
        return WireProtocol::Messages;
    }
    if lower.contains("/chat/completions") || lower.contains("chat/completions") {
        return WireProtocol::ChatCompletions;
    }
    if lower.contains("/responses") {
        return WireProtocol::Responses;
    }
    if lower.contains("generativelanguage.googleapis.com") || lower.contains("generatecontent") {
        return WireProtocol::Gemini;
    }
    match (source, model) {
        (WireProtocol::Messages, Some(model)) if is_chat_family(model) => {
            WireProtocol::ChatCompletions
        }
        (WireProtocol::Messages, Some(model))
            if !model.is_empty() && !model.to_ascii_lowercase().contains("claude") =>
        {
            WireProtocol::ChatCompletions
        }
        _ => source,
    }
}

fn is_chat_family(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    [
        "deepseek", "qwen", "tongyi", "glm", "chatglm", "kimi", "moonshot", "doubao", "豆包",
        "yi-", "minimax", "internlm", "ernie", "文心", "hunyuan", "混元", "spark", "讯飞",
        "baichuan", "stepfun", "阶跃",
    ]
    .iter()
    .any(|family| model.contains(family))
}

pub fn convert_request(value: Value, from: WireProtocol, to: WireProtocol) -> Value {
    if from == to {
        let mut value = value;
        if to == WireProtocol::Responses {
            enable_gemini_server_side_tools(&mut value);
        }
        return value;
    }
    if from == WireProtocol::Responses && to == WireProtocol::Messages {
        return chat_to_messages(responses_to_chat(value));
    }
    if from == WireProtocol::Messages && to == WireProtocol::Responses {
        return chat_to_responses(messages_to_chat(value));
    }
    match (from, to) {
        (WireProtocol::Responses, WireProtocol::ChatCompletions) => responses_to_chat(value),
        (WireProtocol::ChatCompletions, WireProtocol::Responses) => chat_to_responses(value),
        (WireProtocol::Messages, WireProtocol::ChatCompletions) => messages_to_chat(value),
        (WireProtocol::ChatCompletions, WireProtocol::Messages) => chat_to_messages(value),
        (WireProtocol::Messages, WireProtocol::Gemini) => messages_to_gemini(value),
        (WireProtocol::ChatCompletions, WireProtocol::Gemini) => chat_to_gemini(value),
        (WireProtocol::Responses, WireProtocol::Gemini) => chat_to_gemini(responses_to_chat(value)),
        (WireProtocol::Gemini, WireProtocol::Messages) => chat_to_messages(gemini_to_chat(value)),
        (WireProtocol::Gemini, WireProtocol::ChatCompletions) => gemini_to_chat(value),
        (WireProtocol::Gemini, WireProtocol::Responses) => chat_to_responses(gemini_to_chat(value)),
        _ => value,
    }
}

fn enable_gemini_server_side_tools(value: &mut Value) {
    let Some(map) = value.as_object_mut() else {
        return;
    };
    let is_gemini = map
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(is_gemini_model);
    if !is_gemini || !map.get("tools").is_some_and(Value::is_array) {
        return;
    }
    let config = map
        .entry("tool_config")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(config) = config.as_object_mut() {
        config.insert(
            "include_server_side_tool_invocations".into(),
            Value::Bool(true),
        );
    }
}

pub fn convert_response(value: Value, from: WireProtocol, to: WireProtocol) -> Value {
    if from == to {
        return value;
    }
    match (from, to) {
        (WireProtocol::Responses, WireProtocol::ChatCompletions) => {
            responses_response_to_chat(value)
        }
        (WireProtocol::ChatCompletions, WireProtocol::Responses) => {
            chat_response_to_responses(value)
        }
        (WireProtocol::Messages, WireProtocol::ChatCompletions) => messages_response_to_chat(value),
        (WireProtocol::ChatCompletions, WireProtocol::Messages) => chat_response_to_messages(value),
        (WireProtocol::Responses, WireProtocol::Messages) => {
            chat_response_to_messages(responses_response_to_chat(value))
        }
        (WireProtocol::Messages, WireProtocol::Responses) => {
            chat_response_to_responses(messages_response_to_chat(value))
        }
        (WireProtocol::Gemini, WireProtocol::Messages) => {
            chat_response_to_messages(gemini_response_to_chat(value))
        }
        (WireProtocol::Gemini, WireProtocol::ChatCompletions) => gemini_response_to_chat(value),
        (WireProtocol::Gemini, WireProtocol::Responses) => {
            chat_response_to_responses(gemini_response_to_chat(value))
        }
        _ => value,
    }
}

fn messages_to_chat(mut value: Value) -> Value {
    let Some(map) = value.as_object_mut() else {
        return value;
    };
    let mut out = Map::new();
    for key in [
        "model",
        "stream",
        "temperature",
        "top_p",
        "stop",
        "max_tokens",
    ] {
        if let Some(v) = map.remove(key) {
            out.insert(key.to_owned(), v);
        }
    }
    if let Some(choice) = map.remove("tool_choice") {
        out.insert("tool_choice".into(), anthropic_tool_choice_to_chat(choice));
    }
    if let Some(Value::Array(tools)) = map.remove("tools") {
        let gemini_model = out
            .get("model")
            .and_then(Value::as_str)
            .is_some_and(is_gemini_model);
        out.insert(
            "tools".into(),
            Value::Array(
                tools
                    .into_iter()
                    .map(|tool| {
                        let mut converted = anthropic_tool_to_chat(tool);
                        if gemini_model {
                            sanitize_chat_tool_for_gemini(&mut converted);
                        }
                        converted
                    })
                    .collect(),
            ),
        );
    }
    if let Some(thinking) = map.remove("thinking") {
        if thinking.get("type").and_then(Value::as_str) == Some("enabled") {
            out.insert("reasoning_effort".into(), json!("high"));
        }
    }
    if let Some(v) = map.remove("max_tokens") {
        out.insert("max_tokens".to_owned(), v);
    }
    if let Some(system) = map.remove("system") {
        out.insert(
            "messages".to_owned(),
            Value::Array(vec![
                json!({"role":"system", "content": content_text(system)}),
            ]),
        );
    }
    let messages = map
        .remove("messages")
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let mut result = out
        .remove("messages")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    if let Value::Array(items) = messages {
        for item in items {
            result.extend(message_to_chat(item));
        }
    }
    out.insert("messages".to_owned(), Value::Array(result));
    if out.get("stream") == Some(&Value::Bool(true)) {
        out.insert("stream_options".into(), json!({"include_usage":true}));
    }
    Value::Object(out)
}

fn message_to_chat(item: Value) -> Vec<Value> {
    let Some(mut map) = item.as_object().cloned() else {
        return vec![item];
    };
    if map.get("role").and_then(Value::as_str) == Some("tool") {
        if let Some(id) = map.get("tool_use_id").cloned() {
            map.insert("tool_call_id".into(), id);
        }
        map.remove("tool_use_id");
    }
    let mut results = Vec::new();
    if let Some(Value::Array(content)) = map.remove("content") {
        let mut calls = Vec::new();
        let mut text = String::new();
        for part in &content {
            if part.get("type").and_then(Value::as_str) == Some("tool_use") {
                calls.push(json!({"id":part.get("id").cloned().unwrap_or(Value::Null),"type":"function","function":{"name":part.get("name").cloned().unwrap_or(Value::Null),"arguments":part.get("input").map(|v| v.to_string()).unwrap_or_default()}}));
            } else if part.get("type").and_then(Value::as_str) == Some("tool_result") {
                results.push(json!({"role":"tool","tool_call_id":part.get("tool_use_id"),"content":content_text(part.get("content").cloned().unwrap_or(Value::Null))}));
            } else if let Some(s) = part.get("text").and_then(Value::as_str) {
                text.push_str(s);
            }
        }
        let has_calls = !calls.is_empty();
        if !calls.is_empty() {
            map.insert("tool_calls".into(), Value::Array(calls));
        }
        if !text.is_empty() || has_calls || results.is_empty() {
            map.insert(
                "content".into(),
                if text.is_empty() && has_calls {
                    Value::Null
                } else {
                    Value::String(text)
                },
            );
            results.push(Value::Object(map));
        }
        return results;
    } else if let Some(content) = item.get("content") {
        map.insert("content".into(), content.clone());
    }
    vec![Value::Object(map)]
}

fn chat_to_messages(mut value: Value) -> Value {
    let Some(map) = value.as_object_mut() else {
        return value;
    };
    let mut out = Map::new();
    for key in ["model", "stream", "temperature", "top_p", "max_tokens"] {
        if let Some(v) = map.remove(key) {
            out.insert(key.to_owned(), v);
        }
    }
    if let Some(choice) = map.remove("tool_choice") {
        out.insert("tool_choice".into(), chat_tool_choice_to_anthropic(choice));
    }
    if let Some(Value::Array(tools)) = map.remove("tools") {
        out.insert(
            "tools".into(),
            Value::Array(tools.into_iter().map(chat_tool_to_anthropic).collect()),
        );
    }
    if let Some(effort) = map.remove("reasoning_effort") {
        out.insert(
            "thinking".into(),
            json!({"type":"enabled","budget_tokens": reasoning_budget(&effort)}),
        );
    }
    let mut system = None;
    let mut messages = Vec::new();
    if let Some(Value::Array(items)) = map.remove("messages") {
        for item in items {
            if item.get("role").and_then(Value::as_str) == Some("system") {
                system = item.get("content").cloned();
            } else {
                messages.push(chat_message_to_anthropic(item));
            }
        }
    }
    if let Some(system) = system {
        out.insert("system".into(), Value::String(content_text(system)));
    }
    out.insert("messages".into(), Value::Array(messages));
    Value::Object(out)
}

fn chat_message_to_anthropic(item: Value) -> Value {
    let Some(mut map) = item.as_object().cloned() else {
        return item;
    };
    if map.get("role").and_then(Value::as_str) == Some("tool") {
        let id = map.remove("tool_call_id").unwrap_or(Value::Null);
        return json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":content_text(map.remove("content").unwrap_or(Value::Null))}]});
    }
    if let Some(Value::Array(calls)) = map.remove("tool_calls") {
        let mut content = Vec::new();
        if let Some(text) = map
            .remove("content")
            .and_then(|v| (!content_text(v.clone()).is_empty()).then_some(v))
        {
            content.push(json!({"type":"text","text":content_text(text)}));
        }
        for call in calls {
            content.push(json!({"type":"tool_use","id":call.get("id"),"name":call.pointer("/function/name"),"input":call.pointer("/function/arguments").and_then(Value::as_str).and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Object(Map::new()))}));
        }
        return json!({"role":map.remove("role").unwrap_or_else(||json!("assistant")),"content":content});
    }
    if let Some(content) = map.get_mut("content") {
        *content = normalize_content(content.clone());
    }
    Value::Object(map)
}

fn responses_to_chat(mut value: Value) -> Value {
    let Some(map) = value.as_object_mut() else {
        return value;
    };
    let mut out = Map::new();
    for key in ["model", "stream", "temperature", "top_p"] {
        if let Some(v) = map.remove(key) {
            out.insert(key.into(), v);
        }
    }
    if let Some(Value::Array(tools)) = map.remove("tools") {
        let gemini = out
            .get("model")
            .and_then(Value::as_str)
            .is_some_and(is_gemini_model);
        let mut converted = Vec::new();
        for tool in tools {
            responses_tools_to_chat(tool, None, gemini, &mut converted);
        }
        if !converted.is_empty() {
            out.insert("tools".into(), Value::Array(converted));
        }
    }
    if out.contains_key("tools") {
        if let Some(choice) = map.remove("tool_choice") {
            let choice = if choice.get("type").and_then(Value::as_str) == Some("function") {
                json!({"type":"function","function":{"name":responses_wire_tool_name(&choice)}})
            } else if choice.is_string() {
                choice
            } else {
                json!("auto")
            };
            out.insert("tool_choice".into(), choice);
        }
    }
    if out.get("stream") == Some(&Value::Bool(true)) {
        out.insert("stream_options".into(), json!({"include_usage":true}));
    }
    if let Some(reasoning) = map.remove("reasoning") {
        if let Some(effort) = reasoning.get("effort") {
            out.insert("reasoning_effort".into(), effort.clone());
        }
    }
    if let Some(effort) = map.remove("reasoning_effort") {
        out.insert("reasoning_effort".into(), effort);
    }
    if let Some(v) = map.remove("max_output_tokens") {
        out.insert("max_tokens".into(), v);
    }
    if let Some(v) = map.remove("instructions") {
        out.insert(
            "messages".into(),
            json!([{"role":"system","content":content_text(v)}]),
        );
    }
    let input = map.remove("input").unwrap_or_else(|| json!(""));
    let mut messages = out
        .remove("messages")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    if let Value::String(text) = input {
        messages.push(json!({"role":"user","content":text}));
    } else if let Value::Array(items) = input {
        for item in items {
            match item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("message")
            {
                "message" => {
                    let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                    messages.push(json!({"role":if role == "developer" {"system"} else {role},
                        "content":content_text(item.get("content").cloned().unwrap_or(Value::Null))}));
                }
                "function_call" | "custom_tool_call" => {
                    let arguments = if item["type"] == "custom_tool_call" {
                        json!({"input":item.get("input").cloned().unwrap_or(json!(""))}).to_string()
                    } else {
                        item.get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}")
                            .to_owned()
                    };
                    messages.push(json!({"role":"assistant","content":null,"tool_calls":[{
                        "id":item.get("call_id"),"type":"function", "function":{
                            "name":responses_wire_tool_name(&item),"arguments":arguments}}]}));
                }
                "function_call_output" | "custom_tool_call_output" => messages.push(json!({
                    "role":"tool","tool_call_id":item.get("call_id"),
                    "content":content_text(item.get("output").cloned().unwrap_or(Value::Null))})),
                _ => {} // Reasoning and other Responses-only records are not chat messages.
            }
        }
    }
    out.insert("messages".into(), Value::Array(messages));
    Value::Object(out)
}

fn responses_wire_tool_name(value: &Value) -> String {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match value.get("namespace").and_then(Value::as_str) {
        Some(namespace) => format!("{namespace}__{name}"),
        None => name.to_owned(),
    }
}

fn responses_tools_to_chat(
    tool: Value,
    namespace: Option<&str>,
    gemini: bool,
    output: &mut Vec<Value>,
) {
    if tool["type"] == "namespace" {
        if let Some(tools) = tool.get("tools").and_then(Value::as_array) {
            for inner in tools {
                responses_tools_to_chat(inner.clone(), tool["name"].as_str(), gemini, output);
            }
        }
        return;
    }
    let kind = tool
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("function");
    if !matches!(kind, "function" | "custom") {
        return;
    }
    let source = tool.get("function").unwrap_or(&tool);
    let Some(name) = source.get("name").and_then(Value::as_str) else {
        return;
    };
    let name = namespace.map_or_else(
        || name.to_owned(),
        |namespace| format!("{namespace}__{name}"),
    );
    let mut function = json!({"name":name,"parameters":if kind == "custom" {
        json!({"type":"object","properties":{"input":{"type":"string"}},"required":["input"]})
    } else { source.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object","properties":{}})) }});
    if let Some(description) = source.get("description").filter(|v| v.is_string()) {
        function["description"] = description.clone();
    }
    if !gemini && let Some(strict) = source.get("strict") {
        function["strict"] = strict.clone();
    }
    let mut converted = json!({"type":"function","function":function});
    if gemini {
        sanitize_chat_tool_for_gemini(&mut converted);
    }
    output.push(converted);
}

pub fn restore_responses_tool_names(response: &mut Value, request: &Value) {
    let mut bindings = Vec::new();
    fn collect(
        tools: &[Value],
        namespace: Option<&str>,
        bindings: &mut Vec<(String, String, Option<String>, bool)>,
    ) {
        for tool in tools {
            if tool["type"] == "namespace" {
                if let Some(inner) = tool["tools"].as_array() {
                    collect(inner, tool["name"].as_str(), bindings);
                }
            } else if let Some(name) = tool["name"].as_str() {
                let wire = namespace.map_or_else(
                    || name.to_owned(),
                    |namespace| format!("{namespace}__{name}"),
                );
                bindings.push((
                    wire,
                    name.to_owned(),
                    namespace.map(ToOwned::to_owned),
                    tool["type"] == "custom",
                ));
            }
        }
    }
    if let Some(tools) = request["tools"].as_array() {
        collect(tools, None, &mut bindings);
    }
    if let Some(items) = response["output"].as_array_mut() {
        for item in items {
            if item["type"] != "function_call" {
                continue;
            }
            if let Some((_, name, namespace, custom)) = bindings
                .iter()
                .find(|(wire, ..)| item["name"].as_str() == Some(wire.as_str()))
            {
                item["name"] = json!(name);
                if let Some(namespace) = namespace {
                    item["namespace"] = json!(namespace);
                }
                if *custom {
                    item["type"] = json!("custom_tool_call");
                    let input = item["arguments"]
                        .as_str()
                        .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
                        .and_then(|arguments| arguments["input"].as_str().map(ToOwned::to_owned))
                        .unwrap_or_default();
                    item.as_object_mut().unwrap().remove("arguments");
                    item["input"] = json!(input);
                }
            }
        }
    }
}

fn chat_to_responses(mut value: Value) -> Value {
    let Some(map) = value.as_object_mut() else {
        return value;
    };
    let mut out = Map::new();
    for key in [
        "model",
        "stream",
        "temperature",
        "top_p",
        "tools",
        "tool_choice",
    ] {
        if let Some(v) = map.remove(key) {
            out.insert(key.into(), v);
        }
    }
    if let Some(effort) = map.remove("reasoning_effort") {
        out.insert("reasoning".into(), json!({"effort":effort}));
    }
    if let Some(v) = map.remove("max_tokens") {
        out.insert("max_output_tokens".into(), v);
    }
    let mut input = Vec::new();
    let mut instructions = None;
    if let Some(Value::Array(items)) = map.remove("messages") {
        for item in items {
            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
            if role == "system" {
                instructions = Some(content_text(
                    item.get("content").cloned().unwrap_or(Value::Null),
                ));
            } else {
                input.push(json!({"type":"message","role":role,"content":[{"type":"input_text","text":content_text(item.get("content").cloned().unwrap_or(Value::Null))}]}));
            }
        }
    }
    if let Some(v) = instructions {
        out.insert("instructions".into(), Value::String(v));
    }
    out.insert("input".into(), Value::Array(input));
    Value::Object(out)
}

fn content_text(value: Value) -> String {
    match value {
        Value::String(s) => s,
        Value::Array(a) => a
            .into_iter()
            .filter_map(|v| v.get("text").and_then(Value::as_str).map(ToOwned::to_owned))
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
fn normalize_content(value: Value) -> Value {
    if value.is_string() {
        value
    } else {
        Value::String(content_text(value))
    }
}

fn reasoning_budget(value: &Value) -> u64 {
    match value.as_str().unwrap_or("medium") {
        "low" => 2048,
        "high" => 8192,
        "xhigh" | "maximum" => 16384,
        _ => 4096,
    }
}

fn anthropic_tool_choice_to_chat(value: Value) -> Value {
    match value {
        Value::String(value) => match value.as_str() {
            "auto" | "none" | "required" => Value::String(value),
            "any" => Value::String("required".into()),
            _ => Value::String("auto".into()),
        },
        Value::Object(map) => match map.get("type").and_then(Value::as_str) {
            Some("tool") => json!({
                "type": "function",
                "function": {"name": map.get("name").cloned().unwrap_or(Value::Null)}
            }),
            Some("any") => Value::String("required".into()),
            Some("none") => Value::String("none".into()),
            _ => Value::String("auto".into()),
        },
        _ => Value::String("auto".into()),
    }
}

fn chat_tool_choice_to_anthropic(value: Value) -> Value {
    match value {
        Value::String(value) => match value.as_str() {
            "required" => json!({"type": "any"}),
            "none" => json!({"type": "none"}),
            _ => json!({"type": "auto"}),
        },
        Value::Object(map) if map.get("type").and_then(Value::as_str) == Some("function") => {
            json!({
                "type": "tool",
                "name": map.get("function").and_then(|value| value.get("name")).cloned().unwrap_or(Value::Null)
            })
        }
        Value::Object(map) => Value::Object(map),
        _ => json!({"type": "auto"}),
    }
}

fn anthropic_tool_to_chat(value: Value) -> Value {
    json!({
        "type":"function",
        "function": {
            "name": value.get("name").cloned().unwrap_or(Value::Null),
            "description": value.get("description").cloned().unwrap_or(Value::Null),
            "parameters": value.get("input_schema").cloned().unwrap_or_else(|| json!({"type":"object"}))
        }
    })
}

fn chat_tool_to_anthropic(value: Value) -> Value {
    let function = value.get("function").unwrap_or(&value);
    json!({
        "name": function.get("name").cloned().unwrap_or(Value::Null),
        "description": function.get("description").cloned().unwrap_or(Value::Null),
        "input_schema": function.get("parameters").cloned().unwrap_or_else(|| json!({"type":"object"}))
    })
}

fn messages_to_gemini(value: Value) -> Value {
    chat_to_gemini(messages_to_chat(value))
}

fn chat_to_gemini(mut value: Value) -> Value {
    let Some(map) = value.as_object_mut() else {
        return value;
    };
    let mut out = Map::new();
    let mut contents = Vec::new();
    let mut system_text = Vec::new();
    if let Some(system) = map.remove("system") {
        let text = content_text(system);
        if !text.is_empty() {
            system_text.push(text);
        }
    }
    if let Some(Value::Array(messages)) = map.remove("messages") {
        for message in messages {
            if message.get("role").and_then(Value::as_str) == Some("system") {
                let text = content_text(message.get("content").cloned().unwrap_or(Value::Null));
                if !text.is_empty() {
                    system_text.push(text);
                }
                continue;
            }
            let role = match message.get("role").and_then(Value::as_str) {
                Some("assistant") => "model",
                _ => "user",
            };
            let text = content_text(message.get("content").cloned().unwrap_or(Value::Null));
            if !text.is_empty() {
                contents.push(json!({"role":role,"parts":[{"text":text}]}));
            }
            if let Some(Value::Array(calls)) = message.get("tool_calls") {
                let parts = calls.iter().map(|call| json!({"functionCall":{"name":call.pointer("/function/name"),"args":call.pointer("/function/arguments").and_then(Value::as_str).and_then(|s|serde_json::from_str::<Value>(s).ok()).unwrap_or_else(||json!({}))}})).collect::<Vec<_>>();
                if !parts.is_empty() {
                    contents.push(json!({"role":"model","parts":parts}));
                }
            }
        }
    }
    out.insert("contents".into(), Value::Array(contents));
    if !system_text.is_empty() {
        out.insert(
            "systemInstruction".into(),
            json!({"parts":[{"text":system_text.join("\n\n")}]}),
        );
    }
    let mut generation = Map::new();
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("max_tokens", "maxOutputTokens"),
    ] {
        if let Some(v) = map.remove(from) {
            generation.insert(to.into(), v);
        }
    }
    if let Some(Value::Array(stop)) = map.remove("stop") {
        generation.insert("stopSequences".into(), Value::Array(stop));
    }
    if !generation.is_empty() {
        out.insert("generationConfig".into(), Value::Object(generation));
    }
    if let Some(Value::Array(tools)) = map.remove("tools") {
        let declarations=tools.into_iter().map(|tool| {
            let f=tool.get("function").unwrap_or(&tool);
            let parameters = sanitize_gemini_schema(
                f.get("parameters").cloned().unwrap_or_else(||json!({"type":"object"}))
            );
            json!({"name":f.get("name"),"description":f.get("description"),"parameters":parameters})
        }).collect::<Vec<_>>();
        out.insert(
            "tools".into(),
            json!([{"functionDeclarations":declarations}]),
        );
    }
    Value::Object(out)
}

fn is_gemini_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("gemini")
}

/// Gemini's function declaration schema is a deliberately small subset of
/// JSON Schema. Claude Code sends full JSON Schema documents (including
/// `$schema`, `propertyNames`, `const`, unions, and tuple-style `items`),
/// which Gemini rejects or may spend a very long time retrying. Keep the
/// useful structural fields and lower lossy constructs to a valid schema.
fn sanitize_chat_tool_for_gemini(tool: &mut Value) {
    if let Some(parameters) = tool.pointer_mut("/function/parameters") {
        *parameters = sanitize_gemini_schema(parameters.clone());
    }
}

fn sanitize_gemini_schema(value: Value) -> Value {
    let Value::Object(mut source) = value else {
        return json!({"type":"object"});
    };
    let mut out = Map::new();

    if let Some(kind) = source.remove("type") {
        match kind {
            Value::String(kind) => {
                out.insert("type".into(), Value::String(kind));
            }
            Value::Array(kinds) => {
                let nullable = kinds.iter().any(|kind| kind.as_str() == Some("null"));
                if let Some(kind) = kinds
                    .iter()
                    .find_map(Value::as_str)
                    .filter(|kind| *kind != "null")
                {
                    out.insert("type".into(), Value::String(kind.to_owned()));
                }
                if nullable {
                    out.insert("nullable".into(), Value::Bool(true));
                }
            }
            _ => {}
        }
    }
    for key in [
        "format",
        "title",
        "description",
        "nullable",
        "maxItems",
        "minItems",
        "minProperties",
        "maxProperties",
    ] {
        if let Some(value) = source.remove(key) {
            out.insert(key.into(), value);
        }
    }
    if let Some(Value::Array(values)) = source.remove("enum") {
        out.insert("enum".into(), Value::Array(values));
    }
    if let Some(Value::Array(required)) = source.remove("required") {
        out.insert("required".into(), Value::Array(required));
    }
    if let Some(Value::Array(ordering)) = source.remove("propertyOrdering") {
        out.insert("propertyOrdering".into(), Value::Array(ordering));
    }
    if let Some(Value::Object(properties)) = source.remove("properties") {
        let properties = properties
            .into_iter()
            .map(|(name, schema)| (name, sanitize_gemini_schema(schema)))
            .collect();
        out.insert("properties".into(), Value::Object(properties));
    }
    if let Some(items) = source.remove("items") {
        let items = match items {
            Value::Object(_) => sanitize_gemini_schema(items),
            // JSON Schema tuple arrays are not accepted by Gemini. The
            // first element is the closest useful homogeneous approximation.
            Value::Array(mut values) => values
                .drain(..)
                .next()
                .map(sanitize_gemini_schema)
                .unwrap_or_else(|| json!({"type":"string"})),
            _ => json!({"type":"string"}),
        };
        out.insert("items".into(), items);
    } else if out.get("type").and_then(Value::as_str) == Some("array") {
        // Gemini requires an element schema for every ARRAY, while JSON
        // Schema permits an omitted `items` field to mean any value.
        out.insert("items".into(), json!({"type":"string"}));
    }
    if let Some(Value::Array(union)) = source.remove("anyOf").or_else(|| source.remove("oneOf")) {
        let nullable = union
            .iter()
            .any(|item| item.get("type").and_then(Value::as_str) == Some("null"));
        if let Some(schema) = union
            .into_iter()
            .find(|item| item.get("type").and_then(Value::as_str) != Some("null"))
        {
            let mut selected = sanitize_gemini_schema(schema);
            if nullable {
                selected["nullable"] = Value::Bool(true);
            }
            return selected;
        }
    }
    if out.get("type").is_none() {
        out.insert("type".into(), Value::String("object".into()));
    }
    Value::Object(out)
}

fn gemini_to_chat(mut value: Value) -> Value {
    let Some(map) = value.as_object_mut() else {
        return value;
    };
    let mut out = Map::new();
    let mut messages = Vec::new();
    if let Some(system) = map.remove("systemInstruction") {
        messages.push(json!({"role":"system","content":content_text(system.get("parts").cloned().unwrap_or(Value::Null))}));
    }
    if let Some(Value::Array(contents)) = map.remove("contents") {
        for item in contents {
            let role = if item.get("role").and_then(Value::as_str) == Some("model") {
                "assistant"
            } else {
                "user"
            };
            let text = content_text(item.get("parts").cloned().unwrap_or(Value::Null));
            messages.push(json!({"role":role,"content":text}));
        }
    }
    out.insert("messages".into(), Value::Array(messages));
    if let Some(generation) = map.remove("generationConfig") {
        for (from, to) in [
            ("temperature", "temperature"),
            ("topP", "top_p"),
            ("maxOutputTokens", "max_tokens"),
            ("stopSequences", "stop"),
        ] {
            if let Some(v) = generation.get(from) {
                out.insert(to.into(), v.clone());
            }
        }
    }
    if let Some(Value::Array(groups)) = map.remove("tools") {
        let tools=groups.iter().flat_map(|g|g.get("functionDeclarations").and_then(Value::as_array).cloned().unwrap_or_default()).map(|f|json!({"type":"function","function":{"name":f.get("name"),"description":f.get("description"),"parameters":f.get("parameters").cloned().unwrap_or_else(||json!({"type":"object"}))}})).collect();
        out.insert("tools".into(), Value::Array(tools));
    }
    Value::Object(out)
}

fn gemini_response_to_chat(value: Value) -> Value {
    let mut text = String::new();
    let mut calls = Vec::new();
    if let Some(Value::Array(candidates)) = value.get("candidates") {
        for candidate in candidates {
            if let Some(parts) = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
            {
                for part in parts {
                    if let Some(s) = part.get("text").and_then(Value::as_str) {
                        text.push_str(s);
                    }
                    if let Some(call) = part.get("functionCall") {
                        calls.push(json!({"id":format!("call_{}",calls.len()),"type":"function","function":{"name":call.get("name"),"arguments":serde_json::to_string(call.get("args").unwrap_or(&Value::Null)).unwrap_or_default()}}));
                    }
                }
            }
        }
    }
    let mut message = json!({"role":"assistant","content":text});
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    json!({"id":value.get("responseId").cloned().unwrap_or_else(||json!("chatcmpl-agent-switch")),"object":"chat.completion","model":value.get("modelVersion").cloned().unwrap_or(Value::Null),"choices":[{"index":0,"message":message,"finish_reason":"stop"}],"usage":value.get("usageMetadata").cloned().unwrap_or(Value::Null)})
}

fn responses_response_to_chat(value: Value) -> Value {
    let text = value
        .pointer("/output/0/content/0/text")
        .and_then(Value::as_str)
        .or_else(|| value.get("output_text").and_then(Value::as_str))
        .unwrap_or_default();
    json!({"id":value.get("id").cloned().unwrap_or(json!("chatcmpl-agent-switch")),"object":"chat.completion","model":value.get("model").cloned().unwrap_or(Value::Null),"choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],"usage":value.get("usage").cloned().unwrap_or(Value::Null)})
}
fn chat_response_to_responses(value: Value) -> Value {
    let message = value
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut output = Vec::new();
    if !text.is_empty() {
        output.push(json!({"type":"message","id":"msg_agent_switch","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}));
    }
    if let Some(calls) = message["tool_calls"].as_array() {
        for (index, call) in calls.iter().enumerate() {
            output.push(json!({"type":"function_call","id":format!("fc_agent_switch_{index}"),"status":"completed",
                "call_id":call.get("id"),"name":call.pointer("/function/name"),"arguments":call.pointer("/function/arguments")}));
        }
    }
    let usage = value.get("usage").map(|usage| json!({
        "input_tokens":usage.get("prompt_tokens").cloned().unwrap_or(Value::Null),
        "output_tokens":usage.get("completion_tokens").cloned().unwrap_or(Value::Null),
        "total_tokens":usage.get("total_tokens").cloned().unwrap_or(Value::Null),
        "input_tokens_details":{"cached_tokens":usage.pointer("/prompt_tokens_details/cached_tokens").cloned().unwrap_or(json!(0))}
    })).unwrap_or(Value::Null);
    json!({"id":value.get("id").cloned().unwrap_or(json!("resp_agent_switch")),"object":"response","status":"completed","model":value.get("model").cloned().unwrap_or(Value::Null),"output":output,"output_text":text,"usage":usage})
}
fn messages_response_to_chat(value: Value) -> Value {
    let blocks = value
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let text = blocks
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    let tool_calls = blocks.iter().filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use")).map(|block| json!({
        "id": block.get("id").cloned().unwrap_or_else(|| json!("call_agent_switch")),
        "type": "function",
        "function": {
            "name": block.get("name").cloned().unwrap_or(Value::Null),
            "arguments": serde_json::to_string(block.get("input").unwrap_or(&Value::Null)).unwrap_or_else(|_| "{}".into())
        }
    })).collect::<Vec<_>>();
    let mut message = json!({"role":"assistant","content": if text.is_empty() && !tool_calls.is_empty() { Value::Null } else { Value::String(text) }});
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let finish_reason = if message.get("tool_calls").is_some() {
        "tool_calls"
    } else if value["stop_reason"] == "max_tokens" {
        "length"
    } else {
        "stop"
    };
    let usage = value
        .get("usage")
        .map(|usage| {
            // Messages input excludes cached reads and writes; Chat/Responses includes them.
            let read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
            let write = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            let input = usage["input_tokens"]
                .as_u64()
                .and_then(|input| input.checked_add(read)?.checked_add(write));
            let output = usage["output_tokens"].as_u64();
            json!({"prompt_tokens":input,"completion_tokens":output,
            "total_tokens":input.zip(output).and_then(|(input, output)| input.checked_add(output)),
            "prompt_tokens_details":{"cached_tokens":read,"cache_write_tokens":write}})
        })
        .unwrap_or(Value::Null);
    json!({"id":value.get("id").cloned().unwrap_or(json!("chatcmpl-agent-switch")),"object":"chat.completion","model":value.get("model").cloned().unwrap_or(Value::Null),"choices":[{"index":0,"message":message,"finish_reason":finish_reason}],"usage":usage})
}
fn chat_response_to_messages(value: Value) -> Value {
    let message = value
        .pointer("/choices/0/message")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(json!({"type":"text","text":text}));
    }
    if let Some(Value::Array(calls)) = message.get("tool_calls") {
        for call in calls {
            let input = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .and_then(|value| serde_json::from_str::<Value>(value).ok())
                .unwrap_or_else(|| json!({}));
            content.push(json!({"type":"tool_use","id":call.get("id").cloned().unwrap_or_else(|| json!("call_agent_switch")),"name":call.pointer("/function/name").cloned().unwrap_or(Value::Null),"input":input}));
        }
    }
    if content.is_empty() {
        content.push(json!({"type":"text","text":""}));
    }
    let mut usage = value.get("usage").cloned().unwrap_or(json!({}));
    if let Some(input) = usage.get("prompt_tokens").and_then(Value::as_u64) {
        let read = usage
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let write = usage
            .pointer("/prompt_tokens_details/cache_write_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        usage["input_tokens"] = json!(input.saturating_sub(read).saturating_sub(write));
        usage["cache_read_input_tokens"] = json!(read);
        usage["cache_creation_input_tokens"] = json!(write);
    }
    if let Some(output) = usage.get("completion_tokens").cloned() {
        usage["output_tokens"] = output;
    }
    json!({"id":value.get("id").cloned().unwrap_or(json!("msg_agent_switch")),"type":"message","role":"assistant","model":value.get("model").cloned().unwrap_or(Value::Null),"content":content,"stop_reason":if message.get("tool_calls").is_some() { json!("tool_use") } else if value.pointer("/choices/0/finish_reason").and_then(Value::as_str) == Some("length") { json!("max_tokens") } else { json!("end_turn") },"stop_sequence":Value::Null,"usage":usage})
}

#[cfg(test)]
mod tests {
    #[test]
    fn responses_tools_and_results_round_trip_through_google_chat() {
        let request = json!({"model":"models/gemini-3.8-flash","stream":true,
            "tools":[
                {"type":"function","name":"lookup","strict":true,"parameters":{"type":"object","properties":{"city":{"type":"string"}},"additionalProperties":false}},
                {"type":"namespace","name":"functions","tools":[{"type":"function","name":"read","parameters":{"type":"object","properties":{}}}]},
                {"type":"custom","name":"patch","format":{"type":"grammar"}},
                {"type":"web_search","external_web_access":true}],
            "tool_choice":{"type":"function","namespace":"functions","name":"read"},
            "input":[{"role":"developer","content":"Be concise"},
                {"type":"function_call","namespace":"functions","name":"read","call_id":"call_1","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"fixture"}]});
        let chat = convert_request(
            request.clone(),
            WireProtocol::Responses,
            WireProtocol::ChatCompletions,
        );
        assert_eq!(chat["tools"].as_array().unwrap().len(), 3);
        assert_eq!(chat["tools"][0]["function"]["name"], "lookup");
        assert!(chat["tools"][0]["function"].get("strict").is_none());
        assert!(
            chat["tools"][0]["function"]["parameters"]
                .get("additionalProperties")
                .is_none()
        );
        assert_eq!(chat["tools"][1]["function"]["name"], "functions__read");
        assert_eq!(chat["tool_choice"]["function"]["name"], "functions__read");
        assert_eq!(chat["messages"][0]["role"], "system");
        assert_eq!(
            chat["messages"][1]["tool_calls"][0]["function"]["name"],
            "functions__read"
        );
        assert_eq!(chat["messages"][2]["tool_call_id"], "call_1");
        assert_eq!(chat["stream_options"]["include_usage"], true);
        let mut response = convert_response(
            json!({"choices":[{"message":{"tool_calls":[
            {"id":"call_2","function":{"name":"functions__read","arguments":"{}"}},
            {"id":"call_3","function":{"name":"patch","arguments":"{\"input\":\"*** Begin Patch\"}"}}
        ]}}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}),
            WireProtocol::ChatCompletions,
            WireProtocol::Responses,
        );
        restore_responses_tool_names(&mut response, &request);
        assert_eq!(response["output"][0]["namespace"], "functions");
        assert_eq!(response["output"][0]["name"], "read");
        assert_eq!(response["output"][0]["call_id"], "call_2");
        assert_eq!(response["output"][1]["type"], "custom_tool_call");
        assert_eq!(response["output"][1]["input"], "*** Begin Patch");
        assert_eq!(response["usage"]["input_tokens"], 10);
    }

    use super::*;

    #[test]
    fn tool_results_keep_call_ids_and_payload_in_chat_history() {
        let input = json!({"model":"gpt-5.4-mini","messages":[
            {"role":"user","content":"Read the token"},
            {"role":"assistant","content":[{"type":"tool_use","id":"call_1","name":"read_file","input":{"path":"test.txt"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"UNIQUE_TOOL_RESULT_4837"}]}
        ]});
        let output = convert_request(input, WireProtocol::Messages, WireProtocol::ChatCompletions);
        assert_eq!(output["messages"][1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            output["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"test.txt\"}"
        );
        assert_eq!(output["messages"][2]["role"], "tool");
        assert_eq!(output["messages"][2]["tool_call_id"], "call_1");
        assert_eq!(output["messages"][2]["content"], "UNIQUE_TOOL_RESULT_4837");
    }

    #[test]
    fn provider_protocol_detection_keeps_xai_responses_and_maps_chat_families() {
        assert_eq!(
            target_for_request(
                WireProtocol::Responses,
                Some("grok-4.7"),
                "https://api.x.ai/v1"
            ),
            WireProtocol::Responses
        );
        assert_eq!(
            target_for_request(
                WireProtocol::Messages,
                Some("deepseek-v4"),
                "https://api.deepseek.com/v1"
            ),
            WireProtocol::ChatCompletions
        );
        assert_eq!(
            target_for_request(
                WireProtocol::Messages,
                Some("gemini-2.5-pro"),
                "https://generativelanguage.googleapis.com/v1beta"
            ),
            WireProtocol::Gemini
        );
        assert_eq!(
            target_for_request(
                WireProtocol::Messages,
                Some("gemini-2.5-pro"),
                "https://api.example.test/v1"
            ),
            WireProtocol::ChatCompletions
        );
    }

    #[test]
    fn google_openai_endpoint_uses_chat_for_all_client_protocols_and_catalog_ids() {
        for source in [
            WireProtocol::Responses,
            WireProtocol::Messages,
            WireProtocol::ChatCompletions,
        ] {
            for model in ["gemini-3.8-flash", "models/gemini-3.8-flash"] {
                assert_eq!(
                    target_for_request(
                        source,
                        Some(model),
                        "https://generativelanguage.googleapis.com/v1beta/openai/"
                    ),
                    WireProtocol::ChatCompletions
                );
                assert_eq!(
                    target_for_request(
                        source,
                        Some(model),
                        "https://generativelanguage.googleapis.com/v1beta"
                    ),
                    WireProtocol::Gemini
                );
            }
        }
    }

    #[test]
    fn gemini_responses_enable_server_side_tool_invocations() {
        let request = convert_request(
            json!({
                "model":"gemini-3.8-flash",
                "tools":[{"type":"function","name":"lookup"}]
            }),
            WireProtocol::Responses,
            WireProtocol::Responses,
        );
        assert_eq!(
            request["tool_config"]["include_server_side_tool_invocations"],
            true
        );

        let openai = convert_request(
            json!({"model":"gpt-6.1-sol","tools":[{"type":"function","name":"lookup"}]}),
            WireProtocol::Responses,
            WireProtocol::Responses,
        );
        assert!(openai.get("tool_config").is_none());
    }

    #[test]
    fn tools_and_reasoning_survive_messages_chat_conversion() {
        let source = json!({
            "model":"deepseek-v4",
            "thinking":{"type":"enabled"},
            "tools":[{"name":"weather","description":"read weather","input_schema":{"type":"object","properties":{"city":{"type":"string"}}}}],
            "tool_choice":{"type":"tool","name":"weather"},
            "messages":[{"role":"user","content":"weather?"}]
        });
        let chat = convert_request(
            source,
            WireProtocol::Messages,
            WireProtocol::ChatCompletions,
        );
        assert_eq!(chat["reasoning_effort"], "high");
        assert_eq!(chat["tools"][0]["function"]["name"], "weather");
        assert_eq!(chat["tool_choice"]["function"]["name"], "weather");
        let anthropic =
            convert_request(chat, WireProtocol::ChatCompletions, WireProtocol::Messages);
        assert_eq!(anthropic["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(anthropic["thinking"]["type"], "enabled");
        assert_eq!(anthropic["tool_choice"]["type"], "tool");

        let messages = convert_response(
            json!({
                "id":"chatcmpl-test",
                "model":"gpt-5.4-mini",
                "choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}]}}]
            }),
            WireProtocol::ChatCompletions,
            WireProtocol::Messages,
        );
        assert_eq!(messages["content"][0]["type"], "tool_use");
        assert_eq!(messages["content"][0]["input"]["city"], "Paris");
    }

    #[test]
    fn gemini_tool_conversion_removes_unsupported_json_schema_and_repairs_items() {
        let converted = convert_request(
            json!({
                "model":"gemini-3.8-flash",
                "messages":[{"role":"user","content":"inspect"}],
                "tools":[{"name":"inspect","description":"Inspect a value","input_schema":{
                    "$schema":"https://json-schema.org/draft/2020-12/schema",
                    "type":"object",
                    "properties":{
                        "query":{"type":"array","items":[{"type":"string"}]},
                        "where":{"anyOf":[{"type":"string"},{"type":"null"}],"const":"ignored"}
                    },
                    "propertyNames":{"pattern":"^[a-z]+$"},
                    "additionalProperties":false
                }}]
            }),
            WireProtocol::Messages,
            WireProtocol::ChatCompletions,
        );
        let parameters = &converted["tools"][0]["function"]["parameters"];
        assert!(parameters.get("$schema").is_none());
        assert!(parameters.get("propertyNames").is_none());
        assert!(parameters.get("additionalProperties").is_none());
        assert_eq!(parameters["properties"]["query"]["items"]["type"], "string");
        assert_eq!(parameters["properties"]["where"]["type"], "string");
        assert_eq!(parameters["properties"]["where"]["nullable"], true);
    }

    #[test]
    fn gemini_request_and_response_have_expected_shapes() {
        let gemini = convert_request(
            json!({"model":"gemini-2.5-pro","messages":[{"role":"user","content":"hello"}],"max_tokens":32}),
            WireProtocol::ChatCompletions,
            WireProtocol::Gemini,
        );
        assert_eq!(gemini["contents"][0]["parts"][0]["text"], "hello");
        assert_eq!(gemini["generationConfig"]["maxOutputTokens"], 32);
        let chat = convert_response(
            json!({"candidates":[{"content":{"parts":[{"text":"hi"}]},"finishReason":"STOP"}],"modelVersion":"gemini-2.5-pro"}),
            WireProtocol::Gemini,
            WireProtocol::ChatCompletions,
        );
        assert_eq!(chat["choices"][0]["message"]["content"], "hi");
    }

    #[test]
    fn anthropic_system_prompt_is_preserved_when_converted_to_gemini() {
        let gemini = convert_request(
            json!({
                "model":"gemini-2.5-pro",
                "system":"Answer in concise Chinese.",
                "messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]
            }),
            WireProtocol::Messages,
            WireProtocol::Gemini,
        );
        assert_eq!(
            gemini["systemInstruction"]["parts"][0]["text"],
            "Answer in concise Chinese."
        );
        assert_eq!(gemini["contents"][0]["role"], "user");
        assert_eq!(gemini["contents"][0]["parts"][0]["text"], "hello");
    }

    #[test]
    fn responses_and_messages_conversion_preserves_system_and_text() {
        let messages = convert_request(
            json!({
                "model":"claude-opus-4-6",
                "instructions":"Follow the policy.",
                "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]
            }),
            WireProtocol::Responses,
            WireProtocol::Messages,
        );
        assert_eq!(messages["system"], "Follow the policy.");
        assert_eq!(messages["messages"][0]["role"], "user");
        assert_eq!(messages["messages"][0]["content"], "hello");

        let responses = convert_response(
            json!({
                "type":"message",
                "id":"msg_1",
                "model":"claude-opus-4-6",
                "content":[{"type":"text","text":"hi"}],
                "stop_reason":"end_turn",
                "usage":{"input_tokens":12,"output_tokens":3}
            }),
            WireProtocol::Messages,
            WireProtocol::Responses,
        );
        assert_eq!(responses["output_text"], "hi");
        assert_eq!(responses["output"][0]["content"][0]["text"], "hi");
        assert_eq!(responses["usage"]["input_tokens"], 12);
        assert_eq!(responses["usage"]["output_tokens"], 3);
        assert_eq!(responses["usage"]["total_tokens"], 15);
    }

    #[test]
    fn responses_and_gemini_conversion_preserves_user_text() {
        let gemini = convert_request(
            json!({
                "model":"gemini-2.5-pro",
                "instructions":"Be brief.",
                "input":"hello"
            }),
            WireProtocol::Responses,
            WireProtocol::Gemini,
        );
        assert_eq!(gemini["systemInstruction"]["parts"][0]["text"], "Be brief.");
        assert_eq!(gemini["contents"][0]["parts"][0]["text"], "hello");

        let responses = convert_response(
            json!({
                "candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP"}],
                "modelVersion":"gemini-2.5-pro"
            }),
            WireProtocol::Gemini,
            WireProtocol::Responses,
        );
        assert_eq!(responses["output_text"], "hi");
        assert_eq!(responses["output"][0]["content"][0]["text"], "hi");
    }
}
