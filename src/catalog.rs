use crate::gateway::GatewayIdentity;
use crate::network_proxy;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::Duration;
use url::Url;

const GENERIC_CODEX_INSTRUCTIONS: &str = "You are a coding agent running in a terminal. Inspect relevant files before changing them, preserve unrelated work, make focused edits, and verify changes when practical. Follow every system, developer, sandbox, approval, and repository instruction supplied by the client.";

pub async fn fetch_catalog(
    gateway_url: &str,
    api_key: &str,
    client_version: &str,
) -> Result<Value> {
    let gateway = GatewayIdentity::parse(gateway_url)?;
    let source_label = gateway.source_label();
    let endpoint = models_endpoint(gateway_url, client_version)?;
    let client = network_proxy::configure_reqwest_builder(reqwest::Client::builder())?
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(35))
        .user_agent(format!(
            "znnz-agent-launcher/{} codex-cli/{client_version}",
            env!("CARGO_PKG_VERSION")
        ))
        .build()?;

    let request = client
        .get(endpoint.clone())
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header(ACCEPT, "application/json")
        .header(USER_AGENT, format!("codex-cli/{client_version}"));
    let response = network_proxy::send_with_network_retry(request)
        .await
        .map_err(|error| {
            anyhow!(
                "{} {}: {}{}{}",
                crate::i18n::tr("无法连接模型接口", "Unable to connect to model endpoint"),
                endpoint_origin(&endpoint),
                error,
                if crate::gateway::connection_error_hint(&gateway, &error).is_empty() {
                    ""
                } else {
                    ". "
                },
                crate::gateway::connection_error_hint(&gateway, &error)
            )
        })?;

    let status = response.status();
    let body = response.text().await.context(crate::i18n::tr(
        "读取模型接口响应失败",
        "Unable to read model endpoint response",
    ))?;
    if !status.is_success() {
        let short = body.chars().take(500).collect::<String>();
        bail!(
            "{} HTTP {}: {}",
            crate::i18n::tr("模型接口返回", "Model endpoint returned"),
            status.as_u16(),
            short
        );
    }

    let parsed: Value = serde_json::from_str(&body).context(crate::i18n::tr(
        "模型接口没有返回有效 JSON",
        "Model endpoint did not return valid JSON",
    ))?;
    let mut normalized = normalize_catalog(&parsed)?;
    apply_online_source_description(&mut normalized, &source_label)?;
    Ok(normalized)
}

pub fn read_catalog(path: &Path) -> Result<Value> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("无法读取模型目录 {}", path.display()))?;
    let parsed: Value = serde_json::from_str(&raw)
        .with_context(|| format!("模型目录不是有效 JSON: {}", path.display()))?;
    normalize_catalog(&parsed)
}

pub fn normalize_catalog(value: &Value) -> Result<Value> {
    let mut normalized = if value.get("models").and_then(Value::as_array).is_some() {
        json!({ "models": value["models"].clone() })
    } else {
        let data = value
            .get("data")
            .and_then(Value::as_array)
            .context("模型接口既没有 models 数组，也没有标准 OpenAI data 数组")?;
        let models = data
            .iter()
            .filter_map(|model| model.get("id").and_then(Value::as_str))
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .enumerate()
            .map(|(index, id)| generic_codex_model(id, index + 1))
            .collect::<Vec<_>>();
        json!({ "models": models })
    };
    normalize_agent_models(&mut normalized)?;
    validate_catalog(&normalized)?;
    Ok(normalized)
}

fn normalize_agent_models(value: &mut Value) -> Result<()> {
    let models = value
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .context("模型目录缺少 models 数组")?;
    for (index, model) in models.iter_mut().enumerate() {
        model
            .get("slug")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|slug| !slug.is_empty())
            .with_context(|| format!("第 {} 个模型缺少 slug", index + 1))?;
        let visible = model.get("visibility").and_then(Value::as_str) != Some("hide");
        let supported = model.get("supported_in_api").and_then(Value::as_bool) != Some(false);
        if !visible || !supported {
            continue;
        }

        let slug = model
            .get("slug")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let reasoning_level_count = model
            .get("supported_reasoning_levels")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        // Codex Desktop's bundled frontier models expose the extended
        // reasoning picker.  Gateways commonly return only low/medium/high;
        // keep their model catalog, but complete the capability metadata so
        // Desktop does not collapse the picker to "High".
        let needs_expanded_levels = supports_expanded_reasoning(slug) && reasoning_level_count < 6;
        let missing_levels = reasoning_level_count == 0;
        if missing_levels || needs_expanded_levels {
            model["supported_reasoning_levels"] = default_reasoning_levels_for_slug(slug);
        }
        let missing_default = model
            .get("default_reasoning_level")
            .and_then(Value::as_str)
            .map(str::trim)
            .is_none_or(str::is_empty);
        if missing_default {
            model["default_reasoning_level"] = Value::String("medium".to_owned());
        }
    }
    Ok(())
}

