use anyhow::{Context, Result, bail};
use reqwest::{ClientBuilder, NoProxy, Proxy, RequestBuilder, Response};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

const PROXY_ENVIRONMENT_KEYS: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];
const LOCAL_NO_PROXY: &str = "127.0.0.1,localhost,::1";
const NETWORK_RETRY_DELAY: Duration = Duration::from_millis(500);
const PAC_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone)]
enum WindowsProxyConfig {
    Fixed(String),
    Auto {
        pac_url: Option<String>,
        auto_detect: bool,
    },
}

#[derive(Clone)]
struct CachedProxy {
    resolved_at: Instant,
    proxy_url: Option<String>,
}

pub fn configure_reqwest_builder(builder: ClientBuilder) -> Result<ClientBuilder> {
    if environment_proxy_configured() {
        return Ok(builder);
    }
    let Some(config) = windows_system_proxy_config()? else {
        return Ok(builder);
    };
    let no_proxy = NoProxy::from_string(&merged_no_proxy());
    match config {
        WindowsProxyConfig::Fixed(proxy_url) => {
            let proxy = Proxy::all(&proxy_url)
                .with_context(|| format!("Windows 系统代理地址无效: {proxy_url}"))?
                .no_proxy(no_proxy);
            Ok(builder.proxy(proxy))
        }
        WindowsProxyConfig::Auto {
            pac_url,
            auto_detect,
        } => {
            let cache = Arc::new(Mutex::new(HashMap::<String, CachedProxy>::new()));
            let proxy = Proxy::custom(move |url| {
                resolve_auto_proxy_cached(url, pac_url.as_deref(), auto_detect, &cache)
                    .unwrap_or_else(|_| Some("http://127.0.0.1:0".to_owned()))
            })
            .no_proxy(no_proxy);
            Ok(builder.proxy(proxy))
        }
    }
}

pub fn child_proxy_environment(target_url: &str) -> Result<Vec<(String, String)>> {
    if environment_proxy_configured() {
        return Ok(vec![("NO_PROXY".to_owned(), merged_no_proxy())]);
    }
    let Some(config) = windows_system_proxy_config()? else {
        return Ok(Vec::new());
    };
    let proxy_url = match config {
        WindowsProxyConfig::Fixed(proxy_url) => Some(proxy_url),
        WindowsProxyConfig::Auto {
            pac_url,
            auto_detect,
        } => resolve_windows_auto_proxy(target_url, pac_url.as_deref(), auto_detect)?,
    };
    let Some(proxy_url) = proxy_url else {
        return Ok(vec![("NO_PROXY".to_owned(), merged_no_proxy())]);
    };
    Ok(vec![
        ("HTTP_PROXY".to_owned(), proxy_url.clone()),
        ("HTTPS_PROXY".to_owned(), proxy_url),
        ("NO_PROXY".to_owned(), merged_no_proxy()),
    ])
}

/// Retry only failures returned before an HTTP response exists. Callers must
/// not use this helper for billable or otherwise non-idempotent user requests.
pub async fn send_with_network_retry(request: RequestBuilder) -> reqwest::Result<Response> {
    let retry = request.try_clone();
    match request.send().await {
        Ok(response) => Ok(response),
        Err(error) => {
            let Some(retry) = retry else {
                return Err(error);
            };
            tokio::time::sleep(NETWORK_RETRY_DELAY).await;
            retry.send().await
        }
    }
}

fn environment_proxy_configured() -> bool {
    PROXY_ENVIRONMENT_KEYS.iter().any(|name| {
        std::env::var_os(name).is_some_and(|value| !value.to_string_lossy().trim().is_empty())
    })
}

fn merged_no_proxy() -> String {
    let existing = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    if existing.trim().is_empty() {
        LOCAL_NO_PROXY.to_owned()
    } else {
        format!("{LOCAL_NO_PROXY},{}", existing.trim())
    }
}

fn resolve_auto_proxy_cached(
    target_url: &Url,
    pac_url: Option<&str>,
    auto_detect: bool,
    cache: &Mutex<HashMap<String, CachedProxy>>,
) -> Result<Option<String>> {
    let key = target_url.origin().ascii_serialization();
    if let Ok(cache) = cache.lock()
        && let Some(cached) = cache.get(&key)
        && cached.resolved_at.elapsed() < PAC_CACHE_TTL
    {
        return Ok(cached.proxy_url.clone());
    }

    let proxy_url = resolve_windows_auto_proxy(target_url.as_str(), pac_url, auto_detect)?;
    if let Ok(mut cache) = cache.lock() {
        cache.insert(
            key,
            CachedProxy {
                resolved_at: Instant::now(),
                proxy_url: proxy_url.clone(),
            },
        );
    }
    Ok(proxy_url)
}

