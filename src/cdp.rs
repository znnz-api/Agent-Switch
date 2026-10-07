use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::warn;

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(8);
const PORT_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct CdpTarget {
    pub id: String,
    #[serde(rename = "type")]
    pub target_type: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default, rename = "webSocketDebuggerUrl")]
    pub websocket_url: Option<String>,
}

pub fn reserve_loopback_port() -> Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .context("无法为 Codex Desktop 分配 CDP 端口")?;
    Ok(listener.local_addr()?.port())
}

pub async fn port_reachable(port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            PORT_PROBE_TIMEOUT,
            tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
        )
        .await,
        Ok(Ok(_))
    )
}

pub async fn list_targets(port: u16) -> Result<Vec<CdpTarget>> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(HTTP_TIMEOUT)
        .build()
        .context("无法创建 CDP HTTP 客户端")?;
    let mut errors = Vec::new();
    for endpoint in [
        format!("http://127.0.0.1:{port}/json/list"),
        format!("http://127.0.0.1:{port}/json"),
        format!("http://[::1]:{port}/json/list"),
        format!("http://[::1]:{port}/json"),
    ] {
        let result = async {
            let targets = client
                .get(&endpoint)
                .send()
                .await
                .context("CDP 连接失败")?
                .error_for_status()
                .context("CDP 返回错误状态")?
                .json::<Vec<CdpTarget>>()
                .await
                .context("CDP target 列表不是有效 JSON")?;
            for target in &targets {
                if let Some(url) = target.websocket_url.as_deref() {
                    validate_websocket_url(url, port)
                        .with_context(|| format!("target {} 的 WebSocket 地址不安全", target.id))?;
                }
            }
            Result::<_>::Ok(targets)
        }
        .await;
        match result {
            Ok(targets) => return Ok(targets),
            Err(error) => errors.push(format!("{endpoint}: {error:#}")),
        }
    }
    bail!("无法读取 Codex Desktop CDP target：{}", errors.join("；"))
}

pub fn validate_websocket_url(value: &str, expected_port: u16) -> Result<()> {
    let parsed = url::Url::parse(value).context("CDP WebSocket 地址无效")?;
    if parsed.scheme() != "ws" {
        bail!("本地 CDP WebSocket 必须使用 ws")
    }
    let host = parsed.host_str().context("CDP WebSocket 缺少主机")?;
    let address = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .context("CDP WebSocket 主机必须是 IP 地址")?;
    if !address.is_loopback() {
        bail!("CDP WebSocket 必须绑定回环地址")
    }
    if parsed.port() != Some(expected_port) {
        bail!("CDP WebSocket 端口与启动端口不一致")
    }
    Ok(())
}

pub fn injectable_targets(targets: &[CdpTarget], port: u16) -> Vec<CdpTarget> {
    targets
        .iter()
        .filter(|target| {
            target.target_type == "page"
                && target.url.to_ascii_lowercase().starts_with("app://-/")
                && target
                    .websocket_url
                    .as_deref()
                    .is_some_and(|url| validate_websocket_url(url, port).is_ok())
        })
        .cloned()
        .collect()
}

pub async fn inject_target(
    target: &CdpTarget,
    port: u16,
    injection: &crate::injection::ScriptBundle,
) -> Result<()> {
    let websocket_url = target
        .websocket_url
        .as_deref()
        .context("CDP target 缺少 WebSocket 地址")?;
    validate_websocket_url(websocket_url, port)?;
    let (mut socket, _) = tokio::time::timeout(COMMAND_TIMEOUT, connect_async(websocket_url))
        .await
        .context("连接 CDP WebSocket 超时")?
        .context("连接 CDP WebSocket 失败")?;

    send_command(
        &mut socket,
        1,
        "Page.addScriptToEvaluateOnNewDocument",
        json!({ "source": injection.source() }),
    )
    .await?;
    send_command(
        &mut socket,
        2,
        "Runtime.evaluate",
        json!({
            "expression": injection.source(),
            "awaitPromise": false,
            "returnByValue": true,
            "allowUnsafeEvalBlockedByCSP": true
        }),
    )
    .await?;
    let mut healthy = false;
    for attempt in 0..25u64 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let health = send_command(
            &mut socket,
            3 + attempt,
            "Runtime.evaluate",
            json!({
                "expression": injection.health_expression(),
                "awaitPromise": true,
                "returnByValue": true,
                "allowUnsafeEvalBlockedByCSP": true
            }),
        )
        .await?;
        if health
            .pointer("/result/result/value")
            .and_then(Value::as_bool)
            == Some(true)
        {
            healthy = true;
            break;
        }
    }
    if !healthy {
        let details = send_command(
            &mut socket,
            100,
            "Runtime.evaluate",
            json!({
                "expression": injection.health_details_expression(),
                "awaitPromise": true,
                "returnByValue": true,
                "allowUnsafeEvalBlockedByCSP": true
            }),
        )
        .await
        .ok()
        .and_then(|response| response.pointer("/result/result/value").cloned())
        .unwrap_or_else(|| json!({ "installed": false, "diagnostic": "无法读取健康详情" }));
        bail!(
            "注入脚本已执行，但模型目录在 5 秒内未通过健康检查。健康状态: {}",
            serde_json::to_string(&details).unwrap_or_else(|_| "<无法序列化>".to_owned())
        )
    }
    let _ = socket.close(None).await;
    Ok(())
}

