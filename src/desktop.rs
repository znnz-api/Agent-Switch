use crate::{
    catalog, cdp,
    config::{self, ClientPaths},
    gateway::GatewayIdentity,
    helper::Helper,
    i18n, injection, platform,
};
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;
use tokio::process::{Child, Command};
use tracing::{info, warn};

const START_TIMEOUT: Duration = Duration::from_secs(120);
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_MISSED_POLLS: usize = 2;
const RESTART_WINDOW_GRACE: Duration = Duration::from_secs(3);
const RESTART_TERMINATE_TIMEOUT: Duration = Duration::from_secs(5);

enum DesktopLaunch {
    Standalone(Box<Child>),
    Store { pid: u32 },
}

impl DesktopLaunch {
    fn description(&self) -> String {
        match self {
            Self::Standalone(child) => {
                if i18n::language() == i18n::Language::ZhCn {
                    format!("独立版 PID {}", child.id().unwrap_or_default())
                } else {
                    format!("standalone PID {}", child.id().unwrap_or_default())
                }
            }
            Self::Store { pid } => {
                if i18n::language() == i18n::Language::ZhCn {
                    format!("Microsoft Store 版 PID {pid}")
                } else {
                    format!("Microsoft Store PID {pid}")
                }
            }
        }
    }

    fn exited(&mut self) -> Result<bool> {
        match self {
            Self::Standalone(child) => Ok(child.try_wait()?.is_some()),
            // Store/Electron may hand the activation PID to another process; do not rely on that PID alone.
            Self::Store { .. } => Ok(false),
        }
    }
}

