use crate::network_proxy;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::json;
use std::time::Duration;
use url::Url;

pub const DEFAULT_GATEWAY_URL: &str = "https://api.znnz.net";
pub const MAINLAND_ACCESS_ERROR: &str = "接口地址无法在中国大陆地区正常解析访问";
pub const MAINLAND_ACCESS_ERROR_EN: &str =
    "The gateway address cannot be resolved or accessed normally from mainland China";
const MAINLAND_ACCESS_HINT_ZH: &str =
    "。接口地址 • 无法在中国大陆地区正常解析访问，请配置可供本机程序使用的代理或 VPN 后重试";
const MAINLAND_ACCESS_HINT_EN: &str = "The gateway address cannot be resolved or accessed normally from mainland China. Configure a proxy or VPN available to local applications, then try again";
const GENERIC_NETWORK_HINT_ZH: &str = "。请检查域名解析、系统或 HTTPS_PROXY 代理、VPN 以及防火墙";
const GENERIC_NETWORK_HINT_EN: &str =
    "Check DNS resolution, system or HTTPS_PROXY settings, VPN, and firewall";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayIdentity {
    pub base_url: String,
    pub display_name: String,
    pub is_znnz: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayProtocol {
    OpenAiResponses,
    AnthropicMessages,
}

impl GatewayProtocol {
    fn route(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "responses",
            Self::AnthropicMessages => "messages",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::OpenAiResponses => "OpenAI Responses",
            Self::AnthropicMessages => "Anthropic Messages",
        }
    }
}

impl GatewayIdentity {
    pub fn parse(value: &str) -> Result<Self> {
        let mut url = Url::parse(value.trim()).context(crate::i18n::tr(
            "接口地址格式无效",
            "Invalid gateway URL format",
        ))?;
        let host = url.host_str().context(crate::i18n::tr(
            "接口地址缺少主机名",
            "Gateway URL is missing a host",
        ))?;
        if url.scheme() != "https" && !is_loopback(host) {
            bail!(
                "{}",
                crate::i18n::tr(
                    "接口地址必须使用 HTTPS；只有本机回环地址允许 HTTP",
                    "Gateway URLs must use HTTPS; only local loopback addresses may use HTTP"
                )
            );
        }

        url.set_query(None);
        url.set_fragment(None);
        let path = url.path().trim_end_matches('/').to_owned();
        url.set_path(&path);

        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        let is_znnz = matches!(host.as_str(), "znnz.net" | "www.znnz.net" | "api.znnz.net");
        let display_name = if is_znnz {
            "znnz.net".to_owned()
        } else if let Some(port) = url.port() {
            format!("{host}:{port}")
        } else {
            host
        };

        Ok(Self {
            base_url: url.to_string().trim_end_matches('/').to_owned(),
            display_name,
            is_znnz,
        })
    }

    pub fn source_label(&self) -> String {
        // Use the normalized endpoint rather than a branded host label. This
        // keeps model provenance accurate for znnz.net and custom gateways,
        // while never exposing the API key or query/fragment data.
        format!("From {}", self.base_url)
    }

    fn is_remote_custom(&self) -> bool {
        !self.is_znnz
            && Url::parse(&self.base_url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .is_some_and(|host| !is_loopback(&host))
    }
}

pub fn connection_error_hint(gateway: &GatewayIdentity, error: &reqwest::Error) -> &'static str {
    if !error.is_timeout() && !error.is_connect() {
        return "";
    }
    if gateway.is_remote_custom() {
        crate::i18n::tr(MAINLAND_ACCESS_HINT_ZH, MAINLAND_ACCESS_HINT_EN)
    } else {
        crate::i18n::tr(GENERIC_NETWORK_HINT_ZH, GENERIC_NETWORK_HINT_EN)
    }
}

