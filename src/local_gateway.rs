use crate::gui_worker::ClientTarget;
use crate::protocol_stream::ChatToMessages as IncrementalChatToMessages;
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures_util::TryStreamExt;
use http::header::{AUTHORIZATION, CONTENT_TYPE, HeaderName, HeaderValue};
use http::{Method, Request, Response, StatusCode, Uri};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use rand::RngCore;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::io::Read;
use std::net::Ipv4Addr;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use url::Url;
use zeroize::Zeroizing;

const MAX_REQUEST_BODY: usize = 64 * 1024 * 1024;
const MAX_ERROR_BODY: usize = 8 * 1024 * 1024;
type BoxError = Box<dyn std::error::Error + Send + Sync>;
type GatewayBody = UnsyncBoxBody<Bytes, BoxError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayInfo {
    #[serde(default)]
    pub usage_schema: u32,
    #[serde(default)]
    pub statistics_backend: Option<Box<GatewayInfo>>,
    pub pid: u32,
    pub base_url: String,
    #[serde(default)]
    pub gateway_url: String,
    #[serde(default)]
    pub configuration_fingerprint: String,
    pub client_token: String,
    pub admin_token: String,
    #[serde(default)]
    pub attached_clients: Vec<String>,
    pub executable_path: PathBuf,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bootstrap {
    #[serde(default)]
    statistics_backend: bool,
    target: ClientTarget,
    gateway_url: String,
    api_key: String,
    #[serde(default)]
    conversion_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateRequest {
    gateway_url: String,
    api_key: String,
    #[serde(default)]
    conversion_enabled: bool,
}

#[derive(Debug, Clone)]
struct Route {
    gateway_url: Url,
    api_key: Zeroizing<String>,
    conversion_enabled: bool,
}

#[derive(Debug)]
struct GatewayState {
    enabled: AtomicBool,
    target: ClientTarget,
    route: RwLock<Route>,
    client_token: String,
    admin_token: String,
    client: Client,
    direct_client: Client,
    usage: crate::usage::Recorder,
    tool_signatures: Arc<Mutex<crate::tool_signatures::Cache>>,
    active: Arc<AtomicUsize>,
}

struct ManagedRoute {
    listener_task: JoinHandle<()>,
    info: GatewayInfo,
    state: Arc<GatewayState>,
    attached: bool,
}

static MANAGED: OnceLock<Mutex<std::collections::HashMap<ClientTarget, ManagedRoute>>> =
    OnceLock::new();
fn managed_routes() -> &'static Mutex<std::collections::HashMap<ClientTarget, ManagedRoute>> {
    MANAGED.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Hold this through the entire response body, including SSE and cancellation.
pub(crate) struct ActiveRequest(Arc<AtomicUsize>);
impl ActiveRequest {
    fn new(active: Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::SeqCst);
        Self(active)
    }
}
impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn track_adapter_request(upstream: &Url) -> Option<ActiveRequest> {
    let routes = managed_routes().lock().unwrap_or_else(|e| e.into_inner());
    routes
        .values()
        .find(|route| {
            route.info.base_url.trim_end_matches('/') == upstream.as_str().trim_end_matches('/')
        })
        .map(|route| ActiveRequest::new(route.state.active.clone()))
}

pub fn mark_reconfiguring(target: ClientTarget) {
    if let Some(route) = managed_routes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&target)
    {
        route.attached = false;
    }
}

/// The gateway process can outlive its client worker. Keep the route available
/// for the next explicit Start • Connect or Restart • Connect action, but stop reporting the
/// client as attached as soon as its managed worker has ended.
pub fn mark_client_disconnected(target: ClientTarget) {
    if let Some(route) = managed_routes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&target)
    {
        route.attached = false;
    }
}

/// Explicit account-mode switching revokes this client's listener, credentials
/// and attachment. Other clients' routes keep running.
pub fn disconnect_for_account(target: ClientTarget) -> Result<()> {
    if let Some(route) = managed_routes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&target)
    {
        route.state.enabled.store(false, Ordering::SeqCst);
        route.listener_task.abort();
        route.state.usage.flush();
        let path = state_path(target)?;
        if read_info_at(&path)?.is_some_and(|info| {
            info.pid == route.info.pid && info.admin_token == route.info.admin_token
        }) {
            crate::backup::remove_file_if_exists(&path.with_extension("attached.json"))?;
            crate::backup::remove_file_if_exists(&path)?;
        }
    }
    Ok(())
}

/// Refresh managed attachment flags from the process detector. This covers a
/// client closed outside Agent-Switch before its worker has emitted cleanup.
pub fn sync_client_processes(running: &[(ClientTarget, u32)]) {
    let mut routes = managed_routes().lock().unwrap_or_else(|e| e.into_inner());
    for (target, route) in routes.iter_mut() {
        if route.attached
            && !running
                .iter()
                .any(|(running_target, _)| running_target == target)
        {
            route.attached = false;
        }
    }
}

pub fn cleanup_managed() {
    let mut routes = managed_routes().lock().unwrap_or_else(|e| e.into_inner());
    for (target, route) in routes.drain() {
        route.state.enabled.store(false, Ordering::SeqCst);
        route.listener_task.abort();
        route.state.usage.flush();
        if let Ok(path) = state_path(target)
            && read_info_at(&path).ok().flatten().is_some_and(|info| {
                info.pid == route.info.pid && info.admin_token == route.info.admin_token
            })
        {
            let _ = fs::remove_file(path.with_extension("attached.json"));
            let _ = fs::remove_file(path);
        }
    }
}

