//! The GUI, tray, gateway listeners and client adapters share one process.
//! Closing a window never owns/cancels a service; only explicit exit does.
use crate::gui_worker::{ClientTarget, ModelListMode};
use anyhow::{Context, Result};
use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

struct Shared {
    runtime: tokio::runtime::Handle,
    shutdown: tokio::sync::watch::Sender<bool>,
    threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    proxies: Mutex<
        std::collections::HashMap<ClientTarget, crate::claude_desktop_proxy::ClaudeDesktopProxy>,
    >,
    clients: Mutex<std::collections::HashMap<ClientTarget, Arc<ClientControl>>>,
    terminals: Mutex<
        std::collections::HashMap<ClientTarget, Arc<crate::platform::TerminalRestartSession>>,
    >,
}

struct ClientControl {
    cancel: tokio::sync::watch::Sender<bool>,
    finished: tokio::sync::watch::Receiver<bool>,
}

impl ClientControl {
    async fn cancel_and_wait(&self) -> Result<()> {
        let _ = self.cancel.send(true);
        let mut finished = self.finished.clone();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !*finished.borrow() {
                finished
                    .changed()
                    .await
                    .context("旧客户端维护任务异常结束")?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("等待旧客户端维护任务退出超时")??;
        Ok(())
    }
}

static SHARED: OnceLock<Shared> = OnceLock::new();
thread_local! { static LOG: RefCell<Option<PathBuf>> = const { RefCell::new(None) }; }

pub struct Host(Option<tokio::runtime::Runtime>);

impl Host {
    pub fn start() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let (shutdown, _) = tokio::sync::watch::channel(false);
        SHARED
            .set(Shared {
                runtime: runtime.handle().clone(),
                shutdown,
                threads: Mutex::new(Vec::new()),
                proxies: Mutex::new(std::collections::HashMap::new()),
                clients: Mutex::new(std::collections::HashMap::new()),
                terminals: Mutex::new(std::collections::HashMap::new()),
            })
            .map_err(|_| anyhow::anyhow!("后台网关已启动"))?;
        Ok(Self(Some(runtime)))
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(shared) = SHARED.get() {
            let _ = shared.shutdown.send(true);
            if let Ok(mut threads) = shared.threads.lock() {
                // Registered jobs cancel cooperatively; AI client processes are not killed.
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                while threads.iter().any(|thread| !thread.is_finished())
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                for thread in threads.drain(..) {
                    if thread.is_finished() {
                        let _ = thread.join();
                    }
                }
            }
            if let Ok(mut proxies) = shared.proxies.lock() {
                proxies.clear();
            }
        }
        if let Some(runtime) = self.0.take() {
            // Cancels listeners and streaming bodies before flushing their usage events.
            runtime.shutdown_timeout(Duration::from_secs(3));
        }
        crate::local_gateway::cleanup_managed();
    }
}

pub fn is_managed() -> bool {
    SHARED.get().is_some()
}

pub fn log_path() -> Option<PathBuf> {
    LOG.with(|log| log.borrow().clone())
}

pub fn print_line(args: std::fmt::Arguments<'_>) {
    if let Some(path) = log_path() {
        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(path) {
            let _ = writeln!(file, "{args}");
        }
    } else {
        std::println!("{args}");
    }
}

pub fn retain_proxy(target: ClientTarget, proxy: crate::claude_desktop_proxy::ClaudeDesktopProxy) {
    if let Some(shared) = SHARED.get() {
        shared
            .proxies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(target, proxy);
    }
}

pub fn remember_terminal(target: ClientTarget, pid: u32) -> Result<()> {
    if let Some(shared) = SHARED.get() {
        let mut last_error = None;
        let session = (0..12)
            .find_map(
                |_| match crate::platform::TerminalRestartSession::record(pid, target) {
                    Ok(session) => Some(session),
                    Err(error) => {
                        last_error = Some(error);
                        std::thread::sleep(Duration::from_millis(50));
                        None
                    }
                },
            )
            .ok_or_else(|| last_error.unwrap_or_else(|| anyhow::anyhow!("终端会话尚未准备好")))?;
        shared
            .terminals
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(target, Arc::new(session));
    }
    Ok(())
}

async fn prepare_client_reconnect(
    target: ClientTarget,
    gateway_url: &str,
    key: &str,
    mode: ModelListMode,
) -> Result<()> {
    // Validate the replacement catalog before closing any client or changing its route.
    let catalog = crate::catalog::fetch_catalog(gateway_url, key, "0.145.0").await?;
    if mode == ModelListMode::Gateway {
        let hidden = crate::gui_settings::hidden_models_for_configuration(gateway_url, key)?;
        crate::catalog::validate_model_menu_catalog(&crate::catalog::model_menu_catalog(
            &catalog, &hidden,
        ))?;
    }
    release_client(target).await
}