#[cfg(windows)]
fn windows_system_proxy_config() -> Result<Option<WindowsProxyConfig>> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

    let internet_settings = match RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(
        r"Software\Microsoft\Windows\CurrentVersion\Internet Settings",
        KEY_READ,
    ) {
        Ok(key) => key,
        Err(_) => return Ok(None),
    };
    let enabled = internet_settings
        .get_value::<u32, _>("ProxyEnable")
        .unwrap_or(0)
        != 0;
    if enabled {
        let raw = internet_settings
            .get_value::<String, _>("ProxyServer")
            .unwrap_or_default();
        if let Some(proxy_url) = normalize_windows_proxy_server(&raw)? {
            return Ok(Some(WindowsProxyConfig::Fixed(proxy_url)));
        }
    }

    let pac_url = internet_settings
        .get_value::<String, _>("AutoConfigURL")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let auto_detect = internet_settings
        .get_value::<u32, _>("AutoDetect")
        .unwrap_or(0)
        != 0;
    if pac_url.is_some() || auto_detect {
        Ok(Some(WindowsProxyConfig::Auto {
            pac_url,
            auto_detect,
        }))
    } else {
        Ok(None)
    }
}

#[cfg(not(windows))]
fn windows_system_proxy_config() -> Result<Option<WindowsProxyConfig>> {
    Ok(None)
}

#[cfg(windows)]
fn resolve_windows_auto_proxy(
    target_url: &str,
    pac_url: Option<&str>,
    auto_detect: bool,
) -> Result<Option<String>> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_NAMED_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY,
        WINHTTP_AUTO_DETECT_TYPE_DHCP, WINHTTP_AUTO_DETECT_TYPE_DNS_A,
        WINHTTP_AUTOPROXY_AUTO_DETECT, WINHTTP_AUTOPROXY_CONFIG_URL, WINHTTP_AUTOPROXY_OPTIONS,
        WINHTTP_PROXY_INFO, WinHttpCloseHandle, WinHttpGetProxyForUrl, WinHttpOpen,
        WinHttpSetTimeouts,
    };
    use windows::core::{Error, Owned, PCWSTR};

    struct Session(*mut c_void);
    impl Drop for Session {
        fn drop(&mut self) {
            if !self.0.is_null() {
                let _ = unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    let agent = "znnz-agent-launcher\0".encode_utf16().collect::<Vec<_>>();
    let session = Session(unsafe {
        WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        )
    });
    if session.0.is_null() {
        return Err(Error::from_thread()).context("无法创建 Windows PAC 解析会话");
    }
    unsafe { WinHttpSetTimeouts(session.0, 5_000, 5_000, 5_000, 5_000) }
        .context("无法设置 Windows PAC 解析超时")?;

    let target = std::ffi::OsStr::new(target_url)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let pac = pac_url.map(|value| {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    });
    let mut options = WINHTTP_AUTOPROXY_OPTIONS {
        dwFlags: (if pac.is_some() {
            WINHTTP_AUTOPROXY_CONFIG_URL
        } else {
            0
        }) | (if auto_detect {
            WINHTTP_AUTOPROXY_AUTO_DETECT
        } else {
            0
        }),
        dwAutoDetectFlags: if auto_detect {
            WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A
        } else {
            0
        },
        lpszAutoConfigUrl: pac
            .as_ref()
            .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr())),
        fAutoLogonIfChallenged: true.into(),
        ..Default::default()
    };
    let mut info = WINHTTP_PROXY_INFO::default();
    unsafe { WinHttpGetProxyForUrl(session.0, PCWSTR(target.as_ptr()), &mut options, &mut info) }
        .with_context(|| format!("Windows 无法为 {target_url} 解析 PAC 代理"))?;

    let _proxy_memory = if info.lpszProxy.0.is_null() {
        None
    } else {
        Some(unsafe { Owned::new(HGLOBAL(info.lpszProxy.0.cast())) })
    };
    let _bypass_memory = if info.lpszProxyBypass.0.is_null() {
        None
    } else {
        Some(unsafe { Owned::new(HGLOBAL(info.lpszProxyBypass.0.cast())) })
    };
    if info.dwAccessType != WINHTTP_ACCESS_TYPE_NAMED_PROXY || info.lpszProxy.0.is_null() {
        return Ok(None);
    }
    let raw = unsafe { PCWSTR(info.lpszProxy.0).to_string() }
        .context("Windows PAC 返回了无效代理地址")?;
    normalize_resolved_proxy(&raw)
}