async fn ensure_managed(
    target: ClientTarget,
    url: &Url,
    key: &str,
    conversion_enabled: bool,
) -> Result<GatewayInfo> {
    // One GUI job per client; this additional lock also protects CLI-style callers
    // within the host. No await is performed while holding the routes map lock.
    static UPDATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _update = UPDATE.lock().await;
    {
        let mut routes = managed_routes().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(route) = routes.get_mut(&target) {
            *route
                .state
                .route
                .write()
                .map_err(|_| anyhow::anyhow!("网关路由锁已损坏"))? = Route {
                gateway_url: url.clone(),
                api_key: Zeroizing::new(key.trim().to_owned()),
                conversion_enabled,
            };
            route.info.gateway_url = url.as_str().trim_end_matches('/').to_owned();
            route.info.configuration_fingerprint = configuration_fingerprint(url.as_str(), key)?;
            write_info(target, &route.info)?;
            return Ok(route.info.clone());
        }
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let info = GatewayInfo {
        usage_schema: 1,
        statistics_backend: None,
        pid: std::process::id(),
        base_url: format!("http://127.0.0.1:{}", listener.local_addr()?.port()),
        gateway_url: url.as_str().trim_end_matches('/').to_owned(),
        configuration_fingerprint: configuration_fingerprint(url.as_str(), key)?,
        client_token: random_token(),
        admin_token: random_token(),
        attached_clients: Vec::new(),
        executable_path: std::env::current_exe()?,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let state = Arc::new(GatewayState {
        enabled: AtomicBool::new(true),
        target,
        route: RwLock::new(Route {
            gateway_url: url.clone(),
            api_key: Zeroizing::new(key.trim().to_owned()),
            conversion_enabled,
        }),
        client_token: info.client_token.clone(),
        admin_token: info.admin_token.clone(),
        client: crate::network_proxy::configure_reqwest_builder(Client::builder())?
            .connect_timeout(Duration::from_secs(12))
            .pool_idle_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        direct_client: Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(12))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        usage: crate::usage::Recorder::open(&crate::usage::database_path()?)?,
        tool_signatures: Arc::default(),
        active: Arc::new(AtomicUsize::new(0)),
    });
    write_info(target, &info)?;
    let task_state = state.clone();
    let listener_task = tokio::spawn(async move {
        while let Ok((stream, peer)) = listener.accept().await {
            if !peer.ip().is_loopback() {
                continue;
            }
            let state = task_state.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let state = state.clone();
                    async move { Ok::<_, Infallible>(handle_request(request, state).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    managed_routes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            target,
            ManagedRoute {
                info: info.clone(),
                state,
                attached: false,
                listener_task,
            },
        );
    Ok(info)
}

pub fn state_path(target: ClientTarget) -> Result<PathBuf> {
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TEMP").map(PathBuf::from))
        .context("无法确定本地应用数据目录")?
        .join("Agent-Switch")
        .join(if crate::background::is_managed() {
            "gateway-v3"
        } else {
            "gateway-v2"
        });
    fs::create_dir_all(&root)
        .with_context(|| format!("无法创建本地网关状态目录 {}", root.display()))?;
    Ok(root.join(format!("{}.json", target.id())))
}

fn read_info(target: ClientTarget) -> Result<Option<GatewayInfo>> {
    if crate::background::is_managed() {
        return Ok(managed_routes()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&target)
            .map(|route| route.info.clone()));
    }
    read_info_at(&state_path(target)?)
}

fn read_info_at(path: &std::path::Path) -> Result<Option<GatewayInfo>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw = match fs::read(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("无法读取本地网关状态"),
    };
    match serde_json::from_slice(&raw) {
        Ok(value) => Ok(Some(value)),
        Err(_) => Ok(None),
    }
}

fn write_info(target: ClientTarget, info: &GatewayInfo) -> Result<()> {
    let path = state_path(target)?;
    let bytes = serde_json::to_vec_pretty(info)?;
    crate::backup::atomic_write(&path, &bytes)
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|value| format!("{value:02x}")).collect()
}

fn local_url(info: &GatewayInfo, path: &str) -> String {
    format!("{}{}", info.base_url.trim_end_matches('/'), path)
}

pub async fn ensure_and_update(
    target: ClientTarget,
    gateway_url: &str,
    api_key: &str,
    conversion_enabled: bool,
) -> Result<GatewayInfo> {
    let gateway_url = normalize_url(gateway_url)?;
    if api_key.trim().is_empty() {
        bail!("本地网关更新需要 API Key");
    }
    if crate::background::is_managed() {
        return ensure_managed(target, &gateway_url, api_key, conversion_enabled).await;
    }
    let _lock = crate::platform::try_acquire_named_mutex(&format!(
        r"Local\Agent-Switch-gateway-{}",
        target.id()
    ))?
    .context("本地网关正在更新，请稍后重试")?;
    // gateway-v2 identifies the wire protocol; a GUI version upgrade can reuse its live daemon.
    let mut info = match read_info(target)? {
        Some(info) if crate::platform::process_matches_image(info.pid, &info.executable_path) => {
            info
        }
        _ => spawn_daemon(target, &gateway_url, api_key, conversion_enabled, false).await?,
    };
    update_with_statistics(target, &mut info, &gateway_url, api_key, conversion_enabled).await?;
    info.gateway_url = gateway_url.as_str().trim_end_matches('/').to_owned();
    info.configuration_fingerprint = configuration_fingerprint(gateway_url.as_str(), api_key)?;
    write_info(target, &info)?;
    Ok(info)
}

pub fn attached_gateway_url(target: ClientTarget) -> Option<String> {
    read_info(target)
        .ok()
        .flatten()
        .map(|info| info.gateway_url)
        .filter(|url| !url.is_empty())
}

/// Compare the route actually applied by the worker without retaining another plaintext key.
pub fn configuration_fingerprint(gateway_url: &str, api_key: &str) -> Result<String> {
    let url = normalize_url(gateway_url)?;
    let mut digest = Sha256::new();
    digest.update(b"Agent-Switch-route-v1\0");
    digest.update(url.as_str().trim_end_matches('/').as_bytes());
    digest.update([0]);
    digest.update(api_key.trim().as_bytes());
    Ok(format!("{:x}", digest.finalize()))
}

pub fn attached_configuration_fingerprint(target: ClientTarget) -> Option<String> {
    read_info(target)
        .ok()
        .flatten()
        .filter(statistics_ready)
        .map(|info| info.configuration_fingerprint)
        .filter(|fingerprint| !fingerprint.is_empty())
}

#[derive(Serialize, Deserialize)]
struct Attachment {
    gateway_pid: u32,
    worker_pid: u32,
    worker_image: PathBuf,
    processes: Vec<(u32, String)>,
}

pub fn is_attached(target: ClientTarget) -> bool {
    if crate::background::is_managed() {
        return managed_routes()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&target)
            .is_some_and(|route| route.attached);
    }
    let check = || -> Result<bool> {
        let Some(info) = read_info(target)? else {
            return Ok(false);
        };
        if !crate::platform::process_matches_image(info.pid, &info.executable_path) {
            return Ok(false);
        }
        let attachment: Attachment = serde_json::from_slice(&fs::read(
            state_path(target)?.with_extension("attached.json"),
        )?)?;
        Ok(attachment.gateway_pid == info.pid
            && crate::platform::process_matches_image(
                attachment.worker_pid,
                &attachment.worker_image,
            )
            && attachment.processes.iter().any(|(pid, image)| {
                crate::platform::process_matches_image(*pid, std::path::Path::new(image))
            }))
    };
    check().unwrap_or(false)
}

/// Called only after client startup/readiness succeeds. Legacy CLI launch paths do not register.
pub fn confirm_current_worker(target: ClientTarget) -> Result<()> {
    if crate::background::is_managed() {
        let mut routes = managed_routes().lock().unwrap_or_else(|e| e.into_inner());
        let route = routes.get_mut(&target).context("常驻本地网关状态不存在")?;
        route.attached = true;
        return Ok(());
    }
    let managed = crate::runtime_state::load_active_states()?
        .iter()
        .any(|state| state.client == target && state.worker_pid == std::process::id());
    if !managed {
        return Ok(());
    }
    let info = read_info(target)?.context("常驻本地网关状态不存在")?;
    let processes = crate::platform::running_client_processes()?
        .into_iter()
        .filter(|(client, _)| *client == target)
        .filter_map(|(_, pid)| {
            crate::platform::process_image_path(pid)
                .ok()
                .map(|image| (pid, image))
        })
        .collect::<Vec<_>>();
    if processes.is_empty() {
        bail!("客户端启动后未检测到对应进程");
    }
    let attachment = Attachment {
        gateway_pid: info.pid,
        worker_pid: std::process::id(),
        worker_image: std::env::current_exe()?,
        processes,
    };
    crate::backup::atomic_write(
        &state_path(target)?.with_extension("attached.json"),
        &serde_json::to_vec(&attachment)?,
    )
}

pub async fn update_attached(
    target: ClientTarget,
    gateway_url: &str,
    api_key: &str,
    conversion_enabled: bool,
) -> Result<()> {
    if api_key.trim().is_empty() {
        bail!("本地网关更新需要 API Key");
    }
    if crate::background::is_managed() {
        if !is_attached(target) {
            bail!("客户端尚未接入网关");
        }
        ensure_managed(
            target,
            &normalize_url(gateway_url)?,
            api_key,
            conversion_enabled,
        )
        .await?;
        return Ok(());
    }
    let _lock = crate::platform::try_acquire_named_mutex(&format!(
        r"Local\Agent-Switch-gateway-{}",
        target.id()
    ))?
    .context("本地网关正在更新，请稍后重试")?;
    if !is_attached(target) {
        bail!(
            "{} • 自动重启 • 失败!：当前进程尚未接入此网关",
            target.title()
        );
    }
    let mut info = read_info(target)?.context("本地网关状态不存在")?;
    // Never silently create a new endpoint while the client still uses an old one.
    let gateway_url = normalize_url(gateway_url)?;
    update_with_statistics(target, &mut info, &gateway_url, api_key, conversion_enabled).await?;
    info.gateway_url = gateway_url.as_str().trim_end_matches('/').to_owned();
    info.configuration_fingerprint = configuration_fingerprint(gateway_url.as_str(), api_key)?;
    write_info(target, &info)
}

async fn spawn_daemon(
    target: ClientTarget,
    gateway_url: &Url,
    api_key: &str,
    conversion_enabled: bool,
    statistics_backend: bool,
) -> Result<GatewayInfo> {
    let path = if statistics_backend {
        state_path(target)?.with_extension("statistics.json")
    } else {
        state_path(target)?
    };
    let executable = std::env::current_exe().context("无法确定本地网关可执行文件路径")?;
    let mut command = Command::new(executable);
    command
        .arg("internal-gateway")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    let mut child = command.spawn().context("无法启动常驻本地网关")?;
    let bootstrap = serde_json::to_vec(&Bootstrap {
        statistics_backend,
        target,
        gateway_url: gateway_url.to_string(),
        api_key: api_key.trim().to_owned(),
        conversion_enabled,
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(&bootstrap)
            .context("无法发送本地网关初始配置")?;
    }
    for _ in 0..60 {
        if let Some(info) = read_info_at(&path)?
            && info.pid == child.id()
        {
            return Ok(info);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = child.kill();
    if read_info_at(&path)?.is_some_and(|info| info.pid == child.id()) {
        let _ = fs::remove_file(&path);
    }
    bail!("本地网关未能在规定时间内启动")
}

fn statistics_ready(info: &GatewayInfo) -> bool {
    info.usage_schema >= 1
        || info.statistics_backend.as_ref().is_some_and(|backend| {
            backend.usage_schema >= 1
                && crate::platform::process_matches_image(backend.pid, &backend.executable_path)
        })
}

/// Keep the legacy listener address and its in-flight requests alive during upgrade.
/// Only the new backend records usage; the legacy frontend remains a byte forwarder.
async fn update_with_statistics(
    target: ClientTarget,
    info: &mut GatewayInfo,
    gateway_url: &Url,
    api_key: &str,
    conversion_enabled: bool,
) -> Result<()> {
    if info.usage_schema >= 1 {
        return update(info, gateway_url.as_str(), api_key, conversion_enabled).await;
    }
    let backend_path = state_path(target)?.with_extension("statistics.json");
    let candidate = info
        .statistics_backend
        .as_deref()
        .cloned()
        .or(read_info_at(&backend_path)?);
    let mut backend = match candidate {
        Some(backend)
            if backend.usage_schema >= 1
                && crate::platform::process_matches_image(
                    backend.pid,
                    &backend.executable_path,
                ) =>
        {
            backend
        }
        _ => spawn_daemon(target, gateway_url, api_key, conversion_enabled, true).await?,
    };
    update(&backend, gateway_url.as_str(), api_key, conversion_enabled).await?;
    update(
        info,
        &backend.base_url,
        &backend.client_token,
        conversion_enabled,
    )
    .await?;
    backend.gateway_url = gateway_url.as_str().trim_end_matches('/').to_owned();
    backend.configuration_fingerprint = configuration_fingerprint(gateway_url.as_str(), api_key)?;
    crate::backup::atomic_write(&backend_path, &serde_json::to_vec_pretty(&backend)?)?;
    info.statistics_backend = Some(Box::new(backend));
    Ok(())
}

pub async fn update(
    info: &GatewayInfo,
    gateway_url: &str,
    api_key: &str,
    conversion_enabled: bool,
) -> Result<()> {
    let endpoint = local_url(info, "/admin/update");
    let response = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(8))
        .build()?
        .post(endpoint)
        .bearer_auth(&info.admin_token)
        .json(&UpdateRequest {
            gateway_url: gateway_url.to_owned(),
            api_key: api_key.trim().to_owned(),
            conversion_enabled,
        })
        .send()
        .await
        .context("无法连接常驻本地网关")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("本地网关更新失败 ({status}): {body}");
    }
    Ok(())
}

pub async fn run_daemon() -> Result<()> {
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    let bootstrap: Bootstrap = serde_json::from_slice(&input).context("本地网关初始配置无效")?;
    let route = Route {
        gateway_url: normalize_url(&bootstrap.gateway_url)?,
        api_key: Zeroizing::new(bootstrap.api_key),
        conversion_enabled: bootstrap.conversion_enabled,
    };
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    if !address.ip().is_loopback() {
        bail!("本地网关只能绑定回环地址");
    }
    let info = GatewayInfo {
        usage_schema: 1,
        statistics_backend: None,
        pid: std::process::id(),
        base_url: format!("http://127.0.0.1:{}", address.port()),
        gateway_url: bootstrap.gateway_url.trim_end_matches('/').to_owned(),
        configuration_fingerprint: configuration_fingerprint(
            &bootstrap.gateway_url,
            &route.api_key,
        )?,
        client_token: random_token(),
        admin_token: random_token(),
        attached_clients: Vec::new(),
        executable_path: std::env::current_exe()?,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let usage = crate::usage::Recorder::open(&crate::usage::database_path()?)?;
    let state = Arc::new(GatewayState {
        enabled: AtomicBool::new(true),
        target: bootstrap.target,
        route: RwLock::new(route),
        client_token: info.client_token.clone(),
        admin_token: info.admin_token.clone(),
        client: crate::network_proxy::configure_reqwest_builder(Client::builder())?
            .connect_timeout(Duration::from_secs(12))
            .pool_idle_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        direct_client: Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(12))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        usage,
        tool_signatures: Arc::default(),
        active: Arc::new(AtomicUsize::new(0)),
    });
    let published_path = if bootstrap.statistics_backend {
        state_path(bootstrap.target)?.with_extension("statistics.json")
    } else {
        state_path(bootstrap.target)?
    };
    crate::backup::atomic_write(&published_path, &serde_json::to_vec_pretty(&info)?)?;
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let task_state = state.clone();
    let task: JoinHandle<()> = tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => if changed.is_err() || *shutdown_rx.borrow() { break },
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) if peer.ip().is_loopback() => {
                        let state = task_state.clone();
                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let service = service_fn(move |request| {
                                let state = state.clone();
                                async move { Ok::<_, Infallible>(handle_request(request, state).await) }
                            });
                            let _ = hyper::server::conn::http1::Builder::new()
                                .keep_alive(true)
                                .serve_connection(io, service)
                                .await;
                        });
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
    });
    tokio::signal::ctrl_c().await.ok();
    let _ = shutdown.send(true);
    task.abort();
    if read_info_at(&published_path)?.is_some_and(|current| current.pid == info.pid) {
        let _ = fs::remove_file(published_path);
    }
    Ok(())
}

async fn handle_request(
    request: Request<Incoming>,
    state: Arc<GatewayState>,
) -> Response<GatewayBody> {
    if !state.enabled.load(Ordering::SeqCst) {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "gateway_disconnected");
    }
    if request.method() == Method::OPTIONS {
        return empty(StatusCode::NO_CONTENT);
    }
    let path = request.uri().path();
    if path == "/health" && request.method() == Method::GET {
        return json_response(StatusCode::OK, json!({"ok": true}));
    }
    if path == "/admin/update" && request.method() == Method::POST {
        if !authorized(request.headers(), &state.admin_token) {
            return json_error(StatusCode::UNAUTHORIZED, "unauthorized");
        }
        let body = match Limited::new(request.into_body(), 1024 * 1024)
            .collect()
            .await
        {
            Ok(body) => body.to_bytes(),
            Err(_) => return json_error(StatusCode::BAD_REQUEST, "invalid_body"),
        };
        let update: UpdateRequest = match serde_json::from_slice(&body) {
            Ok(value) => value,
            Err(_) => return json_error(StatusCode::BAD_REQUEST, "invalid_update"),
        };
        let route = match normalize_url(&update.gateway_url) {
            Ok(gateway_url) if !update.api_key.trim().is_empty() => Route {
                gateway_url,
                api_key: Zeroizing::new(update.api_key),
                conversion_enabled: update.conversion_enabled,
            },
            _ => return json_error(StatusCode::BAD_REQUEST, "invalid_gateway"),
        };
        if let Ok(mut current) = state.route.write() {
            *current = route;
        } else {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "route_update_failed");
        }
        return json_response(StatusCode::OK, json!({"ok": true}));
    }
    if !authorized(request.headers(), &state.client_token) {
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    if !is_api_path(path) {
        return json_error(StatusCode::NOT_FOUND, "not_found");
    }
    match forward(request, &state).await {
        Ok(response) => response,
        Err(error) => json_response(
            StatusCode::BAD_GATEWAY,
            json!({"error": {"type": "upstream_request_failed", "message": format!("{error:#}")}}),
        ),
    }
}