async fn release_client(target: ClientTarget) -> Result<()> {
    let shared = SHARED.get().context("后台网关尚未启动")?;
    async {
        let previous = shared
            .clients
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&target)
            .cloned();
        if crate::platform::running_client_processes()?
            .iter()
            .any(|(client, _)| *client == target)
        {
            match target {
                ClientTarget::CodexDesktop | ClientTarget::ClaudeDesktop => {
                    crate::desktop::close_for_gateway_attach(target).await?;
                }
                ClientTarget::CodexCli | ClientTarget::ClaudeCode => {
                    let terminal = shared
                        .terminals
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .get(&target)
                        .cloned()
                        .context(crate::i18n::tr(
                            "当前终端不是 Agent-Switch 管理的会话",
                            "The terminal is not an Agent-Switch managed session",
                        ))?;
                    terminal.verify_client_sessions()?;
                    terminal.close()?;
                    if crate::platform::running_client_processes()?
                        .iter()
                        .any(|(client, _)| *client == target)
                    {
                        anyhow::bail!(
                            "{}",
                            crate::i18n::tr(
                                "仍有其他终端会话运行",
                                "Other terminal sessions are still running"
                            )
                        );
                    }
                }
            }
        }
        if let Some(previous) = previous {
            previous.cancel_and_wait().await?;
        }
        shared
            .terminals
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&target);
        shared
            .proxies
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&target);
        if target == ClientTarget::ClaudeDesktop {
            crate::claude_desktop_registry::restore_pending()?;
        }
        crate::local_gateway::mark_client_disconnected(target);
        Ok::<_, anyhow::Error>(())
    }
    .await
    .with_context(|| {
        format!(
            "{} • {}",
            target.title(),
            crate::i18n::tr("自动重启 • 失败!", "Auto-restart • failed!")
        )
    })
}

pub fn start_account_mode_job(target: ClientTarget, log_path: PathBuf) -> Result<Job> {
    let shared = SHARED.get().context("后台网关尚未启动")?;
    let runtime = shared.runtime.clone();
    let mut shutdown = shared.shutdown.subscribe();
    let (sender, receiver) = mpsc::channel();
    let thread = std::thread::Builder::new().name(format!("account-{}", target.id())).spawn(move || {
        let _trace_guard = std::fs::OpenOptions::new().append(true).open(&log_path).ok().map(|file| {
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_target(false).with_ansi(false)
                .without_time().with_writer(Mutex::new(file)).finish())
        });
        LOG.with(|log| *log.borrow_mut() = Some(log_path));
        let result = runtime.block_on(async {
            tokio::select! {
                _ = shutdown.changed() => Ok(()),
                result = async {
                    crate::account_mode::validate_restore(target)?;
                    let was_running = crate::platform::running_client_processes()?.iter().any(|(client, _)| *client == target);
                    // Closing/cancelling monitors must finish before restoring files;
                    // otherwise an old injection job could write the gateway back.
                    release_client(target).await?;
                    crate::account_mode::restore(target)?;
                    crate::local_gateway::disconnect_for_account(target)?;
                    println!("{} • {}", target.title(), crate::i18n::tr("已恢复账号模式", "Account mode restored"));
                    if was_running {
                        crate::gui_worker::restart_account_client(target).await.with_context(|| format!("{} • {}", target.title(),
                            crate::i18n::tr("自动重启 • 失败!", "Auto-restart • failed!")))?;
                    }
                    Ok::<_, anyhow::Error>(())
                } => result,
            }
        }).map_err(|error| crate::i18n::runtime_error(&error));
        if let Err(error) = &result {
            print_line(format_args!("\n{}:\n  {error}", crate::i18n::tr("错误", "Error")));
        }
        let _ = sender.send(result);
    })?;
    let mut threads = shared.threads.lock().unwrap_or_else(|e| e.into_inner());
    threads.retain(|thread| !thread.is_finished());
    threads.push(thread);
    Ok(Job(receiver))
}

#[cfg(test)]
pub fn test_runtime() -> tokio::runtime::Handle {
    SHARED.get().unwrap().runtime.clone()
}

pub struct Job(mpsc::Receiver<std::result::Result<(), String>>);
impl Job {
    pub fn poll(&self) -> Option<std::result::Result<(), String>> {
        match self.0.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(crate::i18n::tr(
                "后台任务意外结束",
                "The background task ended unexpectedly.",
            )
            .into())),
        }
    }
}