fn apply_online_source_description(value: &mut Value, source_label: &str) -> Result<()> {
    let models = value
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .context("模型目录缺少 models 数组")?;
    for model in models {
        let visible = model.get("visibility").and_then(Value::as_str) != Some("hide");
        let supported = model.get("supported_in_api").and_then(Value::as_bool) != Some(false);
        if visible && supported {
            model["description"] = Value::String(source_label.to_owned());
        }
    }
    Ok(())
}

fn default_reasoning_levels() -> Value {
    json!([
        {"effort": "low", "description": "Faster responses with lighter reasoning"},
        {"effort": "medium", "description": "Balanced speed and reasoning depth"},
        {"effort": "high", "description": "Deeper reasoning for complex coding tasks"}
    ])
}

fn expanded_reasoning_levels() -> Value {
    json!([
        {"effort": "low", "description": "Faster responses with lighter reasoning"},
        {"effort": "medium", "description": "Balanced speed and reasoning depth"},
        {"effort": "high", "description": "Deeper reasoning for complex coding tasks"},
        {"effort": "xhigh", "description": "Extra-deep reasoning for harder coding tasks"},
        {"effort": "max", "description": "Maximum reasoning for very hard problems"},
        {"effort": "ultra", "description": "Ultra reasoning for the most difficult tasks"}
    ])
}

fn default_reasoning_levels_for_slug(slug: &str) -> Value {
    if supports_expanded_reasoning(slug) {
        expanded_reasoning_levels()
    } else {
        default_reasoning_levels()
    }
}

fn supports_expanded_reasoning(slug: &str) -> bool {
    let lower = slug.to_ascii_lowercase();
    lower.starts_with("gpt-5")
        || lower.starts_with("gpt-6")
        || lower.starts_with("claude-")
        || lower.starts_with("anthropic-")
}

pub fn validate_catalog(value: &Value) -> Result<()> {
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .context("模型目录缺少 models 数组")?;
    if models.is_empty() {
        bail!("模型目录为空");
    }

    let mut seen = HashSet::new();
    let mut visible = 0usize;
    for (index, model) in models.iter().enumerate() {
        let slug = model
            .get("slug")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|slug| !slug.is_empty())
            .with_context(|| format!("第 {} 个模型缺少 slug", index + 1))?;
        if !seen.insert(slug.to_owned()) {
            bail!("模型目录包含重复 slug: {slug}");
        }
        let hidden = model.get("visibility").and_then(Value::as_str) == Some("hide");
        let supported = model.get("supported_in_api").and_then(Value::as_bool) != Some(false);
        if !hidden && supported {
            visible += 1;
        }
    }
    if visible == 0 {
        bail!("模型目录没有可见且受支持的模型");
    }
    Ok(())
}

pub fn visible_slugs(value: &Value) -> Vec<String> {
    value
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|model| model.get("visibility").and_then(Value::as_str) != Some("hide"))
        .filter(|model| model.get("supported_in_api").and_then(Value::as_bool) != Some(false))
        .filter_map(|model| model.get("slug").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

pub fn select_default_model(value: &Value, requested: Option<&str>) -> Result<String> {
    validate_catalog(value)?;
    let visible = visible_slugs(value)
        .into_iter()
        .filter(|model| is_conversational_model(model))
        .collect::<Vec<_>>();
    if visible.is_empty() {
        bail!("模型目录中没有可用的文本对话模型");
    }

    if let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) {
        if visible.iter().any(|model| model == requested) {
            return Ok(requested.to_owned());
        }
        bail!("指定的默认模型不在当前可见模型目录中: {requested}");
    }

    for preferred in ["gpt-5.6-sol", "gpt-5.5", "gpt-5.4", "claude-sonnet-5"] {
        if visible.iter().any(|model| model == preferred) {
            return Ok(preferred.to_owned());
        }
    }
    Ok(visible[0].clone())
}