#[cfg(not(windows))]
fn resolve_windows_auto_proxy(
    _target_url: &str,
    _pac_url: Option<&str>,
    _auto_detect: bool,
) -> Result<Option<String>> {
    Ok(None)
}

fn normalize_resolved_proxy(raw: &str) -> Result<Option<String>> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("DIRECT") {
        return Ok(None);
    }
    if raw.contains('=') {
        return normalize_windows_proxy_server(raw);
    }
    let selected = raw.split(';').next().unwrap_or(raw).trim();
    let selected = ["PROXY ", "HTTPS ", "HTTP "]
        .iter()
        .find_map(|prefix| {
            selected
                .get(..prefix.len())
                .filter(|head| head.eq_ignore_ascii_case(prefix))
                .map(|_| selected[prefix.len()..].trim())
        })
        .unwrap_or(selected);
    normalize_windows_proxy_server(selected)
}

fn normalize_windows_proxy_server(raw: &str) -> Result<Option<String>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }

    let selected = if raw.contains('=') {
        let entries = raw
            .split(';')
            .filter_map(|entry| entry.trim().split_once('='))
            .map(|(scheme, address)| (scheme.trim().to_ascii_lowercase(), address.trim()))
            .collect::<Vec<_>>();
        entries
            .iter()
            .find(|(scheme, _)| scheme == "https")
            .or_else(|| entries.iter().find(|(scheme, _)| scheme == "http"))
            .map(|(_, address)| *address)
    } else {
        Some(raw)
    };
    let Some(selected) = selected.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    let candidate = if selected.contains("://") {
        selected.to_owned()
    } else {
        format!("http://{selected}")
    };
    let parsed =
        Url::parse(&candidate).with_context(|| format!("Windows 系统代理地址无效: {selected}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("Windows 系统代理不是可用的 HTTP/HTTPS 代理: {selected}");
    }
    Ok(Some(candidate.trim_end_matches('/').to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    fn read_request_headers(stream: &std::net::TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                break;
            }
        }
    }

    #[test]
    fn normalizes_common_windows_fixed_proxy_formats() {
        assert_eq!(
            normalize_windows_proxy_server("127.0.0.1:10808").unwrap(),
            Some("http://127.0.0.1:10808".to_owned())
        );
        assert_eq!(
            normalize_windows_proxy_server("http=127.0.0.1:8080;https=127.0.0.1:10808").unwrap(),
            Some("http://127.0.0.1:10808".to_owned())
        );
        assert_eq!(normalize_windows_proxy_server("").unwrap(), None);
    }

    #[test]
    fn normalizes_winhttp_and_pac_proxy_results() {
        assert_eq!(
            normalize_resolved_proxy("PROXY 127.0.0.1:10808; DIRECT").unwrap(),
            Some("http://127.0.0.1:10808".to_owned())
        );
        assert_eq!(normalize_resolved_proxy("DIRECT").unwrap(), None);
    }

    #[test]
    fn rejects_non_http_windows_proxy_protocols() {
        assert!(normalize_windows_proxy_server("socks5://127.0.0.1:10808").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_winhttp_evaluates_a_pac_url_for_the_target() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_request_headers(&stream);
            let body =
                b"function FindProxyForURL(url, host) { return 'PROXY 127.0.0.1:18080; DIRECT'; }";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-ns-proxy-autoconfig\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        });

        let proxy = resolve_windows_auto_proxy(
            "https://gateway.example.com/v1/models",
            Some(&format!("http://{address}/proxy.pac")),
            false,
        )
        .unwrap();

        assert_eq!(proxy.as_deref(), Some("http://127.0.0.1:18080"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn retries_once_when_no_http_response_is_received() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (first, _) = listener.accept().unwrap();
            read_request_headers(&first);
            drop(first);

            let (mut second, _) = listener.accept().unwrap();
            read_request_headers(&second);
            second
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = send_with_network_retry(client.get(format!("http://{address}/models")))
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "ok");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn does_not_retry_an_explicit_http_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(false).unwrap();
        let server_listener = listener.try_clone().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = server_listener.accept().unwrap();
            read_request_headers(&stream);
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = send_with_network_retry(client.get(format!("http://{address}/models")))
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }
}