pub struct ProviderJob(
    mpsc::Receiver<std::result::Result<Vec<crate::gui_providers::Provider>, String>>,
);

impl ProviderJob {
    pub fn poll(&self) -> Option<std::result::Result<Vec<crate::gui_providers::Provider>, String>> {
        match self.0.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("提供商清单后台任务意外结束".into())),
        }
    }
}

pub fn start_provider_job() -> Result<ProviderJob> {
    let shared = SHARED.get().context("后台网关尚未启动")?;
    let runtime = shared.runtime.clone();
    let (sender, receiver) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("provider-presets".to_owned())
        .spawn(move || {
            let result = runtime
                .block_on(crate::gui_providers::fetch())
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
        })?;
    shared
        .threads
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(thread);
    Ok(ProviderJob(receiver))
}

pub fn start_client_job(
    target: ClientTarget,
    mode: ModelListMode,
    update_only: bool,
    gateway_url: String,
    key: String,
    conversion_enabled: bool,
    log_path: PathBuf,
) -> Result<Job> {
    let shared = SHARED.get().context("后台网关尚未启动")?;
    let runtime = shared.runtime.clone();
    let mut shutdown = shared.shutdown.subscribe();
    let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
    let (completed, finished) = tokio::sync::watch::channel(false);
    let control = Arc::new(ClientControl { cancel, finished });
    let (sender, receiver) = mpsc::channel();
    let thread =
        std::thread::Builder::new()
            .name(format!("attach-{}", target.id()))
            .spawn(move || {
                let _trace_guard = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&log_path)
                    .ok()
                    .map(|file| {
                        tracing::subscriber::set_default(
                            tracing_subscriber::fmt()
                                .with_target(false)
                                .with_ansi(false)
                                .without_time()
                                .with_writer(Mutex::new(file))
                                .finish(),
                        )
                    });
                LOG.with(|log| *log.borrow_mut() = Some(log_path));
                // Keep thread-affine Windows mutexes on this thread; all async I/O runs on
                // the shared host runtime. No additional Agent-Switch process is started.
                let mut registered = false;
                let result = runtime.block_on(async {
            tokio::select! {
                biased;
                _ = shutdown.changed() => Ok(()),
                result = async {
                    if update_only {
                        crate::gui_worker::update_gateway(target, &gateway_url, key, conversion_enabled).await
                    } else {
                        prepare_client_reconnect(target, &gateway_url, &key, mode).await?;
                        shared.clients.lock().unwrap_or_else(|error| error.into_inner()).insert(target, control.clone());
                        registered = true;
                        crate::local_gateway::mark_reconfiguring(target);
                        // The old registered job and its registry cleanup have fully ended.
                        // Only then may a new job acquire the per-client mutex and inject its catalog.
                        tokio::select! {
                            _ = cancelled.changed() => Ok(()),
                            result = crate::runtime_state::run_registered(target, mode,
                                crate::gui_worker::run(target, &gateway_url, key, mode, conversion_enabled)) => result,
                        }
                    }
                } => result,
            }
        }).map_err(|error| crate::i18n::runtime_error(&error));
                if registered {
                    crate::local_gateway::mark_client_disconnected(target);
                    let mut clients = shared.clients.lock().unwrap_or_else(|error| error.into_inner());
                    if clients.get(&target).is_some_and(|active| Arc::ptr_eq(active, &control)) { clients.remove(&target); }
                }
                if let Err(error) = &result {
                    print_line(format_args!(
                        "\n{}:\n  {error}",
                        crate::i18n::tr("错误", "Error")
                    ));
                }
                let _ = completed.send(true);
                let _ = sender.send(result);
            })?;
    let mut threads = shared.threads.lock().unwrap_or_else(|e| e.into_inner());
    threads.retain(|thread| !thread.is_finished());
    threads.push(thread);
    Ok(Job(receiver))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn restart_waits_for_previous_monitor_cleanup() {
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let (completed, finished) = tokio::sync::watch::channel(false);
        let control = ClientControl { cancel, finished };
        let cleaned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = cleaned.clone();
        let monitor = tokio::spawn(async move {
            cancelled.changed().await.unwrap();
            assert!(*cancelled.borrow());
            tokio::task::yield_now().await;
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            completed.send(true).unwrap();
        });
        control.cancel_and_wait().await.unwrap();
        assert!(cleaned.load(std::sync::atomic::Ordering::SeqCst));
        monitor.await.unwrap();
        // Natural exit before a restart is also already fully cleaned up.
        control.cancel_and_wait().await.unwrap();
    }
}
