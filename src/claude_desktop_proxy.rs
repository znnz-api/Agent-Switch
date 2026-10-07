use crate::i18n;
use crate::network_proxy;
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures_util::TryStreamExt;
use http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_EXPOSE_HEADERS, AUTHORIZATION, HeaderValue,
};
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use rand::RngCore;
use reqwest::Client;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::convert::Infallible;
use std::error::Error as StdError;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::instrument::WithSubscriber;
use tracing::{info, warn};
use url::Url;
use zeroize::Zeroize;

const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;
const MAX_UPSTREAM_ERROR_BODY: usize = 8 * 1024 * 1024;
const MAX_LOG_ERROR_CHARS: usize = 800;
type BoxError = Box<dyn StdError + Send + Sync>;
type ProxyBody = UnsyncBoxBody<Bytes, BoxError>;

#[derive(Debug, Clone, Default)]
pub struct SlotOverrides {
    pub haiku: Option<String>,
    pub sonnet: Option<String>,
    pub opus: Option<String>,
    pub fable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSlots {
    pub haiku: String,
    pub sonnet: String,
    pub opus: String,
    pub fable: String,
}

impl ModelSlots {
    fn rewrite<'a>(&'a self, model: &'a str) -> &'a str {
        match model.trim().to_ascii_lowercase().as_str() {
            "haiku" | "claude-haiku" => &self.haiku,
            "sonnet" | "claude-sonnet" => &self.sonnet,
            "opus" | "claude-opus" => &self.opus,
            "fable" | "claude-fable" => &self.fable,
            _ => model,
        }
    }