async fn forward(
    request: Request<Incoming>,
    state: &GatewayState,
) -> Result<Response<GatewayBody>> {
    let active = ActiveRequest::new(state.active.clone());
    let (parts, body) = request.into_parts();
    let (mut target, api_key, configuration, conversion_enabled, upstream_base) = {
        let route = state
            .route
            .read()
            .map_err(|_| anyhow::anyhow!("网关路由锁已损坏"))?;
        (
            upstream_url(&route.gateway_url, &parts.uri)?,
            route.api_key.clone(),
            crate::usage::configuration_id(route.gateway_url.as_str(), route.api_key.as_str()),
            route.conversion_enabled,
            route.gateway_url.clone(),
        )
    };
    let source_protocol = crate::protocol::protocol_for_path(parts.uri.path());
    let client = if matches!(
        target.host_str(),
        Some("127.0.0.1" | "localhost" | "::1" | "[::1]")
    ) {
        &state.direct_client
    } else {
        &state.client
    };
    let bytes = Limited::new(body, MAX_REQUEST_BODY)
        .collect()
        .await
        .map_err(|error| anyhow::anyhow!("读取请求体失败: {error}"))?
        .to_bytes();
    let request_model = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
    let log_model = request_model
        .as_deref()
        .filter(|_| upstream_base.host_str() == Some("generativelanguage.googleapis.com"))
        .map(|model| model.strip_prefix("models/").unwrap_or(model).to_owned())
        .or_else(|| request_model.clone());
    // Codex Desktop sends lightweight POST /responses capability probes with
    // no JSON model. They are not model requests and should not pollute the
    // request log, regardless of whether the provider answers 404 or 405.
    let probe_request = state.target == ClientTarget::CodexDesktop
        && parts.method == Method::POST
        && parts.uri.path().trim_end_matches('/') == "/responses"
        && request_model.is_none();
    let target_protocol = if conversion_enabled {
        source_protocol.map(|source| {
            crate::protocol::target_for_request(
                source,
                request_model.as_deref(),
                upstream_base.as_str(),
            )
        })
    } else {
        source_protocol
    };
    let streaming = request_is_streaming(&bytes);
    if source_protocol != target_protocol {
        remove_client_protocol_query(&mut target);
    }
    let mut request_usage = target_protocol
        .or(source_protocol)
        .map(crate::usage::protocol_for_wire)
        .map(|protocol| {
            let mut metadata = crate::usage::RequestMetadata {
                client: state.target.title().to_owned(),
                model: log_model.clone(),
                request_kind: if probe_request { "probe" } else { "inference" }.to_owned(),
                endpoint: parts.uri.path().to_owned(),
                conversion: None,
                streaming,
            };
            if let (Some(source), Some(destination)) = (source_protocol, target_protocol)
                && source != destination
            {
                metadata.conversion = Some(format!(
                    "{} → {}",
                    crate::usage::protocol_for_wire(source).label(),
                    crate::usage::protocol_for_wire(destination).label()
                ));
            }
            state
                .usage
                .start_with_metadata(configuration.clone(), protocol, metadata)
        });
    if let Some(destination) = target_protocol
        && source_protocol != Some(destination)
    {
        if destination == crate::protocol::WireProtocol::Gemini {
            let model = request_model.as_deref().unwrap_or("gemini");
            let root = if upstream_base.path().trim_end_matches('/').is_empty() {
                "/v1beta"
            } else {
                upstream_base.path().trim_end_matches('/')
            };
            let action = if request_is_streaming(&bytes) {
                "streamGenerateContent"
            } else {
                "generateContent"
            };
            target.set_path(&format!("{root}/models/{model}:{action}"));
            if request_is_streaming(&bytes) {
                target.set_query(Some("alt=sse"));
            }
        } else {
            let path = match destination {
                crate::protocol::WireProtocol::Responses => "/v1/responses",
                crate::protocol::WireProtocol::Messages => "/v1/messages",
                crate::protocol::WireProtocol::ChatCompletions => "/v1/chat/completions",
                crate::protocol::WireProtocol::Gemini => unreachable!(),
            };
            target.set_path(&crate::gateway::upstream_api_path(&upstream_base, path));
        }
    }
    let request_bytes = if conversion_enabled {
        if let (Some(source), Some(target_protocol)) = (source_protocol, target_protocol) {
            // Run the adapter even when source and target protocols match:
            // provider-specific request flags (for example Gemini Responses
            // built-in tool support) still need to be added in that case.
            serde_json::from_slice::<Value>(&bytes)
                .map(|value| {
                    let mut converted =
                        crate::protocol::convert_request(value, source, target_protocol);
                    if target_protocol == crate::protocol::WireProtocol::ChatCompletions
                        && let Ok(cache) = state.tool_signatures.lock()
                    {
                        cache.restore(&configuration, &mut converted);
                    }
                    converted
                })
                .ok()
                .and_then(|value| serde_json::to_vec(&value).ok())
                .map(Bytes::from)
                .unwrap_or(bytes.clone())
        } else {
            bytes.clone()
        }
    } else {
        bytes.clone()
    };
    let model_catalog_request =
        parts.method == Method::GET && canonical_api_path(parts.uri.path()) == "/v1/models";
    let mut request_builder = client.request(parts.method, target);
    for (name, value) in &parts.headers {
        if is_secret_or_hop(name)
            || name == HeaderName::from_static("host")
            || (source_protocol != target_protocol
                && name == HeaderName::from_static("accept-encoding"))
        {
            continue;
        }
        request_builder = request_builder.header(name, value);
    }
    if model_catalog_request {
        // Catalog format can be selected by authentication headers. Preserve
        // the caller's protocol instead of making OpenAI requests look Gemini
        // or Anthropic requests (runapi.co and LiteAPI do this).
        request_builder = if parts.headers.contains_key("x-goog-api-key") {
            request_builder.header("x-goog-api-key", api_key.as_str())
        } else if parts.headers.contains_key("anthropic-version")
            || parts.headers.contains_key("x-api-key")
        {
            request_builder.header("x-api-key", api_key.as_str())
        } else {
            request_builder.header(AUTHORIZATION, format!("Bearer {}", api_key.as_str()))
        };
    } else {
        request_builder = request_builder
            .header(AUTHORIZATION, format!("Bearer {}", api_key.as_str()))
            .header("x-api-key", api_key.as_str())
            .header("x-goog-api-key", api_key.as_str());
    }
    if source_protocol != target_protocol {
        // The adapter parses upstream JSON/SSE bytes and emits a new body.
        // Request uncompressed bytes so downstream Accept-Encoding cannot
        // turn them into a gzip/brotli payload that the adapter cannot parse.
        request_builder = request_builder.header("accept-encoding", "identity");
    }
    let response = request_builder
        .body(request_bytes)
        .send()
        .await
        .map_err(|error| {
            // reqwest URLs can contain sensitive request query parameters. Do not log them.
            let category = if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connect/DNS/TLS/proxy"
            } else {
                "request"
            };
            let detail = format!("{:#}", anyhow::Error::new(error.without_url()))
                .replace(api_key.as_str(), "[REDACTED]");
            anyhow::anyhow!("上游连接失败 ({category})：{detail}")
        })?;
    let status = response.status();
    if let Some(usage) = request_usage.as_mut() {
        usage.set_status_code(status.as_u16());
    }
    let headers = response.headers().clone();
    if !status.is_success()
        && response
            .content_length()
            .is_some_and(|len| len > MAX_ERROR_BODY as u64)
    {
        bail!("上游错误响应过大");
    }
    let mut output = Response::builder().status(status);
    if let Some(headers_out) = output.headers_mut() {
        for (name, value) in &headers {
            if name != HeaderName::from_static("content-length")
                && name != HeaderName::from_static("transfer-encoding")
                && !(status.is_success()
                    && source_protocol != target_protocol
                    && name == HeaderName::from_static("content-encoding"))
            {
                headers_out.insert(name, value.clone());
            }
        }
        // The local catalog endpoint is also used to build client model
        // menus. Preserve the upstream URL so those menus do not display the
        // loopback gateway address as their provider source.
        if let Ok(value) = HeaderValue::from_str(upstream_base.as_str()) {
            headers_out.insert(HeaderName::from_static("x-agent-switch-upstream"), value);
        }
    }
    if let Some(usage) = request_usage.as_mut() {
        usage.http_success = status.is_success();
        if let Some(content_type) = headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
        {
            usage.parser.set_content_type(content_type);
        }
    }
    if let (Some(source), Some(destination)) = (target_protocol, source_protocol)
        && source != destination
        && status.is_success()
    {
        let content_type = headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if content_type.contains("text/event-stream")
            && source == crate::protocol::WireProtocol::ChatCompletions
            && destination == crate::protocol::WireProtocol::Messages
        {
            let mut tracker =
                crate::tool_signatures::Tracker::new(state.tool_signatures.clone(), configuration);
            let upstream_stream =
                futures_util::StreamExt::map(response.bytes_stream(), move |chunk| {
                    if let Ok(bytes) = &chunk {
                        tracker.sse(bytes);
                    }
                    chunk
                });
            let converted_stream =
                incremental_chat_to_messages_stream(upstream_stream, request_usage, active);
            return output
                .body(StreamBody::new(converted_stream).boxed_unsync())
                .context("构造流式协议转换响应失败");
        }
        let upstream_body = response.bytes().await.context("读取协议转换响应失败")?;
        if source == crate::protocol::WireProtocol::ChatCompletions {
            let mut tracker =
                crate::tool_signatures::Tracker::new(state.tool_signatures.clone(), configuration);
            if content_type.contains("text/event-stream") {
                tracker.sse(&upstream_body);
            } else {
                tracker.json(&upstream_body);
            }
        }
        if let Some(usage) = request_usage.as_mut() {
            if !upstream_body.is_empty() {
                usage.mark_first_byte();
            }
            usage.parser.feed(&upstream_body);
        }
        let mut converted = convert_response_body(&upstream_body, source, destination);
        if destination == crate::protocol::WireProtocol::Responses
            && let (Ok(mut value), Ok(request)) = (
                serde_json::from_slice::<Value>(&converted),
                serde_json::from_slice::<Value>(&bytes),
            )
        {
            crate::protocol::restore_responses_tool_names(&mut value, &request);
            if let Ok(body) = serde_json::to_vec(&value) {
                converted = Bytes::from(body);
            }
        }
        let mut output = Response::builder().status(status);
        if let Some(headers_out) = output.headers_mut() {
            for (name, value) in &headers {
                if name != HeaderName::from_static("content-length")
                    && name != HeaderName::from_static("transfer-encoding")
                    && name != HeaderName::from_static("content-encoding")
                    && name != CONTENT_TYPE
                {
                    headers_out.insert(name, value.clone());
                }
            }
            let converted_type = if content_type.contains("text/event-stream") {
                "text/event-stream"
            } else {
                "application/json"
            };
            headers_out.insert(CONTENT_TYPE, converted_type.parse().unwrap());
        }
        let payload = if content_type.contains("text/event-stream") {
            format_converted_sse(&converted, destination)
        } else {
            converted
        };
        return output
            .body(
                Full::new(payload)
                    .map_err(|never| -> BoxError { match never {} })
                    .boxed_unsync(),
            )
            .context("构造协议转换响应失败")
            .inspect(|_response| {
                if let Some(mut usage) = request_usage {
                    usage.finish(true);
                }
            });
    }
    let bytes_stream = response.bytes_stream();
    let stream = futures_util::stream::unfold(
        (bytes_stream, request_usage, active),
        |(mut bytes_stream, mut request_usage, active)| async move {
            match bytes_stream.try_next().await {
                Ok(Some(bytes)) => {
                    if let Some(usage) = request_usage.as_mut() {
                        if !bytes.is_empty() {
                            usage.mark_first_byte();
                        }
                        usage.parser.feed(&bytes);
                    }
                    Some((
                        Ok::<_, BoxError>(Frame::data(bytes)),
                        (bytes_stream, request_usage, active),
                    ))
                }
                Ok(None) => {
                    if let Some(mut usage) = request_usage {
                        usage.finish(true);
                    }
                    None
                }
                Err(error) => {
                    if let Some(mut usage) = request_usage {
                        usage.finish(false);
                    }
                    Some((
                        Err::<Frame<Bytes>, BoxError>(Box::new(error)),
                        (bytes_stream, None, active),
                    ))
                }
            }
        },
    );
    output
        .body(StreamBody::new(stream).boxed_unsync())
        .context("构造网关响应失败")
}