pub async fn inject_all(
    port: u16,
    injection: &crate::injection::ScriptBundle,
) -> Result<HashSet<String>> {
    let targets = injectable_targets(&list_targets(port).await?, port);
    if targets.is_empty() {
        bail!("CDP 已启动，但未发现 app://-/ Codex 渲染页面")
    }
    let mut injected = HashSet::new();
    let mut errors = Vec::new();
    for target in targets {
        match inject_target(&target, port, injection).await {
            Ok(()) => {
                injected.insert(target.id);
            }
            Err(error) => errors.push(format!("{}: {error:#}", target.id)),
        }
    }
    if injected.is_empty() {
        bail!("所有 Codex 渲染页面注入均失败：{}", errors.join("；"))
    }
    for error in errors {
        if crate::i18n::language() == crate::i18n::Language::ZhCn {
            warn!("部分渲染页面注入失败: {error}");
        } else {
            warn!(
                "Some rendered pages failed injection: {}",
                crate::i18n::runtime_error_text(&error)
            );
        }
    }
    Ok(injected)
}

pub async fn target_health(
    target: &CdpTarget,
    port: u16,
    injection: &crate::injection::ScriptBundle,
) -> Result<bool> {
    let websocket_url = target
        .websocket_url
        .as_deref()
        .context("CDP target 缺少 WebSocket 地址")?;
    validate_websocket_url(websocket_url, port)?;
    let (mut socket, _) = tokio::time::timeout(COMMAND_TIMEOUT, connect_async(websocket_url))
        .await
        .context("连接 CDP WebSocket 超时")?
        .context("连接 CDP WebSocket 失败")?;
    let response = send_command(
        &mut socket,
        10,
        "Runtime.evaluate",
        json!({
            "expression": injection.health_expression(),
            "awaitPromise": true,
            "returnByValue": true,
            "allowUnsafeEvalBlockedByCSP": true
        }),
    )
    .await?;
    let _ = socket.close(None).await;
    Ok(response
        .pointer("/result/result/value")
        .and_then(Value::as_bool)
        == Some(true))
}

async fn send_command<S>(socket: &mut S, id: u64, method: &str, params: Value) -> Result<Value>
where
    S: SinkExt<Message>
        + StreamExt<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::error::Error + Send + Sync + 'static,
{
    socket
        .send(Message::Text(
            json!({ "id": id, "method": method, "params": params })
                .to_string()
                .into(),
        ))
        .await
        .with_context(|| format!("发送 CDP 命令 {method} 失败"))?;

    let response = tokio::time::timeout(COMMAND_TIMEOUT, async {
        loop {
            let message = socket
                .next()
                .await
                .context("CDP WebSocket 提前关闭")?
                .context("读取 CDP WebSocket 失败")?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(&text).context("CDP 响应不是有效 JSON")?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return Result::<Value>::Ok(value);
            }
        }
    })
    .await
    .with_context(|| format!("等待 CDP 命令 {method} 响应超时"))??;

    if let Some(error) = response.get("error") {
        bail!("CDP 命令 {method} 失败: {error}")
    }
    if let Some(exception) = response.pointer("/result/exceptionDetails") {
        bail!("CDP 命令 {method} 触发脚本异常: {exception}")
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_expected_loopback_websocket() {
        assert!(validate_websocket_url("ws://127.0.0.1:9229/devtools/page/1", 9229).is_ok());
        assert!(validate_websocket_url("ws://[::1]:9229/devtools/page/1", 9229).is_ok());
        assert!(validate_websocket_url("ws://127.0.0.1:9230/devtools/page/1", 9229).is_err());
        assert!(validate_websocket_url("ws://192.168.1.5:9229/devtools/page/1", 9229).is_err());
        assert!(validate_websocket_url("wss://127.0.0.1:9229/devtools/page/1", 9229).is_err());
    }

    #[test]
    fn only_app_renderer_is_injectable() {
        let targets = vec![
            CdpTarget {
                id: "codex".into(),
                target_type: "page".into(),
                title: "Codex".into(),
                url: "app://-/index.html".into(),
                websocket_url: Some("ws://127.0.0.1:9229/devtools/page/1".into()),
            },
            CdpTarget {
                id: "web".into(),
                target_type: "page".into(),
                title: "Web".into(),
                url: "https://example.com".into(),
                websocket_url: Some("ws://127.0.0.1:9229/devtools/page/2".into()),
            },
        ];
        let selected = injectable_targets(&targets, 9229);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "codex");
    }
    #[tokio::test]
    async fn port_probe_detects_open_and_closed_listener() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_reachable(port).await);
        drop(listener);
        assert!(!port_reachable(port).await);
    }
}
