use crate::gui_worker::{ClientTarget, ModelListMode};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;

const RUNTIME_SCHEMA: u32 = 1;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const HEARTBEAT_FRESHNESS: Duration = Duration::from_secs(8);
const RUNTIME_LOG_PATH_ENV: &str = "ZNNZ_RUNTIME_LOG_PATH";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRuntimeState {
    pub schema: u32,
    pub client: ClientTarget,
    pub worker_pid: u32,
    pub model_list_mode: ModelListMode,
    pub started_at_ms: u64,
    pub heartbeat_ms: u64,
    pub log_path: PathBuf,
    pub executable_path: PathBuf,
    pub version: String,
}

impl ClientRuntimeState {
    fn new(target: ClientTarget, model_list_mode: ModelListMode) -> Result<Self> {
        let now = current_time_ms();
        Ok(Self {
            schema: RUNTIME_SCHEMA,
            client: target,
            worker_pid: std::process::id(),
            model_list_mode,
            started_at_ms: now,
            heartbeat_ms: now,
            log_path: crate::background::log_path()
                .or_else(|| std::env::var_os(RUNTIME_LOG_PATH_ENV).map(PathBuf::from))
                .unwrap_or_default(),
            executable_path: std::env::current_exe()
                .context("无法确定后台工作进程的可执行文件路径")?,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        })
    }

    fn heartbeat_is_fresh_at(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.heartbeat_ms) <= HEARTBEAT_FRESHNESS.as_millis() as u64
    }
}

struct RuntimeFileGuard {
    path: PathBuf,
    worker_pid: u32,
}

impl Drop for RuntimeFileGuard {
    fn drop(&mut self) {
        remove_state_if_owned(&self.path, self.worker_pid);
    }
}

pub async fn run_registered<F>(
    target: ClientTarget,
    model_list_mode: ModelListMode,
    future: F,
) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    let mutex_name = format!(
        r"Local\Agent-Switch-worker-{}{}",
        if crate::background::is_managed() {
            "v3-"
        } else {
            ""
        },
        target.id()
    );
    let _worker_mutex = crate::platform::try_acquire_named_mutex(&mutex_name)?
        .ok_or_else(|| anyhow::anyhow!("{} 已经由 Agent-Switch 后台进程维护", target.title()))?;

    let path = state_path(target)?;
    let mut state = ClientRuntimeState::new(target, model_list_mode)?;
    write_state(&path, &state)?;
    let _file_guard = RuntimeFileGuard {
        path: path.clone(),
        worker_pid: state.worker_pid,
    };

    tokio::pin!(future);
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The initial state was already written above; do not immediately rewrite it on the
    // interval's first instant tick.
    heartbeat.tick().await;

    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = heartbeat.tick() => {
                state.heartbeat_ms = current_time_ms();
                if let Err(error) = write_state(&path, &state) {
                    if crate::i18n::language() == crate::i18n::Language::ZhCn {
                        warn!("无法更新 {} 运行状态心跳: {error:#}", target.title());
                    } else {
                        warn!(
                            "Unable to update {} runtime heartbeat: {}",
                            target.title(),
                            crate::i18n::runtime_error(&error)
                        );
                    }
                }
            }
        }
    }
}

pub fn load_active_states() -> Result<Vec<ClientRuntimeState>> {
    let root = runtime_root()?;
    let mut states = Vec::new();
    for target in ClientTarget::ALL {
        if let Some(state) = load_active_state_from(&state_path_in(&root, target), target)? {
            states.push(state);
        }
    }
    Ok(states)
}

fn load_active_state_from(
    path: &Path,
    expected_target: ClientTarget,
) -> Result<Option<ClientRuntimeState>> {
    if !path.is_file() {
        return Ok(None);
    }

    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("无法读取运行状态 {}", path.display()));
        }
    };
    let state: ClientRuntimeState = match serde_json::from_slice(&raw) {
        Ok(state) => state,
        Err(_) => {
            let _ = std::fs::remove_file(path);
            return Ok(None);
        }
    };

    if state.schema != RUNTIME_SCHEMA || state.client != expected_target {
        let _ = std::fs::remove_file(path);
        return Ok(None);
    }

    let pid_running =
        crate::platform::running_any_process_ids(&[state.worker_pid])?.contains(&state.worker_pid);
    if !pid_running
        || !crate::platform::process_matches_image(state.worker_pid, &state.executable_path)
    {
        remove_state_if_owned(path, state.worker_pid);
        return Ok(None);
    }

    // A stale heartbeat is ignored rather than deleted. This lets a worker recover naturally
    // after Windows sleep/resume; a later GUI scan will attach as soon as the heartbeat moves.
    if !state.heartbeat_is_fresh_at(current_time_ms()) {
        return Ok(None);
    }

    Ok(Some(state))
}

fn write_state(path: &Path, state: &ClientRuntimeState) -> Result<()> {
    let mut raw = serde_json::to_vec_pretty(state).context("无法序列化客户端运行状态")?;
    raw.push(b'\n');
    crate::backup::atomic_write(path, &raw)
        .with_context(|| format!("无法保存客户端运行状态 {}", path.display()))
}

fn remove_state_if_owned(path: &Path, worker_pid: u32) {
    let owned = std::fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<ClientRuntimeState>(&raw).ok())
        .is_some_and(|state| state.worker_pid == worker_pid);
    if owned {
        let _ = std::fs::remove_file(path);
    }
}

fn runtime_root() -> Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("无法确定 LOCALAPPDATA")?;
    Ok(local
        .join("Agent-Switch")
        .join(if crate::background::is_managed() {
            "runtime-v3"
        } else {
            "runtime"
        }))
}

fn state_path(target: ClientTarget) -> Result<PathBuf> {
    Ok(state_path_in(&runtime_root()?, target))
}

fn state_path_in(root: &Path, target: ClientTarget) -> PathBuf {
    root.join(format!("{}.json", target.id()))
}

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state(target: ClientTarget) -> ClientRuntimeState {
        ClientRuntimeState {
            schema: RUNTIME_SCHEMA,
            client: target,
            worker_pid: 1234,
            model_list_mode: ModelListMode::Gateway,
            started_at_ms: 10_000,
            heartbeat_ms: 12_000,
            log_path: PathBuf::from(r"C:\Temp\worker.log"),
            executable_path: PathBuf::from(r"C:\Tools\znnz-client.exe"),
            version: "0.9.19".to_owned(),
        }
    }

    #[test]
    fn runtime_state_json_round_trips() {
        let state = sample_state(ClientTarget::CodexDesktop);
        let encoded = serde_json::to_vec(&state).unwrap();
        let decoded: ClientRuntimeState = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, state);
    }

    #[test]
    fn runtime_state_never_serializes_api_key() {
        let encoded = serde_json::to_string(&sample_state(ClientTarget::ClaudeDesktop)).unwrap();
        assert!(!encoded.to_ascii_lowercase().contains("api_key"));
        assert!(!encoded.contains("sk-"));
    }

    #[test]
    fn every_client_has_a_unique_state_file() {
        let root = Path::new(r"C:\runtime-test");
        let paths = ClientTarget::ALL
            .into_iter()
            .map(|target| state_path_in(root, target))
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(paths.len(), ClientTarget::ALL.len());
    }

    #[test]
    fn heartbeat_freshness_has_a_bounded_window() {
        let state = sample_state(ClientTarget::CodexCli);
        assert!(state.heartbeat_is_fresh_at(12_000));
        assert!(state.heartbeat_is_fresh_at(19_999));
        assert!(!state.heartbeat_is_fresh_at(21_001));
    }
}