/// Convert the common Chat Completions SSE stream to Anthropic Messages SSE
/// incrementally. The old fallback collects the complete response, which makes
/// first-byte latency equal total latency for Claude clients. This path keeps
/// text deltas flowing as soon as the upstream emits them.
fn incremental_chat_to_messages_stream<S>(
    bytes_stream: S,
    request_usage: Option<crate::usage::RequestUsage>,
    active: ActiveRequest,
) -> impl futures_util::Stream<Item = Result<Frame<Bytes>, BoxError>>
where
    S: futures_util::Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
{
    futures_util::stream::unfold(
        (
            bytes_stream,
            IncrementalChatToMessages::default(),
            VecDeque::<Bytes>::new(),
            request_usage,
            active,
            false,
        ),
        |(
            mut bytes_stream,
            mut converter,
            mut pending,
            mut request_usage,
            active,
            mut upstream_done,
        )| async move {
            loop {
                if let Some(bytes) = pending.pop_front() {
                    // Completion is a protocol event, not a TCP EOF. Some
                    // clients stop reading immediately after message_stop.
                    if upstream_done
                        && pending.is_empty()
                        && let Some(mut usage) = request_usage.take()
                    {
                        usage.finish(converter.succeeded());
                    }
                    return Some((
                        Ok::<_, BoxError>(Frame::data(bytes)),
                        (
                            bytes_stream,
                            converter,
                            pending,
                            request_usage,
                            active,
                            upstream_done,
                        ),
                    ));
                }
                if upstream_done {
                    if let Err(error) = converter.finish(&mut pending) {
                        pending.clear();
                        converter.abort();
                        if let Some(mut usage) = request_usage.take() {
                            usage.finish(false);
                        }
                        return Some((
                            Err::<Frame<Bytes>, BoxError>(error.into()),
                            (
                                bytes_stream,
                                converter,
                                pending,
                                request_usage,
                                active,
                                true,
                            ),
                        ));
                    }
                    if let Some(bytes) = pending.pop_front() {
                        return Some((
                            Ok::<_, BoxError>(Frame::data(bytes)),
                            (
                                bytes_stream,
                                converter,
                                pending,
                                request_usage,
                                active,
                                upstream_done,
                            ),
                        ));
                    }
                    if let Some(mut usage) = request_usage.take() {
                        usage.finish(converter.succeeded());
                    }
                    return None;
                }
                match bytes_stream.try_next().await {
                    Ok(Some(bytes)) => {
                        if let Some(usage) = request_usage.as_mut() {
                            if !bytes.is_empty() {
                                usage.mark_first_byte();
                            }
                            usage.parser.feed(&bytes);
                        }
                        if let Err(error) = converter.push(&bytes, &mut pending) {
                            pending.clear();
                            converter.abort();
                            if let Some(mut usage) = request_usage.take() {
                                usage.finish(false);
                            }
                            return Some((
                                Err::<Frame<Bytes>, BoxError>(error.into()),
                                (
                                    bytes_stream,
                                    converter,
                                    pending,
                                    request_usage,
                                    active,
                                    true,
                                ),
                            ));
                        }
                        upstream_done = converter.ended();
                    }
                    Ok(None) => {
                        upstream_done = true;
                    }
                    Err(error) => {
                        converter.abort();
                        pending.clear();
                        if let Some(mut usage) = request_usage.take() {
                            usage.finish(false);
                        }
                        return Some((
                            Err::<Frame<Bytes>, BoxError>(Box::new(error)),
                            (
                                bytes_stream,
                                converter,
                                pending,
                                request_usage,
                                active,
                                true,
                            ),
                        ));
                    }
                }
            }
        },
    )
}

fn convert_response_body(
    body: &[u8],
    from: crate::protocol::WireProtocol,
    to: crate::protocol::WireProtocol,
) -> Bytes {
    let payload = if body.windows(5).any(|window| window == b"data:") {
        let events = body
            .split(|byte| *byte == b'\n')
            .filter_map(|line| {
                line.strip_prefix(b"data:").map(|value| {
                    value
                        .iter()
                        .copied()
                        .skip_while(|byte| byte.is_ascii_whitespace())
                        .collect::<Vec<_>>()
                })
            })
            .filter(|value| *value != b"[DONE]")
            .filter_map(|value| serde_json::from_slice::<Value>(&value).ok())
            .collect::<Vec<_>>();
        aggregate_stream_events(&events, from)
    } else {
        serde_json::from_slice::<Value>(body).ok()
    };
    let converted = payload.map(|value| {
        if value.get("error").is_some_and(|error| !error.is_null()) { value } else { crate::protocol::convert_response(value, from, to) }
    }).unwrap_or_else(|| json!({"error":{"message":"protocol conversion returned an invalid or incomplete response"}}));
    Bytes::from(serde_json::to_vec(&converted).unwrap_or_else(|_| b"{}".to_vec()))
}

fn aggregate_stream_events(
    events: &[Value],
    protocol: crate::protocol::WireProtocol,
) -> Option<Value> {
    if let Some(error) = events
        .iter()
        .find(|event| event.get("error").is_some_and(|error| !error.is_null()))
    {
        return Some(error.clone());
    }
    let last = events.last()?.clone();
    match protocol {
        crate::protocol::WireProtocol::ChatCompletions => {
            let mut result = events
                .iter()
                .find(|v| v.get("id").is_some())
                .cloned()
                .unwrap_or(last);
            let mut text = String::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            for event in events {
                if let Some(part) = event
                    .pointer("/choices/0/delta/content")
                    .and_then(Value::as_str)
                {
                    text.push_str(part);
                }
                if let Some(usage) = event.get("usage").filter(|usage| !usage.is_null()) {
                    result["usage"] = usage.clone();
                }
                if let Some(deltas) = event
                    .pointer("/choices/0/delta/tool_calls")
                    .and_then(Value::as_array)
                {
                    for delta in deltas {
                        let index = delta
                            .get("index")
                            .and_then(Value::as_u64)
                            .unwrap_or(tool_calls.len() as u64)
                            as usize;
                        if index >= 128 {
                            return None;
                        }
                        while tool_calls.len() <= index {
                            tool_calls.push(json!({"id":"call_agent_switch","type":"function","function":{"name":"","arguments":""}}));
                        }
                        let call = &mut tool_calls[index];
                        if let Some(id) = delta.get("id") {
                            call["id"] = id.clone();
                        }
                        if let Some(name) = delta.pointer("/function/name").and_then(Value::as_str)
                        {
                            call["function"]["name"] = Value::String(name.to_owned());
                        }
                        if let Some(arguments) =
                            delta.pointer("/function/arguments").and_then(Value::as_str)
                        {
                            let current = call
                                .pointer("/function/arguments")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            call["function"]["arguments"] =
                                Value::String(format!("{current}{arguments}"));
                        }
                    }
                }
            }
            if !text.is_empty() {
                result["choices"][0]["message"] = json!({"role":"assistant","content":text});
                result["choices"][0]["finish_reason"] = json!("stop");
            }
            if !tool_calls.is_empty() {
                result["choices"][0]["message"]["role"] = json!("assistant");
                if result.pointer("/choices/0/message/content").is_none() {
                    result["choices"][0]["message"]["content"] = Value::Null;
                }
                result["choices"][0]["message"]["tool_calls"] = Value::Array(tool_calls);
                result["choices"][0]["finish_reason"] = json!("tool_calls");
            }
            Some(result)
        }
        crate::protocol::WireProtocol::Messages => {
            let mut result = json!({"type":"message","role":"assistant","content":[]});
            let mut blocks = std::collections::BTreeMap::<usize, Value>::new();
            let mut arguments = std::collections::BTreeMap::<usize, String>::new();
            let mut stopped = false;
            for event in events {
                match event["type"].as_str() {
                    Some("message_start") => {
                        result = event.get("message")?.clone();
                    }
                    Some("content_block_start") => {
                        let index = event["index"].as_u64()? as usize;
                        if index >= 128 {
                            return None;
                        }
                        blocks.insert(index, event.get("content_block")?.clone());
                    }
                    Some("content_block_delta") => {
                        let index = event["index"].as_u64()? as usize;
                        let block = blocks.get_mut(&index)?;
                        match event["delta"]["type"].as_str() {
                            Some("text_delta") => {
                                let text = block["text"].as_str().unwrap_or_default();
                                block["text"] =
                                    json!(format!("{text}{}", event["delta"]["text"].as_str()?));
                            }
                            Some("input_json_delta") => {
                                arguments
                                    .entry(index)
                                    .or_default()
                                    .push_str(event["delta"]["partial_json"].as_str()?);
                            }
                            _ => {}
                        }
                    }
                    Some("message_delta") => {
                        if let Some(delta) = event["delta"].as_object() {
                            for (key, value) in delta {
                                result[key] = value.clone();
                            }
                        }
                        if let Some(usage) = event["usage"].as_object() {
                            if !result["usage"].is_object() {
                                result["usage"] = json!({});
                            }
                            for (key, value) in usage {
                                result["usage"][key] = value.clone();
                            }
                        }
                    }
                    Some("message_stop") => {
                        stopped = true;
                    }
                    Some("error") => {
                        return None;
                    }
                    _ => {}
                }
            }
            // An interrupted stream must not become a completed Responses reply.
            if !stopped {
                return None;
            }
            for (index, json) in arguments {
                if !json.is_empty() {
                    blocks.get_mut(&index)?["input"] = serde_json::from_str(&json).ok()?;
                }
            }
            result["content"] = Value::Array(blocks.into_values().collect());
            Some(result)
        }
        crate::protocol::WireProtocol::Responses => {
            let mut result = events
                .iter()
                .rev()
                .find_map(|v| v.get("response").cloned())
                .unwrap_or(last);
            let mut text = String::new();
            for event in events {
                if let Some(part) = event.get("delta").and_then(Value::as_str) {
                    text.push_str(part);
                }
            }
            if !text.is_empty() {
                result["output_text"] = Value::String(text.clone());
                result["output"] = json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]}]);
            }
            Some(result)
        }
        crate::protocol::WireProtocol::Gemini => {
            let mut text = String::new();
            let mut result = last;
            for event in events {
                if let Some(s) = event
                    .pointer("/candidates/0/content/parts/0/text")
                    .and_then(Value::as_str)
                {
                    text.push_str(s);
                }
                if event.get("usageMetadata").is_some() {
                    result = event.clone();
                }
            }
            result["candidates"] =
                json!([{"content":{"role":"model","parts":[{"text":text}]},"finishReason":"STOP"}]);
            Some(result)
        }
    }
}