fn generic_codex_model(id: &str, priority: usize) -> Value {
    json!({
        "slug": id,
        "display_name": id,
        "description": "From custom gateway",
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": default_reasoning_levels_for_slug(id),
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": priority,
        "additional_speed_tiers": [],
        "service_tiers": [],
        "default_service_tier": null,
        "availability_nux": null,
        "upgrade": null,
        "base_instructions": GENERIC_CODEX_INSTRUCTIONS,
        "model_messages": null,
        "include_skills_usage_instructions": false,
        "supports_reasoning_summary_parameter": false,
        "default_reasoning_summary": "none",
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": null,
        "web_search_tool_type": "text",
        "truncation_policy": { "mode": "tokens", "limit": 10000 },
        "supports_parallel_tool_calls": true,
        "supports_image_detail_original": false,
        "context_window": 128000,
        "max_context_window": 128000,
        "auto_compact_token_limit": null,
        "comp_hash": null,
        "effective_context_window_percent": 90,
        "experimental_supported_tools": [],
        "input_modalities": ["text", "image"],
        "supports_search_tool": false,
        "use_responses_lite": false,
        "auto_review_model_override": null,
        "tool_mode": "direct",
        "multi_agent_version": null
    })
}

fn is_conversational_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    ![
        "embedding",
        "embed-",
        "rerank",
        "moderation",
        "image",
        "imagen",
        "seedance",
        "sora",
        "veo",
        "whisper",
        "tts",
        "transcription",
        "speech",
    ]
    .iter()
    .any(|blocked| lower.contains(blocked))
}

fn models_endpoint(gateway_url: &str, client_version: &str) -> Result<Url> {
    let gateway = GatewayIdentity::parse(gateway_url)?;
    let mut base = Url::parse(gateway_url.trim()).context("网关地址无效")?;
    if base.scheme() != "https" && !is_loopback(&base) {
        bail!("网关必须使用 HTTPS；仅本机回环地址允许 HTTP");
    }
    base.set_query(None);
    base.set_fragment(None);
    let path = base.path().trim_end_matches('/');
    let next = if path.ends_with("/v1") || path == "v1" {
        format!("{path}/models")
    } else {
        format!("{path}/v1/models")
    };
    base.set_path(&next);
    if gateway.is_znnz {
        base.query_pairs_mut()
            .append_pair("client_version", client_version);
    }
    Ok(base)
}

fn is_loopback(url: &Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
}

