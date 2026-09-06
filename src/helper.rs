use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{RwLock, watch};
use tokio::task::JoinHandle;
use tracing::warn;

pub struct Helper {
    address: SocketAddr,
    token: String,
    #[allow(dead_code)]
    catalog: Arc<RwLock<Value>>,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Helper {
    pub async fn start(initial_catalog: Value) -> Result<Self> {
        crate::catalog::validate_catalog(&initial_catalog)?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("无法启动本地模型 Helper")?;
        let address = listener.local_addr()?;
        if !address.ip().is_loopback() {
            bail!("安全检查失败：Helper 没有绑定到回环地址");
        }
        let token = random_token();
        let catalog = Arc::new(RwLock::new(initial_catalog));
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let state = catalog.clone();
        let expected_token = token.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() { break; }
                    }
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, peer)) if peer.ip().is_loopback() => {
                                let state = state.clone();
                                let token = expected_token.clone();
                                tokio::spawn(async move {
                                    if let Err(error) = handle(stream, state, &token).await {
                                        if crate::i18n::language() == crate::i18n::Language::ZhCn {
                                            warn!("Helper 请求失败: {error:#}");
                                        } else {
                                            warn!(
                                                "Helper request failed: {}",
                                                crate::i18n::runtime_error(&error)
                                            );
                                        }
                                    }
                                });
                            }
                            Ok((_stream, peer)) => {
                                if crate::i18n::language() == crate::i18n::Language::ZhCn {
                                    warn!("拒绝非回环 Helper 请求: {peer}");
                                } else {
                                    warn!("Rejected non-loopback Helper request: {peer}");
                                }
                            }
                            Err(error) => {
                                if crate::i18n::language() == crate::i18n::Language::ZhCn {
                                    warn!("Helper accept 失败: {error}");
                                } else {
                                    warn!("Helper accept failed: {error}");
                                }
                                break;
                            }
                        }
                    }
                }
            }
        });
        Ok(Self {
            address,
            token,
            catalog,
            shutdown,
            task,
        })
    }

    pub fn catalog_url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/catalog?token={}",
            self.address.port(),
            self.token
        )
    }

    #[allow(dead_code)]
    pub fn health_url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/health?token={}",
            self.address.port(),
            self.token
        )
    }

    #[allow(dead_code)]
    pub async fn update_catalog(&self, value: Value) -> Result<()> {
        crate::catalog::validate_catalog(&value)?;
        *self.catalog.write().await = value;
        Ok(())
    }

    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

async fn handle(mut stream: TcpStream, catalog: Arc<RwLock<Value>>, token: &str) -> Result<()> {
    let mut buffer = vec![0u8; 16 * 1024];
    let mut length = 0usize;
    loop {
        if length == buffer.len() {
            bail!("HTTP 请求头过大");
        }
        let read = stream.read(&mut buffer[length..]).await?;
        if read == 0 {
            return Ok(());
        }
        length += read;
        if buffer[..length].windows(4).any(|part| part == b"\r\n\r\n") {
            break;
        }
    }
    let request = String::from_utf8_lossy(&buffer[..length]);
    let first = request.lines().next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    if method == "OPTIONS" {
        return respond(&mut stream, 204, "text/plain", b"").await;
    }
    if method != "GET" {
        return respond(
            &mut stream,
            405,
            "application/json",
            br#"{"error":"method_not_allowed"}"#,
        )
        .await;
    }
    let parsed = url::Url::parse(&format!("http://127.0.0.1{target}"))?;
    let supplied = parsed
        .query_pairs()
        .find(|(key, _)| key == "token")
        .map(|(_, value)| value.into_owned());
    if supplied.as_deref() != Some(token) {
        return respond(
            &mut stream,
            403,
            "application/json",
            br#"{"error":"forbidden"}"#,
        )
        .await;
    }

    match parsed.path() {
        "/health" => respond(&mut stream, 200, "application/json", br#"{"ok":true}"#).await,
        "/catalog" | "/codex-model-catalog" => {
            let body = serde_json::to_vec(&*catalog.read().await)?;
            respond(&mut stream, 200, "application/json; charset=utf-8", &body).await
        }
        _ => {
            respond(
                &mut stream,
                404,
                "application/json",
                br#"{"error":"not_found"}"#,
            )
            .await
        }
    }
}

async fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, OPTIONS\r\nCache-Control: no-store\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await?;
    Ok(())
}

fn random_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[allow(dead_code)]
fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn request(address: SocketAddr, target: &str) -> String {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(
                format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        String::from_utf8(response).unwrap()
    }

    fn catalog() -> Value {
        json!({
            "models": [{
                "slug": "gpt-test",
                "visibility": "list",
                "supported_in_api": true
            }]
        })
    }

    #[tokio::test]
    async fn helper_rejects_missing_and_wrong_tokens() {
        let helper = Helper::start(catalog()).await.unwrap();
        assert!(helper.address.ip().is_loopback());

        let missing = request(helper.address, "/health").await;
        assert!(missing.starts_with("HTTP/1.1 403 Forbidden"));
        let wrong = request(helper.address, "/health?token=wrong").await;
        assert!(wrong.starts_with("HTTP/1.1 403 Forbidden"));
        let correct = request(helper.address, &format!("/health?token={}", helper.token)).await;
        assert!(correct.starts_with("HTTP/1.1 200 OK"));
        assert!(correct.ends_with("{\"ok\":true}"));
        helper.stop().await;
    }

    #[tokio::test]
    async fn helper_serves_catalog_only_with_its_random_token() {
        let helper = Helper::start(catalog()).await.unwrap();
        assert_eq!(helper.token.len(), 64);
        assert!(helper.token.chars().all(|ch| ch.is_ascii_hexdigit()));
        let response = request(
            helper.address,
            &format!("/codex-model-catalog?token={}", helper.token),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("gpt-test"));
        helper.stop().await;
    }
}