pub async fn restart_running_desktop() -> Result<()> {
    let companions = platform::running_codex_companion_processes()?;
    if !companions.is_empty() {
        if i18n::language() == i18n::Language::ZhCn {
            bail!(
                "检测到 Codex++ 相关进程仍在运行: {}。为避免它同时改写配置或接管 Desktop，请先完全退出 Codex++ 后重试。",
                companions.join("、")
            );
        } else {
            bail!(
                "Codex++ processes are still running: {}. Exit Codex++ completely before retrying to prevent configuration conflicts.",
                companions.join(", ")
            );
        }
    }

    let close = platform::request_codex_desktop_close()?;
    if close.window_processes.is_empty() {
        println!(
            "{}",
            i18n::tr(
                "未检测到需要关闭的 Codex Desktop 窗口；本次未执行关闭或后台清理，将直接启动。",
                "No Codex Desktop window needs to be closed. Starting directly."
            )
        );
        return Ok(());
    }

    let descriptions = close
        .window_processes
        .iter()
        .map(|(pid, name)| format!("{name} (PID {pid})"))
        .collect::<Vec<_>>();
    println!(
        "{}",
        if i18n::language() == i18n::Language::ZhCn {
            format!(
                "已向 {} 个 Codex Desktop 窗口发送正常关闭请求：{}",
                close.window_count,
                descriptions.join("、")
            )
        } else {
            format!(
                "Sent normal close requests to {} Codex Desktop window(s): {}",
                close.window_count,
                descriptions.join(", ")
            )
        }
    );
    println!(
        "{} {} {}…",
        i18n::tr(
            "正在等待 Codex Desktop 保存状态并退出，最多等待",
            "Waiting up to"
        ),
        RESTART_WINDOW_GRACE.as_secs(),
        i18n::tr("秒", "seconds for Codex Desktop to save and exit")
    );

    let pids = close
        .process_tree
        .iter()
        .map(|(pid, _)| *pid)
        .collect::<Vec<_>>();
    let graceful_deadline = tokio::time::Instant::now() + RESTART_WINDOW_GRACE;
    loop {
        let running = platform::running_process_ids(&pids)?;
        if running.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            println!(
                "{}",
                i18n::tr(
                    "Codex Desktop 已正常退出，准备重新启动。",
                    "Codex Desktop exited normally and is ready to restart."
                )
            );
            return Ok(());
        }
        if tokio::time::Instant::now() >= graceful_deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let running = platform::running_process_ids(&pids)?;
    println!(
        "{}",
        if i18n::language() == i18n::Language::ZhCn {
            format!(
                "Windows 已正常关闭，但仍有 {} 个后台进程。正在清理 Codex Desktop 进程树（PID：{}）……",
                running.len(),
                running
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            format!(
                "Windows closed normally; {} background process(es) remain. Cleaning the Codex Desktop process tree (PIDs: {})...",
                running.len(),
                running
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    );
    let terminate_pids = pids.clone();
    let terminated = tokio::task::spawn_blocking(move || {
        platform::terminate_codex_desktop_process_tree(&terminate_pids)
    })
    .await
    .context(i18n::tr(
        "Codex Desktop 后台进程清理任务执行失败",
        "Codex Desktop background process cleanup failed",
    ))??;
    if !terminated.is_empty() {
        println!(
            "{}",
            if i18n::language() == i18n::Language::ZhCn {
                format!(
                    "已清理 {} 个无法自行退出的 Codex Desktop 后台进程。",
                    terminated.len()
                )
            } else {
                format!(
                    "Cleaned up {} Codex Desktop background process(es) that did not exit on their own.",
                    terminated.len()
                )
            }
        );
    }

    let terminate_deadline = tokio::time::Instant::now() + RESTART_TERMINATE_TIMEOUT;
    loop {
        let remaining = platform::running_process_ids(&pids)?;
        if remaining.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            println!(
                "{}",
                i18n::tr(
                    "Codex Desktop 已完整退出，准备重新启动。",
                    "Codex Desktop has fully exited and is ready to restart."
                )
            );
            return Ok(());
        }
        if tokio::time::Instant::now() >= terminate_deadline {
            bail!(
                "{}",
                if i18n::language() == i18n::Language::ZhCn {
                    format!(
                        "等待 {} 秒后 Codex Desktop 后台进程仍存在，已停止重启：{}",
                        RESTART_TERMINATE_TIMEOUT.as_secs(),
                        remaining
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                } else {
                    format!(
                        "Codex Desktop background processes still have PIDs after {} seconds; restart was stopped: {}",
                        RESTART_TERMINATE_TIMEOUT.as_secs(),
                        remaining
                            .iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub async fn launch_and_inject(
    paths: &ClientPaths,
    gateway_url: &str,
    _api_key: &str,
    desktop_exe: Option<&Path>,
) -> Result<()> {
    let gateway = GatewayIdentity::parse(gateway_url)?;
    let catalog_value = catalog::read_catalog(&paths.codex_catalog).with_context(|| {
        if i18n::language() == i18n::Language::ZhCn {
            format!(
                "启动前需要有效模型目录: {}；请先运行 install 或 refresh",
                paths.codex_catalog.display()
            )
        } else {
            format!(
                "A valid model catalog is required before launch: {}. Run install or refresh first.",
                paths.codex_catalog.display()
            )
        }
    })?;
    let default_model = selected_model(&paths.codex_config)
        .or_else(|| catalog::visible_slugs(&catalog_value).into_iter().next());
    let helper = Helper::start(catalog_value.clone()).await?;
    let renderer_injection = injection::build(
        &helper.catalog_url(),
        &catalog_value,
        default_model.as_deref(),
        &gateway.source_label(),
    )?;
    let debug_port = cdp::reserve_loopback_port()?;

    let session = async {
        let mut launch = launch_desktop(paths, desktop_exe, debug_port).await?;
        println!(
            "{} Codex Desktop ({})",
            i18n::tr("已启动", "Started"),
            launch.description()
        );
        println!(
            "{}",
            i18n::tr(
                "正在等待模型选择器注入，请不要关闭本窗口……",
                "Waiting for model picker injection. Do not close this window…"
            )
        );

        let mut injected = wait_for_initial_injection(debug_port, &renderer_injection, &mut launch).await?;
        println!(
            "{}",
            if i18n::language() == i18n::Language::ZhCn {
                format!("已向 {} 个渲染页面成功注入模型目录；请在 Codex Desktop 中选择网关模型。", injected.len())
            } else {
                format!(
                    "Model injection succeeded for {} rendered page(s); select a gateway model in Codex Desktop.",
                    injected.len()
                )
            }
        );
        println!(
            "{}",
            i18n::tr(
                "启动器会在后台维护注入；退出 Codex Desktop 后本程序会自动结束。",
                "The launcher will maintain injection in the background and exit after Codex Desktop closes."
            )
        );

        watchdog(debug_port, &renderer_injection, &mut launch, &mut injected).await
    }
    .await;

    helper.stop().await;
    session
}

pub async fn launch_without_injection(
    paths: &ClientPaths,
    desktop_exe: Option<&Path>,
) -> Result<()> {
    let debug_port = cdp::reserve_loopback_port()?;
    let mut launch = launch_desktop(paths, desktop_exe, debug_port).await?;
    println!(
        "{} Codex Desktop ({})",
        i18n::tr("已启动", "Started"),
        launch.description()
    );
    println!(
        "{}",
        i18n::tr(
            "当前使用默认模型列表；不会注入网关模型目录。",
            "Using the default model list; the gateway catalog will not be injected."
        )
    );
    wait_for_desktop_ready(debug_port, &mut launch).await?;
    println!(
        "{}",
        i18n::tr(
            "Codex Desktop 已启动；关闭 Codex Desktop 后本程序会自动结束。",
            "Codex Desktop started. The launcher will exit after Codex Desktop closes."
        )
    );
    watch_desktop_exit(debug_port, &mut launch).await
}

async fn wait_for_desktop_ready(debug_port: u16, launch: &mut DesktopLaunch) -> Result<()> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if launch.exited()? {
            bail!(
                "{}",
                i18n::tr(
                    "Codex Desktop 在启动完成前退出",
                    "Codex Desktop exited before startup completed"
                )
            )
        }
        if cdp::port_reachable(debug_port).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!(
        "Codex Desktop startup timed out after {} seconds. Make sure the old Codex Desktop instance is fully closed.",
        START_TIMEOUT.as_secs()
    )
}

async fn watch_desktop_exit(debug_port: u16, launch: &mut DesktopLaunch) -> Result<()> {
    let mut missed = 0usize;
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context(i18n::tr("监听 Ctrl+C 失败", "Failed to listen for Ctrl+C"))?;
                info!(
                    "{}",
                    i18n::tr(
                        "收到 Ctrl+C；Codex Desktop 保持运行，启动器正在结束",
                        "Received Ctrl+C; leaving Codex Desktop running and exiting the launcher"
                    )
                );
                return Ok(());
            }
            _ = tokio::time::sleep(WATCH_INTERVAL) => {}
        }
        if launch.exited()? {
            info!(
                "{}",
                i18n::tr("Codex Desktop 已退出", "Codex Desktop exited")
            );
            return Ok(());
        }
        if cdp::port_reachable(debug_port).await {
            missed = 0;
            continue;
        }
        missed += 1;
        if missed >= MAX_MISSED_POLLS {
            info!(
                "{}",
                i18n::tr(
                    "Codex Desktop 已退出，启动器正在结束",
                    "Codex Desktop exited; exiting the launcher"
                )
            );
            return Ok(());
        }
    }
}

async fn launch_desktop(
    paths: &ClientPaths,
    desktop_exe: Option<&Path>,
    debug_port: u16,
) -> Result<DesktopLaunch> {
    if config::remove_codex_desktop_locale_override(&paths.codex_config)? {
        println!(
            "{}",
            i18n::tr(
                "已移除旧版 Codex Desktop localeOverride，恢复系统语言自动选择。",
                "Removed the legacy Codex Desktop localeOverride; system language selection is restored."
            )
        );
    }
    let flags = vec![
        format!("--remote-debugging-port={debug_port}"),
        format!("--remote-allow-origins=http://127.0.0.1:{debug_port}"),
    ];
    if let Some(executable) = desktop_exe {
        if !executable.is_file() {
            bail!(
                "指定的 Codex Desktop 可执行文件不存在: {}",
                executable.display()
            )
        }
        let child = Command::new(executable)
            .args(&flags)
            .env("CODEX_HOME", &paths.codex_home)
            .spawn()
            .with_context(|| {
                if i18n::language() == i18n::Language::ZhCn {
                    format!("无法启动 {}", executable.display())
                } else {
                    format!("Unable to start {}", executable.display())
                }
            })?;
        return Ok(DesktopLaunch::Standalone(Box::new(child)));
    }

    let default_home = ClientPaths::default_codex_home()?;
    if paths.codex_home != default_home {
        bail!(
            "{}",
            if i18n::language() == i18n::Language::ZhCn {
                format!(
                    "Microsoft Store 版启动不能安全地临时切换 CODEX_HOME。隔离测试目录 {} 必须同时提供 --desktop-exe；默认用户目录可以直接启动。",
                    paths.codex_home.display()
                )
            } else {
                format!(
                    "Microsoft Store launch cannot safely switch CODEX_HOME temporarily. Isolated test directory {} must be used with --desktop-exe; the default user directory can be launched directly.",
                    paths.codex_home.display()
                )
            }
        )
    }
    let arguments = flags
        .iter()
        .map(|value| quote_windows_argument(value))
        .collect::<Vec<_>>()
        .join(" ");
    let pid = platform::activate_store_codex(&arguments).await?;
    Ok(DesktopLaunch::Store { pid })
}

async fn wait_for_initial_injection(
    debug_port: u16,
    renderer_injection: &injection::ScriptBundle,
    launch: &mut DesktopLaunch,
) -> Result<HashSet<String>> {
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    let mut last_error = None;
    while tokio::time::Instant::now() < deadline {
        if launch.exited()? {
            bail!(
                "{}",
                i18n::tr(
                    "Codex Desktop 在 CDP 注入完成前退出",
                    "Codex Desktop exited before CDP injection completed"
                )
            )
        }
        match cdp::inject_all(debug_port, renderer_injection).await {
            Ok(targets) => return Ok(targets),
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
    bail!(
        "Codex Desktop CDP page injection timed out after {} seconds. Last error: {}. Make sure the old instance is fully closed and launch only through this EXE.",
        START_TIMEOUT.as_secs(),
        last_error
            .map(|error| format!("{error:#}"))
            .unwrap_or_else(|| i18n::tr("未知", "unknown").to_owned())
    )
}

async fn watchdog(
    debug_port: u16,
    renderer_injection: &injection::ScriptBundle,
    launch: &mut DesktopLaunch,
    injected: &mut HashSet<String>,
) -> Result<()> {
    let mut missed = 0usize;
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context(i18n::tr("监听 Ctrl+C 失败", "Failed to listen for Ctrl+C"))?;
                info!(
                    "{}",
                    i18n::tr(
                        "收到 Ctrl+C，停止本地 Helper；Codex Desktop 保持运行，但页面重载后将不再自动注入",
                        "Received Ctrl+C; stopping the local Helper. Codex Desktop will keep running, but reloads will no longer be injected automatically"
                    )
                );
                return Ok(());
            }
            _ = tokio::time::sleep(WATCH_INTERVAL) => {}
        }

        if launch.exited()? {
            info!(
                "{}",
                i18n::tr("Codex Desktop 已退出", "Codex Desktop exited")
            );
            return Ok(());
        }

        if !cdp::port_reachable(debug_port).await {
            missed += 1;
            if missed >= MAX_MISSED_POLLS {
                info!(
                    "{}",
                    i18n::tr(
                        "Codex Desktop 已退出，启动器正在结束",
                        "Codex Desktop exited; exiting the launcher"
                    )
                );
                return Ok(());
            }
            warn!(
                "{}",
                if i18n::language() == i18n::Language::ZhCn {
                    format!(
                        "Codex CDP 端口已关闭，正在快速确认 Desktop 是否已退出（最多等待 {} 秒）",
                        WATCH_INTERVAL.as_secs() * MAX_MISSED_POLLS as u64
                    )
                } else {
                    format!(
                        "Codex CDP port closed; quickly checking whether Desktop exited (waiting up to {} seconds)",
                        WATCH_INTERVAL.as_secs() * MAX_MISSED_POLLS as u64
                    )
                }
            );
            continue;
        }

        let targets = match cdp::list_targets(debug_port).await {
            Ok(value) => {
                missed = 0;
                cdp::injectable_targets(&value, debug_port)
            }
            Err(error) => {
                missed += 1;
                if missed >= MAX_MISSED_POLLS {
                    if i18n::language() == i18n::Language::ZhCn {
                        warn!("Codex CDP 持续不可用，启动器正在结束：{error:#}");
                    } else {
                        warn!(
                            "Codex CDP remains unavailable; exiting the launcher: {}",
                            i18n::runtime_error(&error)
                        );
                    }
                    return Ok(());
                }
                if i18n::language() == i18n::Language::ZhCn {
                    warn!("Codex CDP 暂时不可用，将在下一轮重试：{error:#}");
                } else {
                    warn!(
                        "Codex CDP temporarily unavailable; retrying next cycle: {}",
                        i18n::runtime_error(&error)
                    );
                }
                continue;
            }
        };

        for target in targets {
            let healthy = if injected.contains(&target.id) {
                cdp::target_health(&target, debug_port, renderer_injection)
                    .await
                    .unwrap_or(false)
            } else {
                false
            };
            if healthy {
                continue;
            }
            match cdp::inject_target(&target, debug_port, renderer_injection).await {
                Ok(()) => {
                    if injected.insert(target.id.clone()) {
                        if i18n::language() == i18n::Language::ZhCn {
                            info!("已向新渲染页面 {} 注入模型目录", target.id);
                        } else {
                            info!(
                                "Injected the model catalog into new rendered page {}",
                                target.id
                            );
                        }
                    } else {
                        info!("Restored model injection for target {}", target.id);
                    }
                }
                Err(error) => {
                    if i18n::language() == i18n::Language::ZhCn {
                        warn!("渲染页面 {} 重新注入失败: {error:#}", target.id);
                    } else {
                        warn!(
                            "Failed to reinject rendered page {}: {}",
                            target.id,
                            i18n::runtime_error(&error)
                        );
                    }
                }
            }
        }
    }
}

fn selected_model(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let doc = raw.parse::<toml_edit::DocumentMut>().ok()?;
    doc.get("model")
        .and_then(toml_edit::Item::as_str)
        .map(ToOwned::to_owned)
}

fn quote_windows_argument(value: &str) -> String {
    if !value.is_empty()
        && !value
            .bytes()
            .any(|byte| matches!(byte, b' ' | b'\t' | b'"'))
    {
        return value.to_owned();
    }
    let mut output = String::from("\"");
    let mut backslashes = 0usize;
    for ch in value.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                output.push_str(&"\\".repeat(backslashes * 2 + 1));
                output.push('"');
                backslashes = 0;
            }
            _ => {
                output.push_str(&"\\".repeat(backslashes));
                output.push(ch);
                backslashes = 0;
            }
        }
    }
    output.push_str(&"\\".repeat(backslashes * 2));
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_argument_quoting_preserves_spaces_and_quotes() {
        assert_eq!(quote_windows_argument("plain"), "plain");
        assert_eq!(quote_windows_argument("two words"), "\"two words\"");
        assert_eq!(quote_windows_argument("a\"b"), "\"a\\\"b\"");
    }
}