fn format_converted_sse(body: &[u8], protocol: crate::protocol::WireProtocol) -> Bytes {
    let value: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
    let text = value
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .or_else(|| value.get("output_text").and_then(Value::as_str))
        .or_else(|| value.pointer("/content/0/text").and_then(Value::as_str))
        .unwrap_or_default();
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("agent-switch-converted");
    let model = value
        .get("model")
        .cloned()
        .unwrap_or_else(|| json!("unknown"));
    let frame = |event: Option<&str>, data: Value| {
        let event = event.or_else(|| {
            if protocol == crate::protocol::WireProtocol::Messages {
                data.get("type").and_then(Value::as_str)
            } else {
                None
            }
        });
        match event {
            Some(name) => format!(
                "event: {name}\ndata: {}\n\n",
                serde_json::to_string(&data).unwrap_or_else(|_| "{}".into())
            ),
            None => format!(
                "data: {}\n\n",
                serde_json::to_string(&data).unwrap_or_else(|_| "{}".into())
            ),
        }
    };
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return Bytes::from(frame(Some("error"), json!({"type":"error","error":error})));
    }
    let output = match protocol {
        crate::protocol::WireProtocol::ChatCompletions => {
            let first = json!({"id":id,"object":"chat.completion.chunk","created":0,"model":model,"choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]});
            let last = json!({"id":id,"object":"chat.completion.chunk","created":0,"model":model,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
            format!(
                "{}{}data: [DONE]\n\n",
                frame(None, first),
                frame(None, last)
            )
        }
        crate::protocol::WireProtocol::Messages => {
            let blocks = value
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let stop_reason = value
                .get("stop_reason")
                .and_then(Value::as_str)
                .unwrap_or("end_turn");
            let usage = value.get("usage").cloned().unwrap_or_else(|| json!({}));
            let start = json!({"type":"message_start","message":{"id":id,"type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":usage}});
            let mut output = String::new();
            for (index, block) in blocks.iter().enumerate() {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let block_start = json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":block.get("id").cloned().unwrap_or_else(||json!("call_agent_switch")),"name":block.get("name").cloned().unwrap_or(Value::Null),"input":{}}});
                        let partial_json =
                            serde_json::to_string(block.get("input").unwrap_or(&Value::Null))
                                .unwrap_or_else(|_| "{}".into());
                        let delta = json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":partial_json}});
                        let block_stop = json!({"type":"content_block_stop","index":index});
                        output.push_str(&frame(None, block_start));
                        output.push_str(&frame(None, delta));
                        output.push_str(&frame(None, block_stop));
                    }
                    _ => {
                        let block_start = json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}});
                        let block_text = block.get("text").and_then(Value::as_str).unwrap_or("");
                        let delta = json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":block_text}});
                        let block_stop = json!({"type":"content_block_stop","index":index});
                        output.push_str(&frame(None, block_start));
                        output.push_str(&frame(None, delta));
                        output.push_str(&frame(None, block_stop));
                    }
                }
            }
            let message_delta = json!({"type":"message_delta","delta":{"stop_reason":stop_reason,"stop_sequence":null},"usage":usage});
            format!(
                "{}{}{}",
                frame(None, start),
                output,
                frame(None, message_delta) + &frame(None, json!({"type":"message_stop"}))
            )
        }
        crate::protocol::WireProtocol::Responses => {
            let mut sequence = 0;
            let mut output = String::new();
            let mut emit = |mut event: Value| {
                event["sequence_number"] = json!(sequence);
                sequence += 1;
                output.push_str(&frame(event["type"].as_str(), event.clone()));
            };
            let mut initial = value.clone();
            initial["status"] = json!("in_progress");
            initial["output"] = json!([]);
            emit(json!({"type":"response.created","response":initial}));
            emit(json!({"type":"response.in_progress","response":initial}));
            if let Some(items) = value["output"].as_array() {
                for (index, item) in items.iter().enumerate() {
                    let mut added = item.clone();
                    added["status"] = json!("in_progress");
                    let kind = item["type"].as_str().unwrap_or_default();
                    match kind {
                        "message" => {
                            added["content"] = json!([]);
                        }
                        "function_call" => {
                            added["arguments"] = json!("");
                        }
                        "custom_tool_call" => {
                            added["input"] = json!("");
                        }
                        _ => {}
                    }
                    emit(
                        json!({"type":"response.output_item.added","output_index":index,"item":added}),
                    );
                    match kind {
                        "message" => {
                            if let Some(parts) = item["content"].as_array() {
                                for (content_index, part) in parts.iter().enumerate() {
                                    let mut empty = part.clone();
                                    empty["text"] = json!("");
                                    emit(
                                        json!({"type":"response.content_part.added","item_id":item["id"],"output_index":index,"content_index":content_index,"part":empty}),
                                    );
                                    emit(
                                        json!({"type":"response.output_text.delta","item_id":item["id"],"output_index":index,"content_index":content_index,"delta":part["text"]}),
                                    );
                                    emit(
                                        json!({"type":"response.output_text.done","item_id":item["id"],"output_index":index,"content_index":content_index,"text":part["text"]}),
                                    );
                                    emit(
                                        json!({"type":"response.content_part.done","item_id":item["id"],"output_index":index,"content_index":content_index,"part":part}),
                                    );
                                }
                            }
                        }
                        "function_call" | "custom_tool_call" => {
                            let (event, field) = if kind == "function_call" {
                                ("function_call_arguments", "arguments")
                            } else {
                                ("custom_tool_call_input", "input")
                            };
                            emit(
                                json!({"type":format!("response.{event}.delta"),"item_id":item["id"],"output_index":index,"delta":item[field]}),
                            );
                            let mut done = json!({"type":format!("response.{event}.done"),"item_id":item["id"],"output_index":index});
                            done[field] = item[field].clone();
                            emit(done);
                        }
                        _ => {}
                    }
                    emit(
                        json!({"type":"response.output_item.done","output_index":index,"item":item}),
                    );
                }
            }
            emit(json!({"type":"response.completed","response":value}));
            output
        }
        crate::protocol::WireProtocol::Gemini => format!(
            "data: {}\n\n",
            serde_json::to_string(&value).unwrap_or_else(|_| "{}".into())
        ),
    };
    Bytes::from(output)
}

fn authorized(headers: &http::HeaderMap, token: &str) -> bool {
    let bearer = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok());
    bearer.is_some_and(|value| {
        value
            .strip_prefix("Bearer ")
            .is_some_and(|value| value == token)
    }) || x_api_key == Some(token)
}

fn is_secret_or_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization"
            | "x-api-key"
            | "host"
            | "connection"
            | "content-length"
            | "transfer-encoding"
    )
}

fn remove_client_protocol_query(target: &mut Url) {
    // Claude's beta flag is meaningful for Messages, not Chat/Gemini APIs.
    // Preserve unrelated parameters rather than dropping the whole query.
    let pairs: Vec<(String, String)> = target
        .query_pairs()
        .filter(|(name, _)| name != "beta")
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    target.set_query(None);
    if !pairs.is_empty() {
        target.query_pairs_mut().extend_pairs(pairs);
    }
}

fn upstream_url(base: &Url, uri: &Uri) -> Result<Url> {
    let incoming = canonical_api_path(uri.path());
    let path = crate::gateway::upstream_api_path(base, &incoming);
    let mut target = base.clone();
    target.set_path(&path);
    target.set_query(uri.query());
    Ok(target)
}

fn is_api_path(path: &str) -> bool {
    matches!(
        path,
        "/responses"
            | "/messages"
            | "/models"
            | "/generateContent"
            | "/streamGenerateContent"
            | "/v1"
            | "/v1/"
    ) || path.starts_with("/v1/")
}

fn request_is_streaming(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value.get("stream").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn canonical_api_path(path: &str) -> String {
    if path == "/v1" || path.starts_with("/v1/") {
        path.to_owned()
    } else {
        format!("/v1{path}")
    }
}

fn normalize_url(value: &str) -> Result<Url> {
    let mut url = Url::parse(value.trim()).context("网关地址无效")?;
    let host = url.host_str().context("网关地址缺少主机名")?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("网关地址不能包含用户名或密码");
    }
    if url.scheme() != "https"
        && !(url.scheme() == "http" && matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]"))
    {
        bail!("网关必须使用 HTTPS；仅本机回环地址允许 HTTP");
    }
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url)
}