    fn family_default(&self, tier: &str) -> &str {
        match tier {
            "haiku" => &self.haiku,
            "sonnet" => &self.sonnet,
            "opus" => &self.opus,
            "fable" => &self.fable,
            _ => "",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeDesktopModelMode {
    FourSlots,
    FullCatalog,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DesktopInferenceModel {
    /// Claude Desktop sees this stable, Anthropic-shaped route ID.
    pub name: String,
    /// The picker shows the exact model ID returned by the gateway.
    pub label_override: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anthropic_family_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_family_default: Option<bool>,
    /// The real gateway model ID. It is never serialized into managed config.
    #[serde(skip)]
    pub upstream_name: String,
}

#[derive(Debug, Clone)]
pub struct DesktopModelMenu {
    mode: ClaudeDesktopModelMode,
    models: Vec<DesktopInferenceModel>,
    source_label: String,
}

impl DesktopModelMenu {
    pub fn four_slots() -> Self {
        Self {
            mode: ClaudeDesktopModelMode::FourSlots,
            models: Vec::new(),
            source_label: String::new(),
        }
    }

    pub fn full_catalog_with_source(
        catalog: &Value,
        slots: &ModelSlots,
        source_label: impl Into<String>,
    ) -> Result<Self> {
        crate::catalog::validate_catalog(catalog)?;
        let mut upstream_names = crate::catalog::visible_slugs(catalog);
        promote_claude_family(&mut upstream_names, slots);
        let models = upstream_names
            .into_iter()
            .map(|upstream_name| {
                let tier = anthropic_family_tier(&upstream_name).map(str::to_owned);
                let is_family_default = tier
                    .as_deref()
                    .map(|tier| slots.family_default(tier) == upstream_name);
                DesktopInferenceModel {
                    name: desktop_route_id(&upstream_name),
                    label_override: upstream_name.clone(),
                    anthropic_family_tier: tier,
                    is_family_default,
                    upstream_name,
                }
            })
            .collect::<Vec<_>>();
        if models.is_empty() {
            bail!("网关模型目录中没有可显示的模型");
        }
        Ok(Self {
            mode: ClaudeDesktopModelMode::FullCatalog,
            models,
            source_label: source_label.into(),
        })
    }

    pub fn claude_code_catalog(catalog: &Value, source_label: impl Into<String>) -> Result<Self> {
        crate::catalog::validate_catalog(catalog)?;
        // Claude Code's `modelPicker.replaceBuiltInOptions` makes this list
        // the sole source of selectable non-default rows. Do not remove the
        // four family models here: in gateway mode every visible gateway
        // model must be represented exactly once, including Claude models.
        let mut upstream_names = crate::catalog::visible_slugs(catalog);
        group_gateway_models_by_family(&mut upstream_names);
        let models = upstream_names
            .into_iter()
            .map(|upstream_name| DesktopInferenceModel {
                // Claude Code 2.1.237+ discards gateway model IDs that do not
                // contain `claude` or `anthropic`. The loopback proxy exposes a
                // stable Claude-shaped route while preserving the real ID as
                // the visible label and rewrites the route before forwarding.
                name: desktop_route_id(&upstream_name),
                label_override: upstream_name.clone(),
                // Keep gateway Claude IDs as raw IDs. Claude Code's native
                // picker entries are rendered above these gateway rows.
                anthropic_family_tier: None,
                is_family_default: None,
                upstream_name,
            })
            .collect();
        Ok(Self {
            mode: ClaudeDesktopModelMode::FullCatalog,
            models,
            source_label: source_label.into(),
        })
    }

    /// Build Claude Code's curated picker configuration. With
    /// `replaceBuiltInOptions=true`, Claude Code keeps its Default row and
    /// displays exactly these gateway rows. The picker uses the same stable
    /// loopback route IDs as the local proxy, while labels remain the real
    /// gateway model IDs shown to the user.
    pub fn claude_code_model_picker(&self) -> Value {
        json!({
            "replaceBuiltInOptions": true,
            "options": self.models.iter().map(|model| json!({
                "model": model.name,
                "label": model.label_override,
                "description": self.source_label,
            })).collect::<Vec<_>>()
        })
    }

    pub fn mode(&self) -> ClaudeDesktopModelMode {
        self.mode
    }

    pub fn with_hidden_models(&self, hidden_models: &[String]) -> Self {
        let mut menu = self.clone();
        if menu.mode == ClaudeDesktopModelMode::FullCatalog {
            menu.models
                .retain(|model| !hidden_models.contains(&model.upstream_name));
        }
        menu
    }

    pub fn model_count(&self) -> usize {
        match self.mode {
            ClaudeDesktopModelMode::FourSlots => 4,
            ClaudeDesktopModelMode::FullCatalog => self.models.len(),
        }
    }

    pub fn registry_models_json(&self) -> Result<String> {
        match self.mode {
            ClaudeDesktopModelMode::FourSlots => {
                Ok(r#"["haiku","sonnet","opus","fable"]"#.to_owned())
            }
            ClaudeDesktopModelMode::FullCatalog => serde_json::to_string(&self.models)
                .context("无法序列化 Claude Desktop 完整模型列表"),
        }
    }

    pub fn aliased_model_count(&self) -> usize {
        self.models
            .iter()
            .filter(|model| model.name != model.upstream_name)
            .count()
    }

    pub fn default_model_name(&self) -> Option<&str> {
        self.models
            .first()
            .map(|model| model.upstream_name.as_str())
    }

    fn model_routes(&self) -> HashMap<String, String> {
        self.models
            .iter()
            .map(|model| (model.name.clone(), model.upstream_name.clone()))
            .collect()
    }

    pub fn models_response(&self, slots: &ModelSlots) -> Value {
        let data = match self.mode {
            ClaudeDesktopModelMode::FourSlots => vec![
                model_entry(&slots.haiku, &slots.haiku, Some("haiku"), true, None),
                model_entry(&slots.sonnet, &slots.sonnet, Some("sonnet"), true, None),
                model_entry(&slots.opus, &slots.opus, Some("opus"), true, None),
                model_entry(&slots.fable, &slots.fable, Some("fable"), true, None),
            ],
            ClaudeDesktopModelMode::FullCatalog => self
                .models
                .iter()
                .map(|model| {
                    model_entry(
                        &model.name,
                        &model.label_override,
                        model.anthropic_family_tier.as_deref(),
                        model.is_family_default.unwrap_or(false),
                        Some(self.source_label.as_str()),
                    )
                })
                .collect(),
        };
        models_page(data)
    }
}

fn anthropic_family_tier(model: &str) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    ["haiku", "sonnet", "opus", "fable", "mythos"]
        .into_iter()
        .find(|tier| lower.contains(&format!("claude-{tier}")))
}

fn group_gateway_models_by_family(models: &mut [String]) {
    // Stable sorting keeps the gateway's order within each family while
    // preventing models from the same provider family from being scattered.
    models.sort_by_key(|model| gateway_model_family_rank(model));
}

fn gateway_model_family_rank(model: &str) -> u8 {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("claude-") || lower.starts_with("anthropic-") {
        0
    } else if lower.starts_with("gpt-") || lower.starts_with("o1-") || lower.starts_with("o3-") {
        1
    } else if lower.starts_with("gemini-") {
        2
    } else if lower.starts_with("grok-") {
        3
    } else if lower.starts_with("deepseek-") {
        4
    } else {
        5
    }
}

fn promote_claude_family(models: &mut Vec<String>, slots: &ModelSlots) {
    let preferred = [
        "claude-opus-4-8",
        slots.opus.as_str(),
        "claude-fable-5",
        slots.fable.as_str(),
        slots.sonnet.as_str(),
        slots.haiku.as_str(),
    ];

    let mut claude_models = Vec::with_capacity(models.len());
    let mut other_models = Vec::with_capacity(models.len());
    for model in models.drain(..) {
        if anthropic_family_tier(&model).is_some() {
            claude_models.push(model);
        } else {
            other_models.push(model);
        }
    }

    let preferred_index = preferred
        .into_iter()
        .filter(|candidate| candidate.to_ascii_lowercase().starts_with("claude-"))
        .find_map(|candidate| claude_models.iter().position(|model| model == candidate))
        .or_else(|| (!claude_models.is_empty()).then_some(0));
    if let Some(index) = preferred_index {
        let preferred_model = claude_models.remove(index);
        claude_models.insert(0, preferred_model);
    }

    claude_models.extend(other_models);
    *models = claude_models;
}

fn desktop_route_id(upstream_model: &str) -> String {
    let digest = Sha256::digest(upstream_model.as_bytes());
    let suffix = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("claude-gateway-route-{suffix}")
}

fn models_page(data: Vec<Value>) -> Value {
    let first_id = data
        .first()
        .and_then(|model| model.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    let last_id = data
        .last()
        .and_then(|model| model.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "data": data,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id
    })
}

fn model_entry(
    id: &str,
    display_name: &str,
    tier: Option<&str>,
    is_family_default: bool,
    description: Option<&str>,
) -> Value {
    let mut entry = json!({
        "id": id,
        "type": "model",
        "display_name": display_name,
        "created_at": "2026-01-01T00:00:00Z"
    });
    if let Some(description) = description {
        entry["description"] = Value::String(description.to_owned());
    }
    if let Some(tier) = tier {
        entry["anthropic_family_tier"] = Value::String(tier.to_owned());
        entry["is_family_default"] = Value::Bool(is_family_default);
    }
    entry
}

pub fn select_model_slots(catalog: &Value, overrides: &SlotOverrides) -> Result<ModelSlots> {
    crate::catalog::validate_catalog(catalog)?;
    let visible = crate::catalog::visible_slugs(catalog)
        .into_iter()
        .filter(|model| is_conversational_model(model))
        .collect::<Vec<_>>();
    if visible.is_empty() {
        bail!("网关模型目录中没有适合 Claude Desktop 的文本对话模型");
    }

    let haiku =
        resolve_override(&visible, overrides.haiku.as_deref(), "haiku")?.unwrap_or_else(|| {
            pick_last(&visible, &["claude-haiku"])
                .or_else(|| pick_last(&visible, &["gemini", "flash"]))
                .or_else(|| pick_last(&visible, &["mini"]))
                .or_else(|| pick_last(&visible, &["luna"]))
                .or_else(|| pick_last(&visible, &["fast"]))
                .or_else(|| pick_last(&visible, &["lite"]))
                .unwrap_or_else(|| visible[0].clone())
        });
    let sonnet =
        resolve_override(&visible, overrides.sonnet.as_deref(), "sonnet")?.unwrap_or_else(|| {
            pick_last(&visible, &["claude-sonnet"])
                .or_else(|| pick_last(&visible, &["claude-fable"]))
                .or_else(|| pick_exact(&visible, "gpt-5.6-sol"))
                .unwrap_or_else(|| visible[0].clone())
        });
    let opus =
        resolve_override(&visible, overrides.opus.as_deref(), "opus")?.unwrap_or_else(|| {
            pick_last(&visible, &["claude-opus"])
                .or_else(|| pick_exact(&visible, "gpt-5.6-sol-max"))
                .or_else(|| pick_exact(&visible, "gpt-5.6-sol"))
                .unwrap_or_else(|| sonnet.clone())
        });
    let fable =
        resolve_override(&visible, overrides.fable.as_deref(), "fable")?.unwrap_or_else(|| {
            pick_last(&visible, &["claude-fable"])
                .or_else(|| pick_last(&visible, &["claude-sonnet"]))
                .unwrap_or_else(|| sonnet.clone())
        });

    Ok(ModelSlots {
        haiku,
        sonnet,
        opus,
        fable,
    })
}

/// Select Claude Code's four built-in slots without disguising an unrelated
/// model family as Opus, Fable, Sonnet, or Haiku. If the key cannot see a
/// matching Claude family, keep that slot pointed at its canonical Claude
/// model ID; selecting it will then produce the gateway's real unavailable-
/// model response instead of silently running (for example) GPT Mini.
pub fn select_claude_code_model_slots(catalog: &Value) -> Result<ModelSlots> {
    crate::catalog::validate_catalog(catalog)?;
    let visible = crate::catalog::visible_slugs(catalog)
        .into_iter()
        .filter(|model| is_conversational_model(model))
        .collect::<Vec<_>>();
    if visible.is_empty() {
        bail!("网关模型目录中没有可用于 Claude Code 的文本对话模型");
    }

    Ok(ModelSlots {
        haiku: pick_last(&visible, &["claude-haiku"])
            .unwrap_or_else(|| "claude-haiku-4-5-20251001".to_owned()),
        sonnet: pick_last(&visible, &["claude-sonnet"])
            .unwrap_or_else(|| "claude-sonnet-5".to_owned()),
        opus: pick_last(&visible, &["claude-opus"]).unwrap_or_else(|| "claude-opus-5".to_owned()),
        fable: pick_last(&visible, &["claude-fable"])
            .unwrap_or_else(|| "claude-fable-5".to_owned()),
    })
}

fn resolve_override(visible: &[String], value: Option<&str>, slot: &str) -> Result<Option<String>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if !visible.iter().any(|model| model == value) {
        bail!("--{slot}-model 指定的模型不在当前可用文本模型目录中: {value}");
    }
    Ok(Some(value.to_owned()))
}

fn pick_last(visible: &[String], required_fragments: &[&str]) -> Option<String> {
    visible
        .iter()
        .rev()
        .find(|model| {
            let lower = model.to_ascii_lowercase();
            required_fragments
                .iter()
                .all(|fragment| lower.contains(fragment))
        })
        .cloned()
}

fn pick_exact(visible: &[String], expected: &str) -> Option<String> {
    visible.iter().find(|model| *model == expected).cloned()
}

fn is_conversational_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    ![
        "embedding",
        "rerank",
        "image",
        "seedance",
        "sora",
        "veo",
        "whisper",
        "tts",
        "moderation",
    ]
    .iter()
    .any(|blocked| lower.contains(blocked))
}

struct Secret(String);

impl Secret {
    fn new(value: String) -> Self {
        Self(value)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

struct ProxyState {
    gateway: Url,
    upstream_key: Secret,
    local_token: Secret,
    slots: ModelSlots,
    model_routes: HashMap<String, String>,
    models_response: Value,
    client: Client,
    request_sequence: AtomicU64,
}

pub struct ClaudeDesktopProxy {
    address: SocketAddr,
    state: Arc<ProxyState>,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl ClaudeDesktopProxy {
    pub async fn start(
        gateway_url: &str,
        api_key: String,
        slots: ModelSlots,
        menu: DesktopModelMenu,
    ) -> Result<Self> {
        let gateway = normalize_gateway_url(gateway_url)?;
        let model_routes = menu.model_routes();
        let models_response = menu.models_response(&slots);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("无法启动 Claude Desktop 本地代理")?;
        let address = listener.local_addr()?;
        if !address.ip().is_loopback() {
            bail!("安全检查失败：Claude Desktop 代理没有绑定到回环地址");
        }
        let client = network_proxy::builder_for_url(Client::builder(), gateway_url)?
            .connect_timeout(Duration::from_secs(12))
            .pool_idle_timeout(Duration::from_secs(30))
            .user_agent(format!(
                "znnz-client/{} claude-desktop-proxy",
                env!("CARGO_PKG_VERSION")
            ))
            .build()?;
        let state = Arc::new(ProxyState {
            gateway,
            upstream_key: Secret::new(api_key),
            local_token: Secret::new(random_token()),
            slots,
            model_routes,
            models_response,
            client,
            request_sequence: AtomicU64::new(1),
        });
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let task_state = state.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, peer)) if peer.ip().is_loopback() => {
                                let state = task_state.clone();
                                tokio::spawn(async move {
                                    let io = TokioIo::new(stream);
                                    let service = service_fn(move |request| {
                                        let state = state.clone();
                                        async move { Ok::<_, Infallible>(handle_request(request, state).await) }
                                    });
                                    if let Err(error) = hyper::server::conn::http1::Builder::new()
                                        .keep_alive(true)
                                        .serve_connection(io, service)
                                        .await
                                    {
                                        if i18n::language() == i18n::Language::ZhCn {
                                            warn!("Claude Desktop 本地代理连接失败: {error}");
                                        } else {
                                            warn!("Claude Desktop proxy connection failed: {error}");
                                        }
                                    }
                                }.with_current_subscriber());
                            }
                            Ok((_stream, peer)) => {
                                if i18n::language() == i18n::Language::ZhCn {
                                    warn!("拒绝非回环 Claude Desktop 代理请求: {peer}");
                                } else {
                                    warn!("Rejected non-loopback Claude Desktop proxy request: {peer}");
                                }
                            }
                            Err(error) => {
                                if i18n::language() == i18n::Language::ZhCn {
                                    warn!("Claude Desktop 本地代理 accept 失败: {error}");
                                } else {
                                    warn!("Claude Desktop proxy accept failed: {error}");
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }.with_current_subscriber());
        Ok(Self {
            address,
            state,
            shutdown,
            task: Some(task),
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.address.port())
    }

    pub fn local_token(&self) -> &str {
        self.state.local_token.expose()
    }

    pub async fn stop(mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for ClaudeDesktopProxy {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn handle_request(request: Request<Incoming>, state: Arc<ProxyState>) -> Response<ProxyBody> {
    if request.method() == Method::OPTIONS {
        let requested_headers = request
            .headers()
            .get("access-control-request-headers")
            .cloned();
        return cors_response(
            empty_response(StatusCode::NO_CONTENT),
            requested_headers.as_ref(),
        );
    }
    if !authorized(request.headers(), state.local_token.expose()) {
        return cors_response(json_error(StatusCode::UNAUTHORIZED, "unauthorized"), None);
    }

    let path = request.uri().path();
    if request.method() == Method::GET && path == "/health" {
        return cors_response(json_response(StatusCode::OK, json!({"ok": true})), None);
    }
    if request.method() == Method::GET && path == "/v1/models" {
        let count = state
            .models_response
            .get("data")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if i18n::language() == i18n::Language::ZhCn {
            info!("Claude Desktop 已请求 /v1/models：返回 {count} 个模型");
        } else {
            info!("Claude Desktop requested /v1/models: returned {count} models");
        }
        return cors_response(
            json_response(StatusCode::OK, state.models_response.clone()),
            None,
        );
    }
    if !path.starts_with("/v1/") {
        return cors_response(json_error(StatusCode::NOT_FOUND, "not_found"), None);
    }

    match forward_request(request, &state).await {
        Ok(response) => cors_response(response, None),
        Err(error) => {
            if i18n::language() == i18n::Language::ZhCn {
                warn!("Claude Desktop 上游请求失败: {error:#}");
            } else {
                warn!(
                    "Claude Desktop upstream request failed: {}",
                    i18n::runtime_error(&error)
                );
            }
            cors_response(
                json_error(StatusCode::BAD_GATEWAY, "upstream_request_failed"),
                None,
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelRewriteInfo {
    route: Option<String>,
    upstream: Option<String>,
    state: &'static str,
}

impl ModelRewriteInfo {
    fn missing() -> Self {
        Self {
            route: None,
            upstream: None,
            state: "missing",
        }
    }

    fn non_string(kind: &'static str) -> Self {
        Self {
            route: None,
            upstream: None,
            state: kind,
        }
    }

    fn unchanged(model: &str) -> Self {
        Self {
            route: Some(model.to_owned()),
            upstream: Some(model.to_owned()),
            state: "unchanged",
        }
    }

    fn rewritten(route: &str, upstream: &str) -> Self {
        Self {
            route: Some(route.to_owned()),
            upstream: Some(upstream.to_owned()),
            state: "rewritten",
        }
    }

    fn route_for_log(&self) -> String {
        self.route
            .as_deref()
            .map(|value| safe_log_value(value, 160))
            .unwrap_or_else(|| format!("<{}>", self.state))
    }

    fn upstream_for_log(&self) -> String {
        self.upstream
            .as_deref()
            .map(|value| safe_log_value(value, 160))
            .unwrap_or_else(|| format!("<{}>", self.state))
    }
}

async fn forward_request(
    request: Request<Incoming>,
    state: &ProxyState,
) -> Result<Response<ProxyBody>> {
    let active = crate::local_gateway::track_adapter_request(&state.gateway);
    let request_id = state.request_sequence.fetch_add(1, Ordering::Relaxed);
    let started = Instant::now();
    let (parts, body) = request.into_parts();
    let method = parts.method.clone();
    let path = parts.uri.path().to_owned();
    let endpoint = upstream_url(&state.gateway, &parts.uri)?;
    let collected = Limited::new(body, MAX_REQUEST_BODY)
        .collect()
        .await
        .map_err(|error| {
            anyhow::anyhow!("Claude Desktop request body read failed or exceeded 64 MiB: {error}")
        })?;
    let mut bytes = collected.to_bytes().to_vec();
    let mut model_info = ModelRewriteInfo::missing();
    let mut stream = None;
    let request_log_label = i18n::tr("[Claude代理请求]", "[Claude proxy request]");
    let response_log_label = i18n::tr("[Claude代理响应]", "[Claude proxy response]");
    let json_candidate =
        !bytes.is_empty() && (request_is_json(&parts.headers) || looks_like_json_object(&bytes));
    let json_state = if json_candidate {
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(mut value) => {
                stream = value.get("stream").and_then(Value::as_bool);
                model_info = rewrite_model(&mut value, &state.slots, &state.model_routes);
                bytes = serde_json::to_vec(&value)?;
                "parsed"
            }
            Err(error) => {
                warn!(
                    "{request_log_label} id={request_id} {method} {path} JSON parse failed (request content omitted): {error}"
                );
                "invalid"
            }
        }
    } else {
        "not-json"
    };

    info!(
        "{request_log_label} id={request_id} {method} {path} modelRoute={} model={} rewrite={} stream={} bodyBytes={} json={json_state}",
        model_info.route_for_log(),
        model_info.upstream_for_log(),
        model_info.state,
        stream.map_or("unknown", |value| if value { "1" } else { "0" }),
        bytes.len()
    );

    let mut upstream = state.client.request(method, endpoint);
    for (name, value) in &parts.headers {
        if is_hop_or_secret_header(name.as_str()) {
            continue;
        }
        upstream = upstream.header(name, value);
    }
    upstream = upstream
        // Error bodies are inspected for logging; keep their bytes readable.
        .header(http::header::ACCEPT_ENCODING, "identity")
        .header(
            AUTHORIZATION,
            format!("Bearer {}", state.upstream_key.expose()),
        )
        .header("x-api-key", state.upstream_key.expose())
        .body(bytes);

    let response = upstream
        .send()
        .await
        .context("unable to connect to upstream gateway")?;
    let status = response.status();
    let headers = response.headers().clone();
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| safe_log_value(value, 120))
        .unwrap_or_else(|| "<none>".to_owned());
    let elapsed_ms = started.elapsed().as_millis();

    if !status.is_success() {
        if response
            .content_length()
            .is_some_and(|length| length > MAX_UPSTREAM_ERROR_BODY as u64)
        {
            warn!(
                "{response_log_label} id={request_id} model={} status={} durationMs={elapsed_ms} contentType={content_type} error=<body exceeded {} MiB safety limit>",
                model_info.upstream_for_log(),
                status.as_u16(),
                MAX_UPSTREAM_ERROR_BODY / 1024 / 1024
            );
            bail!("upstream error response exceeded safety limit");
        }
        let error_body = response
            .bytes()
            .await
            .context("unable to read upstream error response")?;
        if error_body.len() > MAX_UPSTREAM_ERROR_BODY {
            warn!(
                "{response_log_label} id={request_id} model={} status={} durationMs={elapsed_ms} contentType={content_type} error=<body exceeded {} MiB safety limit>",
                model_info.upstream_for_log(),
                status.as_u16(),
                MAX_UPSTREAM_ERROR_BODY / 1024 / 1024
            );
            bail!("upstream error response exceeded safety limit");
        }
        let summary = summarize_upstream_error(&error_body, state.upstream_key.expose());
        warn!(
            "{response_log_label} id={request_id} model={} status={} durationMs={elapsed_ms} contentType={content_type} error={summary}",
            model_info.upstream_for_log(),
            status.as_u16()
        );
        let mut output = Response::builder().status(status);
        if let Some(target_headers) = output.headers_mut() {
            copy_response_headers(&headers, target_headers);
        }
        return output
            .body(full_body(error_body))
            .context("unable to construct Claude Desktop upstream error response");
    }

    info!(
        "{response_log_label} id={request_id} model={} status={} durationMs={elapsed_ms} contentType={content_type}",
        model_info.upstream_for_log(),
        status.as_u16()
    );
    let stream = response
        .bytes_stream()
        .map_ok(move |bytes| {
            let _keep_active_until_body_drops = &active;
            Frame::data(bytes)
        })
        .map_err(|error| -> BoxError { Box::new(error) });
    let body = StreamBody::new(stream).boxed_unsync();
    let mut output = Response::builder().status(status);
    if let Some(target_headers) = output.headers_mut() {
        copy_response_headers(&headers, target_headers);
    }
    output
        .body(body)
        .context("unable to construct Claude Desktop proxy response")
}

fn rewrite_model(
    value: &mut Value,
    slots: &ModelSlots,
    model_routes: &HashMap<String, String>,
) -> ModelRewriteInfo {
    let Some(model) = value.get_mut("model") else {
        return ModelRewriteInfo::missing();
    };
    let Some(current) = model.as_str() else {
        return ModelRewriteInfo::non_string(json_kind(model));
    };
    let rewritten = model_routes
        .get(current)
        .map(String::as_str)
        .unwrap_or_else(|| slots.rewrite(current));
    let info = if rewritten != current {
        ModelRewriteInfo::rewritten(current, rewritten)
    } else {
        ModelRewriteInfo::unchanged(current)
    };
    if rewritten != current {
        *model = Value::String(rewritten.to_owned());
    }
    info
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "model-null",
        Value::Bool(_) => "model-bool",
        Value::Number(_) => "model-number",
        Value::String(_) => "model-string",
        Value::Array(_) => "model-array",
        Value::Object(_) => "model-object",
    }
}

fn looks_like_json_object(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(b'{')
}

fn summarize_upstream_error(body: &[u8], upstream_key: &str) -> String {
    summarize_upstream_error_for_language(body, upstream_key, i18n::language())
}

fn summarize_upstream_error_for_language(
    body: &[u8],
    upstream_key: &str,
    language: i18n::Language,
) -> String {
    let selected = match serde_json::from_slice::<Value>(body) {
        Ok(value) => collect_error_fields(&value),
        Err(_) => match std::str::from_utf8(body) {
            Ok(text)
                if !text.trim().is_empty()
                    && !text.chars().any(|c| c.is_control() && !c.is_whitespace()) =>
            {
                text.to_owned()
            }
            _ => language
                .text(
                    "上游返回了非文本错误内容",
                    "Upstream returned a non-text error response",
                )
                .to_owned(),
        },
    };
    let redacted = redact_sensitive(&selected, upstream_key);
    safe_log_value(&redacted, MAX_LOG_ERROR_CHARS)
}

fn collect_error_fields(value: &Value) -> String {
    let mut fields = Vec::new();
    for (label, pointer) in [
        ("type", "/error/type"),
        ("code", "/error/code"),
        ("message", "/error/message"),
        ("message", "/message"),
        ("detail", "/detail"),
        ("type", "/type"),
        ("code", "/code"),
    ] {
        if let Some(field) = value.pointer(pointer) {
            let rendered = match field {
                Value::String(text) => text.clone(),
                Value::Null => continue,
                other => other.to_string(),
            };
            let item = format!("{label}={rendered}");
            if !fields.contains(&item) {
                fields.push(item);
            }
        }
    }
    if fields.is_empty() {
        value.to_string()
    } else {
        fields.join(" | ")
    }
}

fn redact_sensitive(value: &str, upstream_key: &str) -> String {
    let mut redacted = if upstream_key.is_empty() {
        value.to_owned()
    } else {
        value.replace(upstream_key, "[REDACTED]")
    };
    redact_prefixed_token(&mut redacted, "sk-", false);
    redact_prefixed_token(&mut redacted, "Bearer ", true);
    redacted
}

fn redact_prefixed_token(value: &mut String, prefix: &str, keep_prefix: bool) {
    let mut search_from = 0usize;
    while let Some(relative) = value[search_from..].find(prefix) {
        let start = search_from + relative;
        let token_start = start + prefix.len();
        let token_end = value[token_start..]
            .find(|character: char| {
                character.is_whitespace()
                    || matches!(
                        character,
                        '"' | '\'' | '\u{60}' | ',' | ';' | ')' | ']' | '}'
                    )
            })
            .map(|offset| token_start + offset)
            .unwrap_or(value.len());
        let replacement = if keep_prefix {
            format!("{prefix}[REDACTED]")
        } else {
            "[REDACTED]".to_owned()
        };
        value.replace_range(start..token_end, &replacement);
        search_from = start + replacement.len();
    }
}

fn safe_log_value(value: &str, max_chars: usize) -> String {
    let collapsed = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut output = collapsed.chars().take(max_chars).collect::<String>();
    if collapsed.chars().count() > max_chars {
        output.push('\u{2026}');
    }
    if output.is_empty() {
        "<empty>".to_owned()
    } else {
        output
    }
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok());
    bearer
        .into_iter()
        .chain(api_key)
        .any(|supplied| constant_time_equal(supplied.as_bytes(), expected.as_bytes()))
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

fn request_is_json(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("json"))
}

fn is_hop_or_secret_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "x-api-key"
            | "host"
            | "content-length"
            | "accept-encoding"
            | "connection"
            | "transfer-encoding"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "keep-alive"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn copy_response_headers(source: &HeaderMap, target: &mut HeaderMap) {
    for (name, value) in source {
        if matches!(
            name.as_str().to_ascii_lowercase().as_str(),
            "content-length"
                | "connection"
                | "transfer-encoding"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "upgrade"
        ) {
            continue;
        }
        target.append(name, value.clone());
    }
}

fn upstream_url(base: &Url, uri: &Uri) -> Result<Url> {
    let mut target = base.clone();
    let incoming = uri.path();
    let path = crate::gateway::upstream_api_path(base, incoming);
    target.set_path(&path);
    target.set_query(uri.query());
    Ok(target)
}

fn normalize_gateway_url(value: &str) -> Result<Url> {
    let mut url = Url::parse(value.trim()).context("网关地址无效")?;
    if url.scheme() != "https" && !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
    {
        bail!("网关必须使用 HTTPS；仅本机回环地址允许 HTTP");
    }
    if url.host_str().is_none() {
        bail!("网关地址缺少主机名");
    }
    url.set_query(None);
    url.set_fragment(None);
    let normalized_path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&normalized_path);
    Ok(url)
}

fn cors_response(
    mut response: Response<ProxyBody>,
    requested_headers: Option<&HeaderValue>,
) -> Response<ProxyBody> {
    let headers = response.headers_mut();
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    headers.insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        requested_headers.cloned().unwrap_or_else(|| {
            HeaderValue::from_static(
                "authorization, x-api-key, anthropic-version, anthropic-beta, content-type, accept",
            )
        }),
    );
    headers.insert(
        ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("request-id, x-request-id"),
    );
    headers.insert(
        "access-control-allow-private-network",
        HeaderValue::from_static("true"),
    );
    response
}

fn json_response(status: StatusCode, value: Value) -> Response<ProxyBody> {
    let bytes = serde_json::to_vec(&value)
        .unwrap_or_else(|_| br#"{"error":"serialization_failed"}"#.to_vec());
    Response::builder()
        .status(status)
        .header(
            http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )
        .header(http::header::CACHE_CONTROL, "no-store")
        .body(full_body(Bytes::from(bytes)))
        .expect("static local response is valid")
}

fn json_error(status: StatusCode, code: &str) -> Response<ProxyBody> {
    json_response(status, json!({"error": {"type": code, "message": code}}))
}

fn empty_response(status: StatusCode) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .body(full_body(Bytes::new()))
        .expect("static empty response is valid")
}

fn full_body(bytes: Bytes) -> ProxyBody {
    Full::new(bytes)
        .map_err(|never: Infallible| match never {})
        .boxed_unsync()
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Value {
        json!({
            "models": [
                {"slug":"gpt-5.6-sol","visibility":"list","supported_in_api":true},
                {"slug":"gpt-5.6-sol-max","visibility":"list","supported_in_api":true},
                {"slug":"claude-fable-5","visibility":"list","supported_in_api":true},
                {"slug":"claude-haiku-4-5-20251001","visibility":"list","supported_in_api":true},
                {"slug":"claude-opus-4-6","visibility":"list","supported_in_api":true},
                {"slug":"claude-opus-4-8","visibility":"list","supported_in_api":true},
                {"slug":"claude-sonnet-4-6","visibility":"list","supported_in_api":true},
                {"slug":"claude-sonnet-5","visibility":"list","supported_in_api":true},
                {"slug":"gemini-3-pro-image","visibility":"list","supported_in_api":true},
                {"slug":"seedance-2-0","visibility":"list","supported_in_api":true}
            ]
        })
    }

    #[test]
    fn selects_latest_anthropic_family_models_and_filters_media_models() {
        let slots = select_model_slots(&catalog(), &SlotOverrides::default()).unwrap();
        assert_eq!(slots.haiku, "claude-haiku-4-5-20251001");
        assert_eq!(slots.sonnet, "claude-sonnet-5");
        assert_eq!(slots.opus, "claude-opus-4-8");
        assert_eq!(slots.fable, "claude-fable-5");
    }

    #[test]
    fn claude_code_slots_never_fall_back_to_an_unrelated_model_family() {
        let only_gpt = json!({
            "models": [{
                "slug": "gpt-5.4-mini",
                "visibility": "list",
                "supported_in_api": true
            }]
        });
        let slots = select_claude_code_model_slots(&only_gpt).unwrap();
        assert_eq!(slots.opus, "claude-opus-5");
        assert_eq!(slots.fable, "claude-fable-5");
        assert_eq!(slots.sonnet, "claude-sonnet-5");
        assert_eq!(slots.haiku, "claude-haiku-4-5-20251001");
        assert!(
            [slots.opus, slots.fable, slots.sonnet, slots.haiku]
                .iter()
                .all(|model| model != "gpt-5.4-mini")
        );
    }

    #[test]
    fn explicit_overrides_must_be_visible_conversational_models() {
        let overrides = SlotOverrides {
            opus: Some("gpt-5.6-sol-max".into()),
            ..Default::default()
        };
        assert_eq!(
            select_model_slots(&catalog(), &overrides).unwrap().opus,
            "gpt-5.6-sol-max"
        );
        let invalid = SlotOverrides {
            opus: Some("seedance-2-0".into()),
            ..Default::default()
        };
        assert!(select_model_slots(&catalog(), &invalid).is_err());
    }

    #[test]
    fn slot_and_full_catalog_routes_are_rewritten_to_real_models() {
        let value = catalog();
        let slots = select_model_slots(&value, &SlotOverrides::default()).unwrap();
        let mut alias = json!({"model":"haiku","messages":[]});
        rewrite_model(&mut alias, &slots, &HashMap::new());
        assert_eq!(alias["model"], slots.haiku);

        let menu =
            DesktopModelMenu::full_catalog_with_source(&value, &slots, "From gateway").unwrap();
        let route = menu
            .models
            .iter()
            .find(|model| model.upstream_name == "gpt-5.6-sol-max")
            .unwrap()
            .name
            .clone();
        let mut routed = json!({"model":route,"messages":[]});
        let rewrite = rewrite_model(&mut routed, &slots, &menu.model_routes());
        assert_eq!(routed["model"], "gpt-5.6-sol-max");
        assert_eq!(rewrite.state, "rewritten");
        assert_eq!(rewrite.upstream.as_deref(), Some("gpt-5.6-sol-max"));

        let mut real = json!({"model":"gpt-5.6-sol-max","messages":[]});
        rewrite_model(&mut real, &slots, &menu.model_routes());
        assert_eq!(real["model"], "gpt-5.6-sol-max");
    }

    #[test]
    fn four_slot_models_response_uses_claude_desktop_data_shape() {
        let slots = select_model_slots(&catalog(), &SlotOverrides::default()).unwrap();
        let menu = DesktopModelMenu::four_slots();
        let response = menu.models_response(&slots);
        let data = response["data"].as_array().unwrap();
        assert_eq!(data.len(), 4);
        assert_eq!(data[0]["anthropic_family_tier"], "haiku");
        assert_eq!(data[3]["anthropic_family_tier"], "fable");
        assert!(data.iter().all(|model| model["is_family_default"] == true));
        assert!(data.iter().all(|model| model["type"] == "model"));
        assert!(data.iter().all(|model| model["created_at"].is_string()));
        assert_eq!(response["has_more"], false);
        assert_eq!(response["first_id"], slots.haiku);
        assert_eq!(response["last_id"], slots.fable);
        assert_eq!(
            menu.registry_models_json().unwrap(),
            r#"["haiku","sonnet","opus","fable"]"#
        );
    }

    #[test]
    fn full_catalog_uses_stable_claude_compatible_routes_and_exact_labels() {
        let value = catalog();
        let slots = select_model_slots(&value, &SlotOverrides::default()).unwrap();
        let menu =
            DesktopModelMenu::full_catalog_with_source(&value, &slots, "From gateway").unwrap();
        assert_eq!(menu.model_count(), 10);
        assert_eq!(menu.aliased_model_count(), 10);
        assert_eq!(menu.default_model_name(), Some("claude-opus-4-8"));
        let upstream_names = menu
            .models
            .iter()
            .map(|model| model.upstream_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            &upstream_names[..6],
            &[
                "claude-opus-4-8",
                "claude-fable-5",
                "claude-haiku-4-5-20251001",
                "claude-opus-4-6",
                "claude-sonnet-4-6",
                "claude-sonnet-5",
            ]
        );
        assert_eq!(
            &upstream_names[6..],
            &[
                "gpt-5.6-sol",
                "gpt-5.6-sol-max",
                "gemini-3-pro-image",
                "seedance-2-0",
            ],
            "non-Claude models must preserve the gateway order"
        );
        let gpt_max = menu
            .models
            .iter()
            .find(|model| model.upstream_name == "gpt-5.6-sol-max")
            .unwrap();
        assert_eq!(gpt_max.label_override, "gpt-5.6-sol-max");
        assert!(
            menu.models
                .iter()
                .all(|model| model.name.starts_with("claude-gateway-route-"))
        );
        assert_eq!(
            menu.models[0].name,
            desktop_route_id("claude-opus-4-8"),
            "route IDs must remain stable across launches"
        );

        let registry: Value = serde_json::from_str(&menu.registry_models_json().unwrap()).unwrap();
        assert_eq!(registry.as_array().unwrap().len(), 10);
        let registry_gpt_max = registry
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["labelOverride"] == "gpt-5.6-sol-max")
            .unwrap();
        assert_eq!(
            registry_gpt_max["name"],
            desktop_route_id("gpt-5.6-sol-max")
        );
        assert_eq!(registry_gpt_max["labelOverride"], "gpt-5.6-sol-max");
        assert!(registry_gpt_max.get("upstreamName").is_none());
    }

    #[test]
    fn claude_code_catalog_keeps_every_gateway_model_once() {
        let value = catalog();
        let slots = select_model_slots(&value, &SlotOverrides::default()).unwrap();
        let menu =
            DesktopModelMenu::claude_code_catalog(&value, "From https://api.znnz.net").unwrap();
        let upstream_names = menu
            .models
            .iter()
            .map(|model| model.upstream_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            upstream_names,
            vec![
                "claude-fable-5",
                "claude-haiku-4-5-20251001",
                "claude-opus-4-6",
                "claude-opus-4-8",
                "claude-sonnet-4-6",
                "claude-sonnet-5",
                "gpt-5.6-sol",
                "gpt-5.6-sol-max",
                "gemini-3-pro-image",
                "seedance-2-0",
            ]
        );
        assert!(upstream_names.contains(&"claude-opus-4-6"));
        assert!(upstream_names.contains(&"gpt-5.6-sol"));
        assert!(upstream_names.contains(&"seedance-2-0"));
        assert!(
            menu.models
                .iter()
                .all(|model| model.name.starts_with("claude-gateway-route-"))
        );
        let gpt = menu
            .models
            .iter()
            .find(|model| model.upstream_name == "gpt-5.6-sol")
            .unwrap();
        assert_eq!(gpt.label_override, "gpt-5.6-sol");
        assert_eq!(gpt.name, desktop_route_id("gpt-5.6-sol"));
        assert!(menu.models.iter().all(|model| {
            model.anthropic_family_tier.is_none() && model.is_family_default.is_none()
        }));
        let response = menu.models_response(&slots);
        assert!(
            response["data"]
                .as_array()
                .unwrap()
                .iter()
                .all(|model| { model["description"] == "From https://api.znnz.net" })
        );
        assert_eq!(menu.model_count(), 10);
        let picker = menu.claude_code_model_picker();
        assert_eq!(picker["replaceBuiltInOptions"], true);
        assert_eq!(picker["options"].as_array().unwrap().len(), 10);
    }

    #[test]
    fn claude_code_picker_has_one_row_for_a_one_model_gateway() {
        let value = json!({
            "models": [{
                "slug": "gpt-5.4-mini",
                "visibility": "list",
                "supported_in_api": true
            }]
        });
        let menu =
            DesktopModelMenu::claude_code_catalog(&value, "From https://api.znnz.net").unwrap();
        let picker = menu.claude_code_model_picker();
        let options = picker["options"].as_array().unwrap();
        assert_eq!(menu.model_count(), 1);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0]["label"], "gpt-5.4-mini");
        assert_eq!(options[0]["description"], "From https://api.znnz.net");
        assert_eq!(options[0]["model"], desktop_route_id("gpt-5.4-mini"));
    }

    #[test]
    fn hidden_menus_do_not_remove_proxy_routes_or_models_endpoint_entries() {
        let value = catalog();
        let slots = select_model_slots(&value, &SlotOverrides::default()).unwrap();
        let ids = crate::catalog::visible_slugs(&value);
        for full in [
            DesktopModelMenu::full_catalog_with_source(&value, &slots, "test").unwrap(),
            DesktopModelMenu::claude_code_catalog(&value, "test").unwrap(),
        ] {
            let hidden = full.with_hidden_models(&ids);
            assert_eq!(hidden.model_count(), 0);
            assert_eq!(hidden.registry_models_json().unwrap(), "[]");
            assert!(
                hidden.claude_code_model_picker()["options"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(full.model_routes().len(), ids.len());
            assert_eq!(
                full.models_response(&slots)["data"]
                    .as_array()
                    .unwrap()
                    .len(),
                ids.len()
            );
        }
    }

    #[test]
    fn full_catalog_assigns_one_default_per_available_claude_family() {
        let value = catalog();
        let slots = select_model_slots(&value, &SlotOverrides::default()).unwrap();
        let menu =
            DesktopModelMenu::full_catalog_with_source(&value, &slots, "From gateway").unwrap();
        for (tier, expected) in [
            ("haiku", slots.haiku.as_str()),
            ("sonnet", slots.sonnet.as_str()),
            ("opus", slots.opus.as_str()),
            ("fable", slots.fable.as_str()),
        ] {
            let defaults = menu
                .models
                .iter()
                .filter(|model| model.anthropic_family_tier.as_deref() == Some(tier))
                .filter(|model| model.is_family_default == Some(true))
                .collect::<Vec<_>>();
            assert_eq!(defaults.len(), 1, "tier={tier}");
            assert_eq!(defaults[0].upstream_name, expected);
        }
        let media = menu
            .models
            .iter()
            .find(|model| model.upstream_name == "seedance-2-0")
            .unwrap();
        assert_eq!(media.anthropic_family_tier, None);
        assert_eq!(media.is_family_default, None);
    }

    #[test]
    fn upstream_urls_do_not_duplicate_v1() {
        let base = normalize_gateway_url("https://api.znnz.net/v1/").unwrap();
        let uri: Uri = "/v1/messages?beta=1".parse().unwrap();
        assert_eq!(
            upstream_url(&base, &uri).unwrap().as_str(),
            "https://api.znnz.net/v1/messages?beta=1"
        );
        let root = normalize_gateway_url("https://api.znnz.net").unwrap();
        assert_eq!(
            upstream_url(&root, &uri).unwrap().as_str(),
            "https://api.znnz.net/v1/messages?beta=1"
        );
    }

    #[tokio::test]
    async fn local_proxy_serves_authenticated_model_catalog_and_cors() {
        let slots = select_model_slots(&catalog(), &SlotOverrides::default()).unwrap();
        let proxy = ClaudeDesktopProxy::start(
            "https://api.znnz.net",
            "upstream-test-key".into(),
            slots.clone(),
            DesktopModelMenu::four_slots(),
        )
        .await
        .unwrap();
        let client = reqwest::Client::new();
        let response = client
            .get(format!("{}/v1/models?limit=1000", proxy.base_url()))
            .bearer_auth(proxy.local_token())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "*"
        );
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["data"].as_array().unwrap().len(), 4);
        assert_eq!(body["data"][0]["id"], slots.haiku);

        let preflight = client
            .request(
                reqwest::Method::OPTIONS,
                format!("{}/v1/messages", proxy.base_url()),
            )
            .header(
                "access-control-request-headers",
                "authorization,anthropic-version,content-type",
            )
            .send()
            .await
            .unwrap();
        assert_eq!(preflight.status(), reqwest::StatusCode::NO_CONTENT);
        assert_eq!(
            preflight
                .headers()
                .get("access-control-allow-headers")
                .unwrap(),
            "authorization,anthropic-version,content-type"
        );
        proxy.stop().await;
    }

    #[tokio::test]
    async fn local_proxy_serves_full_gateway_model_catalog() {
        let value = catalog();
        let slots = select_model_slots(&value, &SlotOverrides::default()).unwrap();
        let menu =
            DesktopModelMenu::full_catalog_with_source(&value, &slots, "From gateway").unwrap();
        let expected_count = menu.model_count();
        let proxy = ClaudeDesktopProxy::start(
            "https://api.znnz.net",
            "upstream-test-key".into(),
            slots,
            menu,
        )
        .await
        .unwrap();
        let body: Value = reqwest::Client::new()
            .get(format!("{}/v1/models", proxy.base_url()))
            .bearer_auth(proxy.local_token())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let data = body["data"].as_array().unwrap();
        assert_eq!(data.len(), expected_count);
        assert_eq!(data[0]["id"], desktop_route_id("claude-opus-4-8"));
        assert_eq!(data[0]["display_name"], "claude-opus-4-8");
        let gpt = data
            .iter()
            .find(|model| model["display_name"] == "gpt-5.6-sol")
            .unwrap();
        assert_eq!(gpt["id"], desktop_route_id("gpt-5.6-sol"));
        let gpt_max = data
            .iter()
            .find(|model| model["display_name"] == "gpt-5.6-sol-max")
            .unwrap();
        assert_eq!(gpt_max["id"], desktop_route_id("gpt-5.6-sol-max"));
        assert!(data.iter().all(|model| model["type"] == "model"));
        assert!(data.iter().all(|model| model["created_at"].is_string()));
        assert_eq!(body["has_more"], false);
        assert_eq!(body["first_id"], desktop_route_id("claude-opus-4-8"));
        assert_eq!(body["last_id"], desktop_route_id("seedance-2-0"));
        proxy.stop().await;
    }

    #[test]
    fn upstream_error_summary_redacts_keys_and_bearer_tokens() {
        let body = br#"{"error":{"type":"invalid_request_error","message":"bad sk-visible-token and Bearer bearer-secret plus upstream-test-key"}}"#;
        let summary = summarize_upstream_error(body, "upstream-test-key");
        assert!(summary.contains("invalid_request_error"));
        assert!(summary.contains("[REDACTED]"));
        assert!(!summary.contains("sk-visible-token"));
        assert!(!summary.contains("bearer-secret"));
        assert!(!summary.contains("upstream-test-key"));
    }

    #[test]
    fn upstream_error_summary_does_not_log_binary_garbage() {
        for body in [&[0xff, 0xfe, 0x00, 0x81, 0x7f][..], &b"\0binary"[..]] {
            assert_eq!(
                summarize_upstream_error_for_language(body, "", i18n::Language::ZhCn),
                "上游返回了非文本错误内容"
            );
            assert_eq!(
                summarize_upstream_error_for_language(body, "", i18n::Language::En),
                "Upstream returned a non-text error response"
            );
        }

        let text = summarize_upstream_error(b"gateway unavailable", "");
        assert_eq!(text, "gateway unavailable");
        let chinese = "{\"error\":{\"message\":\"模型不可用\"}}";
        assert_eq!(
            summarize_upstream_error(chinese.as_bytes(), ""),
            "message=模型不可用"
        );
    }

    #[tokio::test]
    async fn proxy_rewrites_route_and_preserves_upstream_error_body() {
        let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
        let captured_tx = Arc::new(tokio::sync::Mutex::new(Some(captured_tx)));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let gateway_address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let captured_tx = captured_tx.clone();
            let service = service_fn(move |request: Request<Incoming>| {
                let captured_tx = captured_tx.clone();
                async move {
                    assert_eq!(request.headers()[http::header::ACCEPT_ENCODING], "identity");
                    assert_eq!(
                        request
                            .headers()
                            .get_all(http::header::ACCEPT_ENCODING)
                            .iter()
                            .count(),
                        1
                    );
                    let authorization = request
                        .headers()
                        .get(AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    let api_key = request
                        .headers()
                        .get("x-api-key")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    let body = request.into_body().collect().await.unwrap().to_bytes();
                    let value: Value = serde_json::from_slice(&body).unwrap();
                    if let Some(sender) = captured_tx.lock().await.take() {
                        let _ = sender.send((value, authorization, api_key));
                    }
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(StatusCode::BAD_REQUEST)
                            .header(http::header::CONTENT_TYPE, "application/json")
                            .body(Full::new(Bytes::from_static(
                                br#"{"error":{"type":"mock_error","message":"upstream-test-key must still reach the client"}}"#,
                            )))
                            .unwrap(),
                    )
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });

        let value = catalog();
        let slots = select_model_slots(&value, &SlotOverrides::default()).unwrap();
        let menu =
            DesktopModelMenu::full_catalog_with_source(&value, &slots, "From gateway").unwrap();
        let route = menu
            .models
            .iter()
            .find(|model| model.upstream_name == "gpt-5.6-sol-max")
            .unwrap()
            .name
            .clone();
        let proxy = ClaudeDesktopProxy::start(
            &format!("http://{gateway_address}"),
            "upstream-test-key".into(),
            slots,
            menu,
        )
        .await
        .unwrap();
        let response = reqwest::Client::new()
            .post(format!("{}/v1/messages", proxy.base_url()))
            .bearer_auth(proxy.local_token())
            .header(http::header::ACCEPT_ENCODING, "gzip, br")
            .json(&json!({
                "model": route,
                "stream": true,
                "messages": [{"role":"user","content":"not logged"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        let returned = response.text().await.unwrap();
        assert!(returned.contains("upstream-test-key must still reach the client"));

        let (captured, authorization, api_key) = captured_rx.await.unwrap();
        assert_eq!(captured["model"], "gpt-5.6-sol-max");
        assert_eq!(captured["stream"], true);
        assert_eq!(authorization.as_deref(), Some("Bearer upstream-test-key"));
        assert_eq!(api_key.as_deref(), Some("upstream-test-key"));
        proxy.stop().await;
        server.abort();
    }

    #[test]
    fn token_comparison_and_header_filtering_are_safe() {
        assert!(constant_time_equal(b"same", b"same"));
        assert!(!constant_time_equal(b"same", b"diff"));
        assert!(!constant_time_equal(b"short", b"longer"));
        assert!(is_hop_or_secret_header(http::header::HOST.as_str()));
        assert!(is_hop_or_secret_header(
            http::header::CONTENT_LENGTH.as_str()
        ));
        assert!(is_hop_or_secret_header(http::header::CONNECTION.as_str()));
        assert!(is_hop_or_secret_header(
            http::header::TRANSFER_ENCODING.as_str()
        ));
    }
}