pub async fn ensure_protocol(
    gateway_url: &str,
    api_key: &str,
    protocol: GatewayProtocol,
) -> Result<()> {
    let gateway = GatewayIdentity::parse(gateway_url)?;
    if gateway.is_znnz {
        return Ok(());
    }

    let endpoint = protocol_endpoint(&gateway.base_url, protocol)?;
    let client = network_proxy::configure_reqwest_builder(reqwest::Client::builder())?
        .connect_timeout(Duration::from_secs(6))
        .timeout(Duration::from_secs(12))
        .user_agent(format!(
            "znnz-agent-launcher/{}/protocol-check",
            env!("CARGO_PKG_VERSION")
        ))
        .build()?;
    let body = match protocol {
        GatewayProtocol::OpenAiResponses => json!({
            "model": "__znnz_agent_launcher_protocol_probe__",
            "input": "",
            "max_output_tokens": 1,
            "stream": false
        }),
        GatewayProtocol::AnthropicMessages => json!({
            "model": "__znnz_agent_launcher_protocol_probe__",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "protocol probe"}],
            "stream": false
        }),
    };

    let request = client
        .post(endpoint.clone())
        .header(AUTHORIZATION, format!("Bearer {api_key}"))
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body);
    let response = network_proxy::send_with_network_retry(request)
        .await
        .map_err(|error| {
            let hint = connection_error_hint(&gateway, &error);
            anyhow!(
                "{} {}: {}{}{}",
                crate::i18n::tr("无法检查", "Unable to check"),
                protocol.label(),
                error,
                if hint.is_empty() { "" } else { ". " },
                hint
            )
        })?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let body = response
        .text()
        .await
        .unwrap_or_default()
        .chars()
        .take(2000)
        .collect::<String>();

    if route_is_supported(status.as_u16(), &content_type, &body) {
        return Ok(());
    }
    if crate::i18n::language() == crate::i18n::Language::ZhCn {
        bail!(
            "自定义网关不支持 {} 协议：{}。Codex 需要 Responses API，Claude 需要 Anthropic Messages API；只有 Chat Completions 的网关需要先接入 LiteLLM、CLIProxyAPI 等转换层。",
            protocol.label(),
            endpoint
        )
    } else {
        bail!(
            "The custom gateway does not support {} at {}. Codex requires the Responses API and Claude requires the Anthropic Messages API. A Chat Completions-only gateway needs a conversion layer such as LiteLLM or CLIProxyAPI.",
            protocol.label(),
            endpoint
        )
    }
}

fn protocol_endpoint(base_url: &str, protocol: GatewayProtocol) -> Result<Url> {
    let mut url = Url::parse(base_url).context(crate::i18n::tr(
        "接口地址格式无效",
        "Invalid gateway URL format",
    ))?;
    let path = url.path().trim_end_matches('/');
    let next = if path.ends_with("/v1") || path == "v1" {
        format!("{path}/{}", protocol.route())
    } else {
        format!("{path}/v1/{}", protocol.route())
    };
    url.set_path(&next);
    Ok(url)
}

fn route_is_supported(status: u16, content_type: &str, body: &str) -> bool {
    if status == 405 {
        return false;
    }
    if status != 404 {
        return true;
    }

    let lower = body.to_ascii_lowercase();
    let obvious_missing_route = lower.contains("cannot post")
        || lower.contains("cannot get")
        || lower.contains("route not found")
        || lower.contains("page not found")
        || lower.contains("<html");
    content_type.contains("json") && !obvious_missing_route
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_default_and_custom_gateways() {
        let znnz = GatewayIdentity::parse("https://api.znnz.net/v1/").unwrap();
        assert_eq!(znnz.base_url, "https://api.znnz.net/v1");
        assert_eq!(znnz.display_name, "znnz.net");
        assert_eq!(znnz.source_label(), "From https://api.znnz.net/v1");
        assert!(znnz.is_znnz);

        let custom = GatewayIdentity::parse("https://gateway.example.com/api/").unwrap();
        assert_eq!(custom.base_url, "https://gateway.example.com/api");
        assert_eq!(custom.display_name, "gateway.example.com");
        assert_eq!(
            custom.source_label(),
            "From https://gateway.example.com/api"
        );
        assert!(!custom.is_znnz);
        assert!(custom.is_remote_custom());
        assert!(!znnz.is_remote_custom());
    }

    #[test]
    fn mainland_access_message_only_applies_to_remote_custom_gateways() {
        assert!(
            GatewayIdentity::parse("https://gateway.example.com")
                .unwrap()
                .is_remote_custom()
        );
        assert!(
            !GatewayIdentity::parse("https://api.znnz.net")
                .unwrap()
                .is_remote_custom()
        );
        assert!(
            !GatewayIdentity::parse("http://127.0.0.1:4000")
                .unwrap()
                .is_remote_custom()
        );
    }

    #[test]
    fn rejects_insecure_remote_http() {
        assert!(GatewayIdentity::parse("http://gateway.example.com").is_err());
        assert!(GatewayIdentity::parse("http://127.0.0.1:4000").is_ok());
    }

    #[test]
    fn builds_protocol_endpoints_without_duplicate_v1() {
        assert_eq!(
            protocol_endpoint(
                "https://gateway.example.com/v1",
                GatewayProtocol::OpenAiResponses
            )
            .unwrap()
            .as_str(),
            "https://gateway.example.com/v1/responses"
        );
        assert_eq!(
            protocol_endpoint(
                "https://gateway.example.com",
                GatewayProtocol::AnthropicMessages
            )
            .unwrap()
            .as_str(),
            "https://gateway.example.com/v1/messages"
        );
    }

    #[test]
    fn distinguishes_missing_routes_from_model_errors() {
        assert!(!route_is_supported(
            404,
            "text/html",
            "<html>Cannot POST /v1/responses"
        ));
        assert!(route_is_supported(
            404,
            "application/json",
            r#"{"error":{"message":"model not found"}}"#
        ));
        assert!(route_is_supported(400, "application/json", "invalid model"));
    }
}