fn json_response(status: StatusCode, value: Value) -> Response<GatewayBody> {
    let bytes = serde_json::to_vec(&value)
        .unwrap_or_else(|_| br#"{"error":"serialization_failed"}"#.to_vec());
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json; charset=utf-8")
        .body(full(Bytes::from(bytes)))
        .expect("static gateway response")
}

fn json_error(status: StatusCode, code: &str) -> Response<GatewayBody> {
    json_response(status, json!({"error": {"type": code, "message": code}}))
}

fn empty(status: StatusCode) -> Response<GatewayBody> {
    Response::builder()
        .status(status)
        .body(full(Bytes::new()))
        .expect("static gateway response")
}

fn full(bytes: Bytes) -> GatewayBody {
    Full::new(bytes)
        .map_err(|never| match never {})
        .boxed_unsync()
}

#[cfg(test)]
mod tests {
    #[test]
    fn null_error_fields_do_not_mask_successful_conversion() {
        use crate::protocol::WireProtocol::{Messages, Responses};
        let body = json!({"type":"message","error":null,"content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":1,"output_tokens":1}});
        let converted =
            convert_response_body(&serde_json::to_vec(&body).unwrap(), Messages, Responses);
        let response: Value = serde_json::from_slice(&converted).unwrap();
        assert_eq!(response["output_text"], "ok");
        let stream = format_converted_sse(
            &serde_json::to_vec(&json!({"error":null,"output":[]})).unwrap(),
            Responses,
        );
        assert!(
            std::str::from_utf8(&stream)
                .unwrap()
                .contains("response.completed")
        );
    }

    #[test]
    fn messages_stream_preserves_parallel_tools_text_and_usage_in_responses() {
        use crate::protocol::WireProtocol::{Messages, Responses};
        let events = vec![
            json!({"type":"message_start","message":{"id":"msg_test","type":"message","role":"assistant","model":"claude-test","content":[],"usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":20,"cache_creation_input_tokens":5}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"checking "}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"cities"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_a","name":"functions__lookup","input":{}}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call_b","name":"functions__lookup","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":\""}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"city\":\"London\"}"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"Paris\"}"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}),
            json!({"type":"message_stop"}),
        ];
        let body = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
        let response: Value =
            serde_json::from_slice(&convert_response_body(body.as_bytes(), Messages, Responses))
                .unwrap();
        assert_eq!(response["output_text"], "checking cities");
        assert_eq!(response["output"][1]["call_id"], "call_a");
        assert_eq!(response["output"][1]["arguments"], "{\"city\":\"Paris\"}");
        assert_eq!(response["output"][2]["call_id"], "call_b");
        assert_eq!(response["output"][2]["arguments"], "{\"city\":\"London\"}");
        assert_eq!(response["usage"]["input_tokens"], 35);
        assert_eq!(response["usage"]["output_tokens"], 12);
        assert_eq!(response["usage"]["total_tokens"], 47);
        assert_eq!(
            response["usage"]["input_tokens_details"]["cached_tokens"],
            20
        );
    }

    #[test]
    fn interrupted_or_error_messages_stream_does_not_emit_fake_completion() {
        use crate::protocol::WireProtocol::{Messages, Responses};
        for body in [
            "data: {\"type\":\"message_start\",\"message\":{\"content\":[]}}\n\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"retry\"}}\n\n",
        ] {
            let converted = convert_response_body(body.as_bytes(), Messages, Responses);
            let stream = format_converted_sse(&converted, Responses);
            let stream = std::str::from_utf8(&stream).unwrap();
            assert!(stream.contains("event: error"));
            assert!(!stream.contains("response.completed"));
        }
    }

    #[test]
    fn converted_responses_sse_exposes_completed_text_and_tool_items() {
        let response = json!({"id":"resp_test","status":"completed","output":[
            {"id":"msg_test","type":"message","status":"completed","role":"assistant","content":[{"type":"output_text","text":"hello","annotations":[]}]},
            {"id":"fc_test","type":"function_call","status":"completed","call_id":"call_test","name":"read","namespace":"functions","arguments":"{}"}
        ]});
        let stream = format_converted_sse(
            &serde_json::to_vec(&response).unwrap(),
            crate::protocol::WireProtocol::Responses,
        );
        let text = std::str::from_utf8(&stream).unwrap();
        let events: Vec<Value> = text
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        assert_eq!(events[0]["response"]["status"], "in_progress");
        assert_eq!(events[0]["response"]["output"], json!([]));
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["sequence_number"], index);
        }
        let done: Vec<_> = events
            .iter()
            .filter(|event| event["type"] == "response.output_item.done")
            .collect();
        assert_eq!(done.len(), 2);
        assert_eq!(done[0]["item"]["content"][0]["text"], "hello");
        assert_eq!(done[1]["item"]["namespace"], "functions");
        assert!(events.iter().any(|event| event["type"]
            == "response.function_call_arguments.delta"
            && event["delta"] == "{}"));
        assert_eq!(events.last().unwrap()["response"], response);
    }

    #[test]
    fn cross_protocol_query_removes_beta_but_preserves_other_parameters() {
        let mut target =
            Url::parse("https://example.test/v1/messages?beta=true&trace=test").unwrap();
        remove_client_protocol_query(&mut target);
        assert_eq!(target.query(), Some("trace=test"));
        target.set_query(Some("beta=true"));
        remove_client_protocol_query(&mut target);
        assert_eq!(target.query(), None);
    }

    use super::*;
    use futures_util::StreamExt;

    #[test]
    fn incremental_chat_to_messages_emits_before_upstream_done() {
        let mut converter = IncrementalChatToMessages::default();
        let mut pending = VecDeque::new();
        let first = b"data: {\"id\":\"chat-1\",\"model\":\"gemini-3.8-flash\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n";
        converter
            .push(&first[..first.len() - 2], &mut pending)
            .unwrap();
        assert!(
            pending.is_empty(),
            "an incomplete SSE event must stay buffered"
        );
        converter
            .push(&first[first.len() - 2..], &mut pending)
            .unwrap();
        assert_eq!(pending.len(), 1);
        let message_start = String::from_utf8(pending.pop_front().unwrap().to_vec()).unwrap();
        assert!(message_start.contains("message_start"));

        converter.push(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
            &mut pending,
        ).unwrap();
        assert_eq!(
            pending.len(),
            2,
            "text must be emitted before the stream ends"
        );
        assert!(
            String::from_utf8(pending.pop_front().unwrap().to_vec())
                .unwrap()
                .contains("content_block_start")
        );
        assert!(
            String::from_utf8(pending.pop_front().unwrap().to_vec())
                .unwrap()
                .contains("hello")
        );

        converter.push(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            &mut pending,
        ).unwrap();
        assert_eq!(pending.len(), 3);
        assert!(
            String::from_utf8(pending.pop_front().unwrap().to_vec())
                .unwrap()
                .contains("content_block_stop")
        );
        assert!(
            String::from_utf8(pending.pop_front().unwrap().to_vec())
                .unwrap()
                .contains("message_delta")
        );
        assert!(
            String::from_utf8(pending.pop_front().unwrap().to_vec())
                .unwrap()
                .contains("message_stop")
        );
    }

    #[test]
    fn incremental_chat_to_messages_preserves_tool_use_events() {
        let mut converter = IncrementalChatToMessages::default();
        let mut pending = VecDeque::new();
        let first = format!(
            "data: {}\n\n",
            serde_json::to_string(&json!({
                "id":"chat-tool",
                "model":"gpt-tool",
                "choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"{\\\"path\\\":\\\"test.txt\\\"}"}}]},"finish_reason":null}]
            })).unwrap()
        );
        converter.push(first.as_bytes(), &mut pending).unwrap();
        let output = pending
            .drain(..)
            .map(|bytes| String::from_utf8(bytes.to_vec()).unwrap())
            .collect::<String>();
        assert!(output.contains("content_block_start"));
        assert!(output.contains("tool_use"));
        assert!(output.contains("read_file"));
        assert!(output.contains("test.txt"));

        converter.push(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
            &mut pending,
        ).unwrap();
        let output = pending
            .drain(..)
            .map(|bytes| String::from_utf8(bytes.to_vec()).unwrap())
            .collect::<String>();
        assert!(output.contains("content_block_stop"));
        assert!(output.contains("tool_use"));
        assert!(output.contains("message_stop"));
    }

    #[tokio::test]
    async fn cancelled_converted_stream_releases_request_and_records_cancellation() {
        let root = std::env::temp_dir().join(format!(
            "agent-switch-cancel-{:032x}",
            rand::random::<u128>()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("usage.sqlite");
        let recorder = crate::usage::Recorder::open(&path).unwrap();
        let mut usage = recorder.start("isolated-stream".to_owned(), crate::usage::Protocol::Chat);
        usage.set_status_code(200);
        usage.parser.set_content_type("text/event-stream");
        let active = Arc::new(AtomicUsize::new(0));
        let upstream = futures_util::stream::iter([Ok::<_, reqwest::Error>(Bytes::from_static(
            b"data: {\"id\":\"chat-1\",\"model\":\"gpt-test\",\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n"
        ))]).chain(futures_util::stream::pending());
        let mut stream = Box::pin(incremental_chat_to_messages_stream(
            upstream,
            Some(usage),
            ActiveRequest::new(active.clone()),
        ));
        assert!(stream.next().await.unwrap().is_ok());
        assert_eq!(active.load(Ordering::SeqCst), 1);
        drop(stream); // Client stopped reading: drop also releases the upstream.
        assert_eq!(active.load(Ordering::SeqCst), 0);
        recorder.flush();
        let connection = rusqlite::Connection::open(&path).unwrap();
        let outcome: String = connection
            .query_row("SELECT outcome FROM requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(outcome, "cancelled");
        drop(connection);
        drop(recorder);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn converted_messages_streams_before_upstream_completion() {
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = release.clone();
        let upstream = serve(move |request| {
            let gate = gate.clone();
            async move {
                assert_eq!(request.uri().path(), "/v1/chat/completions");
                assert_eq!(request.headers()["accept-encoding"], "identity");
                assert_eq!(request.headers().get_all("accept-encoding").iter().count(), 1);
                let first = Bytes::from_static(
                    b"data: {\"id\":\"chat-1\",\"model\":\"gpt-test\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
                );
                let second = Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
                );
                let finish = Bytes::from_static(
                    b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n",
                );
                let stream = futures_util::stream::iter([
                    Ok::<_, BoxError>(Frame::data(first)),
                    Ok::<_, BoxError>(Frame::data(second)),
                ])
                .chain(futures_util::stream::once(async move {
                    gate.notified().await;
                    Ok::<_, BoxError>(Frame::data(finish))
                }))
                .chain(futures_util::stream::pending()); // [DONE] must end the response even if TCP remains open.
                Response::builder()
                    .header(CONTENT_TYPE, "text/event-stream")
                    .header("content-encoding", "identity")
                    .body(StreamBody::new(stream).boxed_unsync())
                    .unwrap()
            }
        })
        .await;
        let gateway = test_gateway(&format!("{}/v1", upstream.url)).await;
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let update = client
            .post(format!("{}/admin/update", gateway.url))
            .bearer_auth("admin-token")
            .json(&json!({
                "gateway_url": format!("{}/v1", upstream.url),
                "api_key": "first-key",
                "conversion_enabled": true
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(update.status(), StatusCode::OK);

        let mut response = client
            .post(format!("{}/messages", gateway.url))
            .bearer_auth("client-token")
            .header("accept-encoding", "gzip, br")
            .body(r#"{"model":"gpt-test","stream":true,"messages":[{"role":"user","content":"hello"}]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key("content-encoding"));
        // HTTP chunks are arbitrary: one chunk can contain several SSE events.
        let prefix = tokio::time::timeout(Duration::from_secs(2), async {
            let mut prefix = String::new();
            while !prefix.contains("hello") {
                let chunk = response
                    .chunk()
                    .await
                    .unwrap()
                    .expect("premature stream end");
                prefix.push_str(std::str::from_utf8(&chunk).unwrap());
            }
            prefix
        })
        .await
        .expect("text must arrive before the upstream finish gate opens");
        assert!(prefix.contains("message_start"));
        release.notify_one();
        let rest = response.bytes().await.unwrap();
        assert!(
            rest.windows(b"content_block_stop".len())
                .any(|window| window == b"content_block_stop")
        );
        assert!(
            rest.windows(b"message_stop".len())
                .any(|window| window == b"message_stop")
        );
        let (recorder, path) = gateway.statistics.as_ref().unwrap();
        recorder.flush();
        let stats = crate::usage::load_summaries(path).unwrap();
        let summary = stats.values().next().unwrap();
        assert_eq!((summary.total, summary.success, summary.failed), (1, 1, 0));
        assert_eq!((summary.input, summary.output), (Some(5), Some(1)));
    }

    #[tokio::test]
    #[ignore = "requires an explicitly supplied Claude Code executable; isolated loopback-only smoke test"]
    async fn installed_claude_code_displays_converted_stream() {
        let executable = std::env::var_os("AGENT_SWITCH_CLAUDE_TEST_EXE")
            .expect("supply the Claude Code executable path explicitly");
        let upstream = serve(|request| async move {
            if request.uri().path().ends_with("/count_tokens") {
                return json_response(StatusCode::OK, json!({"input_tokens":10}));
            }
            assert_eq!(request.uri().path(), "/v1/chat/completions");
            let bytes = request.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            let model = body["model"].as_str().unwrap().to_owned();
            let first = Bytes::from(format!("data: {}\n\n", json!({
                "id":"chat-smoke","model":model,"choices":[{"index":0,"delta":{"role":"assistant","content":"STREAM_SMOKE_"},"finish_reason":null}]
            })));
            let stream = futures_util::stream::once(async move { Ok::<_, BoxError>(Frame::data(first)) })
                .chain(futures_util::stream::once(async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Ok::<_, BoxError>(Frame::data(Bytes::from_static(
                        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"OK\"},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n"
                    )))
                }));
            Response::builder().header(CONTENT_TYPE, "text/event-stream")
                .body(StreamBody::new(stream).boxed_unsync()).unwrap()
        }).await;
        let gateway = test_gateway(&format!("{}/v1", upstream.url)).await;
        let client = Client::builder().no_proxy().build().unwrap();
        client.post(format!("{}/admin/update", gateway.url)).bearer_auth("admin-token")
            .json(&json!({"gateway_url":format!("{}/v1", upstream.url),"api_key":"first-key","conversion_enabled":true}))
            .send().await.unwrap().error_for_status().unwrap();
        let root = std::env::current_dir()
            .unwrap()
            .join("target/qa")
            .join(format!(
                "claude-stream-smoke-{:032x}",
                rand::random::<u128>()
            ));
        fs::create_dir_all(&root).unwrap();
        for model in ["gpt-6.1-sol", "grok-4.7", "gemini-3.8-flash"] {
            let mut command = tokio::process::Command::new(&executable);
            command
                .args([
                    "--bare",
                    "--print",
                    "--no-session-persistence",
                    "--strict-mcp-config",
                    "--setting-sources",
                    "",
                    "--tools",
                    "",
                    "--system-prompt",
                    "Reply briefly.",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--include-partial-messages",
                    "--model",
                    model,
                    "Reply STREAM_SMOKE_OK.",
                ])
                .current_dir(&root)
                .env("CLAUDE_CONFIG_DIR", root.join("config"))
                .env("ANTHROPIC_BASE_URL", &gateway.url)
                .env("ANTHROPIC_API_KEY", "client-token")
                .env_remove("ANTHROPIC_AUTH_TOKEN")
                .env_remove("CLAUDE_CODE_OAUTH_TOKEN")
                .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
                .env("DISABLE_AUTOUPDATER", "1")
                .env("DISABLE_TELEMETRY", "1")
                .env("DISABLE_ERROR_REPORTING", "1")
                .kill_on_drop(true);
            #[cfg(windows)]
            command.creation_flags(0x08000000);
            let output = tokio::time::timeout(Duration::from_secs(30), command.output())
                .await
                .expect("isolated Claude Code did not finish")
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{model}: {stdout}\n{stderr}");
            let events = stdout
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect::<Vec<_>>();
            let result = events
                .iter()
                .find(|event| event["type"] == "result")
                .expect("Claude did not produce a result");
            assert_eq!(result["subtype"], "success", "{model}: {stdout}");
            assert_eq!(result["result"], "STREAM_SMOKE_OK", "{model}: {stdout}");
            assert!(
                events
                    .iter()
                    .any(|event| event.pointer("/event/type").and_then(Value::as_str)
                        == Some("content_block_delta")),
                "Claude did not receive incremental events"
            );
            eprintln!("{model}: Claude Code displayed STREAM_SMOKE_OK and exited successfully");
        }
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn configuration_summary_survives_reload_and_older_state_is_unknown() {
        let mut info: GatewayInfo = serde_json::from_value(json!({
            "pid": 1, "base_url": "http://127.0.0.1:1",
            "gateway_url": "https://api.example.test",
            "client_token": "client-token", "admin_token": "admin-token",
            "executable_path": "gateway.exe", "version": "1.4.0"
        }))
        .unwrap();
        assert!(info.configuration_fingerprint.is_empty());
        assert_eq!(info.usage_schema, 0);
        assert!(info.statistics_backend.is_none());
        assert!(!statistics_ready(&info));
        info.configuration_fingerprint =
            configuration_fingerprint(&info.gateway_url, "private-key").unwrap();
        let serialized = serde_json::to_string(&info).unwrap();
        assert!(!serialized.contains("private-key"));
        let restored: GatewayInfo = serde_json::from_str(&serialized).unwrap();
        assert_eq!(
            restored.configuration_fingerprint,
            configuration_fingerprint(" https://api.example.test/ ", " private-key ").unwrap()
        );
        assert_ne!(
            restored.configuration_fingerprint,
            configuration_fingerprint("https://api.example.test", "another-key").unwrap()
        );
    }

    struct TestServer {
        url: String,
        task: JoinHandle<()>,
        statistics: Option<(crate::usage::Recorder, PathBuf)>,
    }
    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn serve<F, Fut>(handler: F) -> TestServer
    where
        F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Response<GatewayBody>> + Send + 'static,
    {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let handler = handler.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let handler = handler.clone();
                        async move { Ok::<_, Infallible>(handler(request).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        TestServer {
            url,
            task,
            statistics: None,
        }
    }

    async fn test_gateway(upstream: &str) -> TestServer {
        let path = std::env::temp_dir().join(format!(
            "agent-switch-gateway-test-{:032x}.sqlite",
            rand::random::<u128>()
        ));
        let recorder = crate::usage::Recorder::open(&path).unwrap();
        let state = Arc::new(GatewayState {
            enabled: AtomicBool::new(true),
            target: ClientTarget::CodexDesktop,
            route: RwLock::new(Route {
                gateway_url: normalize_url(upstream).unwrap(),
                api_key: Zeroizing::new("first-key".into()),
                conversion_enabled: false,
            }),
            client_token: "client-token".into(),
            admin_token: "admin-token".into(),
            // Deliberately broken proxy: loopback upstreams must always bypass it.
            client: Client::builder()
                .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
                .build()
                .unwrap(),
            direct_client: Client::builder().no_proxy().build().unwrap(),
            usage: recorder.clone(),
            tool_signatures: Arc::default(),
            active: Arc::new(AtomicUsize::new(0)),
        });
        let mut server = serve(move |request| {
            let state = state.clone();
            async move { handle_request(request, state).await }
        })
        .await;
        server.statistics = Some((recorder, path));
        server
    }

    #[test]
    fn managed_host_routes_reopen_streaming_exit_and_cleanup() {
        // Global host and app-data isolation must run in a separate test process:
        // never overwrite real gateway state or alter other parallel tests.
        if std::env::var_os("AGENT_SWITCH_ISOLATED_HOST_TEST").is_none() {
            let root = std::env::temp_dir()
                .join(format!("agent-switch-host-{:032x}", rand::random::<u128>()));
            fs::create_dir_all(&root).unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "local_gateway::tests::managed_host_routes_reopen_streaming_exit_and_cleanup",
                    "--nocapture",
                ])
                .env("AGENT_SWITCH_ISOLATED_HOST_TEST", "1")
                .env("LOCALAPPDATA", &root)
                .output()
                .unwrap();
            let _ = fs::remove_dir_all(&root);
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let host = crate::background::Host::start().unwrap();
        let runtime = crate::background::test_runtime();
        let (infos, _upstream) = runtime.block_on(async {
            let upstream = serve(|request| async move {
                if request.uri().path().ends_with("/responses") {
                    let stream = futures_util::stream::once(async {
                        Ok::<_, BoxError>(Frame::data(Bytes::from_static(b"data: waiting\n\n")))
                    })
                    .chain(futures_util::stream::pending());
                    Response::builder()
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(StreamBody::new(stream).boxed_unsync())
                        .unwrap()
                } else {
                    json_response(
                        StatusCode::OK,
                        json!({"auth":request.headers()[AUTHORIZATION].to_str().unwrap()}),
                    )
                }
            })
            .await;
            let mut infos = Vec::new();
            for (index, target) in ClientTarget::ALL.into_iter().enumerate() {
                let info = ensure_and_update(target, &upstream.url, &format!("key-{index}"), false)
                    .await
                    .unwrap();
                assert_eq!(info.pid, std::process::id());
                confirm_current_worker(target).unwrap();
                infos.push(info);
            }
            assert_eq!(
                infos
                    .iter()
                    .map(|info| &info.base_url)
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                4
            );
            let updated =
                ensure_and_update(ClientTarget::CodexDesktop, &upstream.url, "new-key", false)
                    .await
                    .unwrap();
            assert_eq!(updated.base_url, infos[1].base_url);
            assert_eq!(updated.client_token, infos[1].client_token);
            // Protocol switches keep all four clients' endpoint, credentials,
            // model attachment and provider identity stable. Changed URLs/keys
            // are rejected here and must use the restart/configuration path.
            for (index, target) in ClientTarget::ALL.into_iter().enumerate() {
                let key = if index == 1 {
                    "new-key".to_owned()
                } else {
                    format!("key-{index}")
                };
                let before = managed_routes()
                    .lock()
                    .unwrap()
                    .get(&target)
                    .unwrap()
                    .info
                    .clone();
                for enabled in [true, false] {
                    crate::gui_worker::update_gateway(target, &upstream.url, key.clone(), enabled)
                        .await
                        .unwrap();
                    let routes = managed_routes().lock().unwrap();
                    let route = routes.get(&target).unwrap();
                    assert!(route.attached);
                    assert_eq!(route.info.base_url, before.base_url);
                    assert_eq!(route.info.client_token, before.client_token);
                    assert_eq!(
                        route.info.configuration_fingerprint,
                        before.configuration_fingerprint
                    );
                    assert_eq!(
                        route.state.route.read().unwrap().conversion_enabled,
                        enabled
                    );
                }
                assert!(
                    crate::gui_worker::update_gateway(
                        target,
                        &upstream.url,
                        "different-key".into(),
                        true
                    )
                    .await
                    .is_err()
                );
                assert!(
                    crate::gui_worker::update_gateway(
                        target,
                        "https://different.invalid/v1",
                        key,
                        true
                    )
                    .await
                    .is_err()
                );
                let routes = managed_routes().lock().unwrap();
                let route = routes.get(&target).unwrap();
                assert!(route.attached);
                assert_eq!(route.info.gateway_url, before.gateway_url);
                assert_eq!(
                    route.info.configuration_fingerprint,
                    before.configuration_fingerprint
                );
                assert!(!route.state.route.read().unwrap().conversion_enabled);
            }
            (infos, upstream)
        });
        let client_runtime = tokio::runtime::Runtime::new().unwrap();
        let mut unfinished = client_runtime.block_on(async {
            for (index, info) in infos.iter().enumerate() {
                // A newly opened HTTP client still uses the original endpoint/token.
                let client = Client::builder().no_proxy().build().unwrap();
                let result: Value = client
                    .get(local_url(info, "/v1/models"))
                    .bearer_auth(&info.client_token)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                assert_eq!(
                    result["auth"],
                    if index == 1 {
                        "Bearer new-key".to_owned()
                    } else {
                        format!("Bearer key-{index}")
                    }
                );
                assert_eq!(
                    client
                        .get(local_url(info, "/v1/models"))
                        .bearer_auth(&infos[(index + 1) % 4].client_token)
                        .send()
                        .await
                        .unwrap()
                        .status(),
                    StatusCode::UNAUTHORIZED
                );
            }
            let client = Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(8))
                .build()
                .unwrap();
            let mut response = client
                .post(local_url(&infos[1], "/responses"))
                .bearer_auth(&infos[1].client_token)
                .body("{}")
                .send()
                .await
                .unwrap();
            assert!(response.chunk().await.unwrap().is_some());
            // The client process may already have closed, but a stream still needs confirmation.
            response
        });
        let (proxy_url, proxy_token) = runtime.block_on(async {
            let proxy = crate::claude_desktop_proxy::ClaudeDesktopProxy::start(
                &infos[3].base_url,
                infos[3].client_token.clone(),
                crate::claude_desktop_proxy::ModelSlots {
                    haiku: "haiku".into(),
                    sonnet: "sonnet".into(),
                    opus: "opus".into(),
                    fable: "fable".into(),
                },
                crate::claude_desktop_proxy::DesktopModelMenu::four_slots(),
            )
            .await
            .unwrap();
            let proxy_url = proxy.base_url();
            let proxy_token = proxy.local_token().to_owned();
            crate::background::retain_proxy(ClientTarget::ClaudeDesktop, proxy);
            (proxy_url, proxy_token)
        });
        client_runtime.block_on(async {
            let client = Client::builder().no_proxy().build().unwrap();
            assert!(
                client
                    .get(format!("{proxy_url}/v1/models"))
                    .bearer_auth(proxy_token)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .is_success()
            );
        });
        // Restoring one client to account mode revokes its listener and old
        // credential without disrupting the other three clients. Reconnecting
        // creates a fresh listener/token instead of reviving the revoked pair.
        runtime.block_on(async {
            let revoked = infos[0].clone();
            let state = managed_routes()
                .lock()
                .unwrap()
                .get(&ClientTarget::CodexCli)
                .unwrap()
                .state
                .clone();
            disconnect_for_account(ClientTarget::CodexCli).unwrap();
            assert!(!is_attached(ClientTarget::CodexCli));
            assert!(!state_path(ClientTarget::CodexCli).unwrap().exists());
            assert!(!state.enabled.load(Ordering::SeqCst));
            for target in [
                ClientTarget::CodexDesktop,
                ClientTarget::ClaudeCode,
                ClientTarget::ClaudeDesktop,
            ] {
                assert!(is_attached(target));
            }
            let fresh = ensure_and_update(ClientTarget::CodexCli, &_upstream.url, "key-0", false)
                .await
                .unwrap();
            assert_ne!(fresh.client_token, revoked.client_token);
            let client = Client::builder().no_proxy().build().unwrap();
            assert_eq!(
                client
                    .get(local_url(&fresh, "/v1/models"))
                    .bearer_auth(&revoked.client_token)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
            assert!(
                client
                    .get(local_url(&fresh, "/v1/models"))
                    .bearer_auth(&fresh.client_token)
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .is_success()
            );
        });
        drop(host);
        for target in ClientTarget::ALL {
            assert!(!state_path(target).unwrap().exists());
            assert!(!is_attached(target));
        }
        client_runtime.block_on(async {
            let client = Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap();
            for info in &infos {
                assert!(client.get(local_url(info, "/health")).send().await.is_err());
            }
            assert!(
                client
                    .get(format!("{proxy_url}/v1/models"))
                    .send()
                    .await
                    .is_err()
            );
            assert!(unfinished.chunk().await.is_err());
        });
        let statistics =
            crate::usage::load_summaries(&crate::usage::database_path().unwrap()).unwrap();
        let summary = statistics.values().next().unwrap();
        assert_eq!(summary.total, 1);
        assert_eq!(summary.success, 0);
        assert_eq!(summary.failed, 1);
    }

    #[tokio::test]
    async fn catalog_fetch_avoids_claude_aliases_directly_and_through_gateway() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let upstream = serve(move |request| {
            let calls = observed.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(request.uri().path(), "/v1/models");
                assert_eq!(request.headers()[AUTHORIZATION], "Bearer first-key");
                // Reproduce runapi selecting Gemini format from a Google key.
                if request.headers().contains_key("x-goog-api-key") {
                    return json_response(StatusCode::OK, json!({
                        "models": [{"name": "claude-sonnet-4-6", "displayName": "claude-sonnet-4-6"}],
                        "nextPageToken": ""
                    }));
                }
                // Reproduce LiteAPI selecting aliases from the version header.
                let id = if request.headers().contains_key("anthropic-version") {
                    "claude-liteapi-claude-sonnet-4-6"
                } else {
                    "claude-sonnet-4-6"
                };
                json_response(StatusCode::OK, json!({"data": [{"id": id}]}))
            }
        })
        .await;
        let gateway = test_gateway(&format!("{}/v1", upstream.url)).await;
        for (url, key) in [
            (upstream.url.as_str(), "first-key"),
            (gateway.url.as_str(), "client-token"),
        ] {
            let catalog = crate::catalog::fetch_catalog(url, key, "0.145.0")
                .await
                .unwrap();
            assert_eq!(
                crate::catalog::visible_slugs(&catalog),
                ["claude-sonnet-4-6"]
            );
            assert_eq!(catalog["models"][0]["display_name"], "claude-sonnet-4-6");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn catalog_fetch_falls_back_to_anthropic_auth_directly_and_through_gateway() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let upstream = serve(move |request| {
            let calls = observed.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if !request.headers().contains_key("anthropic-version") {
                    return json_error(StatusCode::UNAUTHORIZED, "api_key_required");
                }
                assert_eq!(request.headers()["x-api-key"], "first-key");
                assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
                assert!(!request.headers().contains_key("x-goog-api-key"));
                assert!(!request.headers().contains_key(AUTHORIZATION));
                json_response(
                    StatusCode::OK,
                    json!({"data": [{"id": "claude-sonnet-4-6"}]}),
                )
            }
        })
        .await;
        let gateway = test_gateway(&format!("{}/v1", upstream.url)).await;
        for (url, key) in [
            (upstream.url.as_str(), "first-key"),
            (gateway.url.as_str(), "client-token"),
        ] {
            let catalog = crate::catalog::fetch_catalog(url, key, "0.145.0")
                .await
                .unwrap();
            assert_eq!(
                crate::catalog::visible_slugs(&catalog),
                ["claude-sonnet-4-6"]
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn catalog_fetch_handles_anthropic_missing_version_400_through_gateway() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let upstream = serve(move |request| {
            let calls = observed.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if !request.headers().contains_key("anthropic-version") {
                    return json_response(
                        StatusCode::BAD_REQUEST,
                        json!({
                            "type": "error",
                            "error": {
                                "type": "invalid_request_error",
                                "message": "anthropic-version: header is required"
                            }
                        }),
                    );
                }
                assert_eq!(request.headers()["x-api-key"], "first-key");
                assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
                assert!(!request.headers().contains_key("x-goog-api-key"));
                assert!(!request.headers().contains_key(AUTHORIZATION));
                json_response(
                    StatusCode::OK,
                    json!({"data": [{"id": "claude-sonnet-4-6"}]}),
                )
            }
        })
        .await;
        let gateway = test_gateway(&format!("{}/v1", upstream.url)).await;
        for (url, key) in [
            (upstream.url.as_str(), "first-key"),
            (gateway.url.as_str(), "client-token"),
        ] {
            let catalog = crate::catalog::fetch_catalog(url, key, "0.145.0")
                .await
                .unwrap();
            assert_eq!(
                crate::catalog::visible_slugs(&catalog),
                ["claude-sonnet-4-6"]
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn catalog_fetch_keeps_unrelated_400_errors_without_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let upstream = serve(move |_| {
            let calls = observed.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                json_response(
                    StatusCode::BAD_REQUEST,
                    json!({
                        "error": {"type": "invalid_request_error", "message": "invalid limit"}
                    }),
                )
            }
        })
        .await;
        let gateway = test_gateway(&upstream.url).await;
        let error = crate::catalog::fetch_catalog(&gateway.url, "client-token", "0.145.0")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("HTTP 400"));
        assert!(error.contains("invalid limit"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn catalog_fetch_does_not_retry_http_server_errors_with_anthropic_headers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let upstream = serve(move |_| {
            let calls = observed.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                json_error(StatusCode::BAD_GATEWAY, "upstream_request_failed")
            }
        })
        .await;
        let result = crate::catalog::fetch_catalog(&upstream.url, "first-key", "0.145.0").await;
        assert!(result.unwrap_err().to_string().contains("HTTP 502"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn forwards_auth_and_body_switches_route_and_preserves_in_flight_stream() {
        let release = Arc::new(tokio::sync::Notify::new());
        let gate = release.clone();
        let first = serve(move |request| {
            let gate = gate.clone();
            async move {
                assert_eq!(request.headers()[AUTHORIZATION], "Bearer first-key");
                if request.uri().path() == "/v1/responses" {
                    assert_eq!(request.headers()["x-api-key"], "first-key");
                    assert_eq!(request.method(), Method::POST);
                    let body = request.into_body().collect().await.unwrap().to_bytes();
                    assert_eq!(body, r#"{"model":"test","stream":true}"#);
                    let stream = futures_util::stream::once(async {
                        Ok::<_, BoxError>(Frame::data(Bytes::from_static(b"data: first\n\n")))
                    })
                    .chain(futures_util::stream::once(async move {
                        gate.notified().await;
                        Ok::<_, BoxError>(Frame::data(Bytes::from_static(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":20}}}\n\n")))
                    }));
                    Response::builder()
                        .header(CONTENT_TYPE, "text/event-stream")
                        .body(StreamBody::new(stream).boxed_unsync())
                        .unwrap()
                } else {
                    json_response(StatusCode::OK, json!({"uri": request.uri().to_string()}))
                }
            }
        })
        .await;
        let second = serve(|request| async move {
            if request.method() == Method::POST {
                assert_eq!(request.headers()[AUTHORIZATION], "Bearer second-key");
                assert_eq!(request.headers()["x-api-key"], "second-key");
            } else {
                assert_eq!(request.headers()["x-api-key"], "second-key");
                assert!(!request.headers().contains_key(AUTHORIZATION));
                assert!(!request.headers().contains_key("x-goog-api-key"));
            }
            json_response(
                StatusCode::OK,
                json!({"uri": request.uri().to_string(), "second": true, "usage":{"input_tokens":2000,"output_tokens":40}}),
            )
        })
        .await;
        let gateway = test_gateway(&format!("{}/v1", first.url)).await;
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        assert_eq!(
            client
                .get(format!("{}/v1/models", gateway.url))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let models: Value = client
            .get(format!("{}/v1/models?client_version=123", gateway.url))
            .bearer_auth("client-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(models["uri"], "/v1/models?client_version=123");
        let mut streaming = client
            .post(format!("{}/responses", gateway.url))
            .bearer_auth("client-token")
            .body(r#"{"model":"test","stream":true}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(streaming.chunk().await.unwrap().unwrap(), "data: first\n\n");
        let update_request =
            json!({"gateway_url": format!("{}/api/v1", second.url), "api_key": "second-key"});
        assert_eq!(
            client
                .post(format!("{}/admin/update", gateway.url))
                .bearer_auth("client-token")
                .json(&update_request)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            client
                .post(format!("{}/admin/update", gateway.url))
                .bearer_auth("admin-token")
                .json(&update_request)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let models: Value = client
            .get(format!("{}/v1/models", gateway.url))
            .header("x-api-key", "client-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(models["uri"], "/api/v1/models");
        assert_eq!(models["second"], true);
        release.notify_one();
        assert_eq!(
            streaming.chunk().await.unwrap().unwrap(),
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":20}}}\n\n"
        );
        assert!(streaming.chunk().await.unwrap().is_none());
        let second_response = client
            .post(format!("{}/responses", gateway.url))
            .bearer_auth("client-token")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert!(second_response.status().is_success());
        second_response.bytes().await.unwrap();
        let (recorder, path) = gateway.statistics.as_ref().unwrap();
        recorder.flush();
        let statistics = crate::usage::load_summaries(path).unwrap();
        let first_id = crate::usage::configuration_id(&format!("{}/v1", first.url), "first-key");
        let second_id =
            crate::usage::configuration_id(&format!("{}/api/v1", second.url), "second-key");
        assert_eq!(statistics.len(), 2);
        assert_eq!(
            (
                statistics[&first_id].total,
                statistics[&first_id].success,
                statistics[&first_id].input
            ),
            (1, 1, Some(100))
        );
        assert_eq!(
            (
                statistics[&second_id].total,
                statistics[&second_id].success,
                statistics[&second_id].input
            ),
            (1, 1, Some(2000))
        );
    }

    #[tokio::test]
    async fn connection_error_is_actionable_and_does_not_leak_keys_or_query() {
        let gateway = test_gateway("http://127.0.0.1:0").await;
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let response = client
            .get(format!("{}/v1/models?secret=sensitive-query", gateway.url))
            .bearer_auth("client-token")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let text = response.text().await.unwrap();
        assert!(text.contains("connect/DNS/TLS/proxy"), "{text}");
        for secret in ["first-key", "client-token", "sensitive-query"] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    #[tokio::test]
    async fn upstream_http_errors_are_passed_through_without_retry() {
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let calls = count.clone();
        let upstream = serve(move |_| {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                json_response(
                    StatusCode::UNAUTHORIZED,
                    json!({"error": "invalid upstream key"}),
                )
            }
        })
        .await;
        let gateway = test_gateway(&upstream.url).await;
        let response = Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("{}/v1/models", gateway.url))
            .bearer_auth("client-token")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            response
                .text()
                .await
                .unwrap()
                .contains("invalid upstream key")
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn local_gateway_keeps_v1_path_once() {
        let base = Url::parse("https://example.test/v1").unwrap();
        let uri: Uri = "/v1/messages?stream=true".parse().unwrap();
        assert_eq!(
            upstream_url(&base, &uri).unwrap().as_str(),
            "https://example.test/v1/messages?stream=true"
        );
    }

    #[test]
    fn local_gateway_adds_v1_for_codex_and_claude_paths() {
        let base = Url::parse("https://example.test").unwrap();
        for path in ["/responses", "/messages", "/models"] {
            let uri: Uri = path.parse().unwrap();
            assert_eq!(
                upstream_url(&base, &uri).unwrap().as_str(),
                format!("https://example.test/v1{path}")
            );
        }
    }

    #[test]
    fn local_gateway_rejects_plaintext_remote_url() {
        assert!(normalize_url("http://example.test").is_err());
    }
}