fn endpoint_origin(url: &Url) -> String {
    format!(
        "{}://{}",
        url.scheme(),
        url.host_str().unwrap_or("<unknown>")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_catalog_and_filters_hidden_models() {
        let value = json!({
            "models": [
                {"slug": "visible", "visibility": "list", "supported_in_api": true},
                {"slug": "hidden", "visibility": "hide", "supported_in_api": true},
                {"slug": "unsupported", "visibility": "list", "supported_in_api": false}
            ]
        });
        validate_catalog(&value).unwrap();
        assert_eq!(visible_slugs(&value), vec!["visible"]);
    }

    #[test]
    fn normalizes_standard_openai_model_list() {
        let normalized = normalize_catalog(&json!({
            "object": "list",
            "data": [
                {"id": "gpt-custom", "object": "model"},
                {"id": "text-embedding-custom", "object": "model"}
            ]
        }))
        .unwrap();
        assert_eq!(
            visible_slugs(&normalized),
            vec!["gpt-custom", "text-embedding-custom"]
        );
        assert_eq!(
            select_default_model(&normalized, None).unwrap(),
            "gpt-custom"
        );
        assert_eq!(normalized["models"][0]["display_name"], "gpt-custom");
        assert_eq!(normalized["models"][0]["default_reasoning_level"], "medium");
        assert_eq!(
            normalized["models"][0]["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(normalized["models"][1]["default_reasoning_level"], "medium");
    }

    #[test]
    fn online_catalog_uses_normalized_gateway_address_as_model_source() {
        let mut normalized = normalize_catalog(&json!({
            "models": [
                {
                    "slug": "gpt-visible",
                    "description": "stale upstream description",
                    "visibility": "list",
                    "supported_in_api": true
                },
                {
                    "slug": "gpt-hidden",
                    "description": "keep hidden metadata",
                    "visibility": "hide",
                    "supported_in_api": true
                }
            ]
        }))
        .unwrap();
        let source = GatewayIdentity::parse("https://gateway.example.com/v1/?token=ignored#part")
            .unwrap()
            .source_label();
        apply_online_source_description(&mut normalized, &source).unwrap();
        assert_eq!(
            normalized["models"][0]["description"],
            "From https://gateway.example.com/v1"
        );
        assert_eq!(
            normalized["models"][1]["description"],
            "keep hidden metadata"
        );
    }

    #[test]
    fn repairs_empty_reasoning_levels_without_hiding_gateway_models() {
        let normalized = normalize_catalog(&json!({
            "models": [
                {
                    "slug": "deepseek-v4-pro",
                    "visibility": "list",
                    "supported_in_api": true,
                    "default_reasoning_level": null,
                    "supported_reasoning_levels": []
                },
                {
                    "slug": "seedance-2-0",
                    "visibility": "list",
                    "supported_in_api": true
                }
            ]
        }))
        .unwrap();
        assert_eq!(
            visible_slugs(&normalized),
            vec!["deepseek-v4-pro", "seedance-2-0"]
        );
        assert_eq!(normalized["models"][0]["default_reasoning_level"], "medium");
        assert_eq!(
            normalized["models"][0]["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(normalized["models"][1]["default_reasoning_level"], "medium");
    }

    #[test]
    fn frontier_models_receive_expanded_reasoning_levels() {
        let normalized = normalize_catalog(&json!({
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "visibility": "list",
                    "supported_in_api": true
                }
            ]
        }))
        .unwrap();
        let levels = normalized["models"][0]["supported_reasoning_levels"]
            .as_array()
            .unwrap();
        assert_eq!(levels.len(), 6);
        assert_eq!(levels[3]["effort"], "xhigh");
        assert_eq!(levels[4]["effort"], "max");
        assert_eq!(levels[5]["effort"], "ultra");
    }

    #[test]
    fn requested_default_model_must_be_visible() {
        let catalog = json!({"models": [{"slug": "model-a"}]});
        assert_eq!(
            select_default_model(&catalog, Some("model-a")).unwrap(),
            "model-a"
        );
        assert!(select_default_model(&catalog, Some("missing")).is_err());
    }

    #[test]
    fn rejects_duplicate_model_slugs() {
        let value = json!({
            "models": [
                {"slug": "same", "visibility": "list"},
                {"slug": "same", "visibility": "hide"}
            ]
        });
        let error = validate_catalog(&value).unwrap_err().to_string();
        assert!(error.contains("重复 slug"));
    }

    #[test]
    fn rejects_empty_or_entirely_hidden_catalogs() {
        assert!(validate_catalog(&json!({"models": []})).is_err());
        assert!(
            validate_catalog(&json!({
                "models": [{"slug": "hidden", "visibility": "hide"}]
            }))
            .is_err()
        );
    }

    #[test]
    fn models_endpoint_normalizes_v1_and_rejects_insecure_remote_http() {
        assert_eq!(
            models_endpoint("https://api.znnz.net", "1.2.3")
                .unwrap()
                .as_str(),
            "https://api.znnz.net/v1/models?client_version=1.2.3"
        );
        assert_eq!(
            models_endpoint("https://api.znnz.net/v1/", "1.2.3")
                .unwrap()
                .as_str(),
            "https://api.znnz.net/v1/models?client_version=1.2.3"
        );
        assert!(models_endpoint("http://api.znnz.net", "1.2.3").is_err());
        assert!(models_endpoint("http://127.0.0.1:1234", "1.2.3").is_ok());
        assert_eq!(
            models_endpoint("https://gateway.example.com/v1", "1.2.3")
                .unwrap()
                .as_str(),
            "https://gateway.example.com/v1/models"
        );
    }
}
