use crate::claude_desktop_proxy::{
    ClaudeDesktopModelMode, ClaudeDesktopProxy, DesktopModelMenu, ModelSlots, SlotOverrides,
    select_model_slots,
};
use crate::{catalog, claude_desktop_registry as registry, i18n, platform};
use anyhow::{Context, Result, bail};
use std::time::Duration;
use tracing::{info, warn};
use zeroize::Zeroizing;

const DISCOVERY_CLIENT_VERSION: &str = "claude-desktop-1.24012.9";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_MISSED_POLLS: usize = 2;

pub async fn diagnose(gateway_url: &str, api_key: Option<&str>, online: bool) -> Result<()> {
    println!("znnz-client Claude Desktop 只读诊断\n");

    match registry::status() {
        Ok(status) => {
            println!(
                "注册表策略键:       {}",
                if status.policy_key_exists {
                    "存在"
                } else {
                    "不存在"
                }
            );
            println!(
                "受管注册表值:       {}",
                if status.managed_values_present.is_empty() {
                    "未设置".to_owned()
                } else {
                    status.managed_values_present.join(", ")
                }
            );
            println!(
                "待恢复加密备份:     {}",
                if status.pending_backup {
                    "存在"
                } else {
                    "不存在"
                }
            );
            println!("备份位置:           {}", status.backup_path.display());
        }
        Err(error) => println!("注册表检查:         失败: {error:#}"),
    }

    match platform::running_claude_desktop_processes() {
        Ok(processes) if processes.is_empty() => println!("运行中的 Desktop:  未检测到"),
        Ok(processes) => println!("运行中的 Desktop:  {}", processes.join("；")),
        Err(error) => println!("运行中的 Desktop:  检查失败: {error:#}"),
    }

    if online {
        let key = api_key.context("在线诊断需要 API Key")?;
        println!("\n在线检查:");
        let catalog = catalog::fetch_catalog(gateway_url, key, DISCOVERY_CLIENT_VERSION).await?;
        let visible = catalog::visible_slugs(&catalog);
        let slots = select_model_slots(&catalog, &SlotOverrides::default())?;
        println!("  网关模型目录:     正常，{} 个可见模型", visible.len());
        print_slots(&slots);
    }

    println!("\n诊断未修改注册表、Claude 数据目录或历史任务，也未输出完整 API Key。");
    Ok(())
}

pub async fn launch(
    gateway_url: &str,
    api_key: String,
    overrides: SlotOverrides,
    mode: ClaudeDesktopModelMode,
) -> Result<()> {
    let status = registry::status()?;
    let running = platform::running_claude_desktop_processes()?;
    if !running.is_empty() {
        if i18n::language() == i18n::Language::ZhCn {
            let pending = if status.pending_backup {
                "并且检测到上次异常退出遗留的注册表备份；请先完全退出 Desktop，再运行 claude-desktop restore"
            } else {
                ""
            };
            bail!(
                "检测到 Claude Desktop 仍在运行: {}。为保护历史任务并确保注册表配置生效，请先从托盘完全退出 Claude Desktop后重试。{}",
                running.join("；"),
                pending
            );
        } else {
            let pending = if status.pending_backup {
                " An encrypted registry backup from an earlier abnormal exit is also present; fully exit Desktop, then run claude-desktop restore first."
            } else {
                ""
            };
            bail!(
                "Claude Desktop is still running: {}. Fully exit Claude Desktop from the tray before retrying to protect existing tasks and apply the registry configuration.{}",
                running.join(", "),
                pending
            );
        }
    }
    if registry::restore_pending()? {
        println!(
            "{}",
            i18n::tr(
                "检测到上次异常退出遗留的加密备份，已先恢复原 Claude Desktop 注册表配置。",
                "Recovered the original Claude Desktop registry settings from a backup left by an earlier abnormal exit."
            )
        );
    }

    let api_key = Zeroizing::new(api_key);
    println!(
        "{}",
        i18n::tr(
            "正在拉取网关模型目录……",
            "Fetching the gateway model catalog…"
        )
    );
    let catalog = catalog::fetch_catalog(gateway_url, &api_key, DISCOVERY_CLIENT_VERSION).await?;
    let slots = select_model_slots(&catalog, &overrides)?;
    let menu = match mode {
        ClaudeDesktopModelMode::FourSlots => {
            println!(
                "{}",
                i18n::tr(
                    "Claude Desktop 将使用以下四个兼容模型槽位:",
                    "Claude Desktop will use these four compatible model slots:"
                )
            );
            print_slots(&slots);
            DesktopModelMenu::four_slots()
        }
        ClaudeDesktopModelMode::FullCatalog => {
            let gateway = crate::gateway::GatewayIdentity::parse(gateway_url)?;
            let menu = DesktopModelMenu::full_catalog_with_source(
                &catalog,
                &slots,
                gateway.source_label(),
            )?;
            println!(
                "{} {} {}",
                i18n::tr(
                    "Claude Desktop 将使用完整网关模型列表：",
                    "Claude Desktop will use the full gateway catalog:"
                ),
                menu.model_count(),
                i18n::tr("个模型（实验模式）。", "models (experimental).")
            );
            println!(
                "{}",
                i18n::tr(
                    "Claude 族默认槽位映射:",
                    "Default Claude-family slot mapping:"
                )
            );
            print_slots(&slots);
            println!(
                "{}",
                i18n::tr(
                    "提示：图片、视频等非文本模型虽然会列出，但可能不适用于 Claude 聊天界面。",
                    "Note: image, video, and other non-text models may be listed but might not work in the Claude chat interface."
                )
            );
            if let Some(default_model) = menu.default_model_name() {
                if i18n::language() == i18n::Language::ZhCn {
                    println!("完整列表默认模型:     {default_model}（优先 Claude 自家模型）");
                } else {
                    println!(
                        "Full-catalog default model: {default_model} (prefers Claude-family models)"
                    );
                }
            }
            menu
        }
    };
    let inference_models = menu.registry_models_json()?;
    let discovery_enabled = match menu.mode() {
        ClaudeDesktopModelMode::FourSlots => None,
        ClaudeDesktopModelMode::FullCatalog => Some(false),
    };
    if menu.mode() == ClaudeDesktopModelMode::FullCatalog {
        if i18n::language() == i18n::Language::ZhCn {
            println!(
                "完整列表兼容层:     {} 个模型使用稳定的 Claude 路由 ID，界面仍显示网关原始模型名。",
                menu.aliased_model_count()
            );
            println!(
                "注册表模型格式:     对象数组，{} 个条目，{} 字节。",
                menu.model_count(),
                inference_models.len()
            );
            println!("模型发现设置:       关闭；使用固定列表，同时本地 /v1/models 提供同一目录。");
        } else {
            println!(
                "Full-catalog compatibility: {} model(s) use stable Claude route IDs while the UI keeps the original gateway model names.",
                menu.aliased_model_count()
            );
            println!(
                "Registry model format: object array, {} entries, {} bytes.",
                menu.model_count(),
                inference_models.len()
            );
            println!(
                "Model discovery: disabled; using a fixed list also served by local /v1/models."
            );
        }
    }

    let proxy = ClaudeDesktopProxy::start(gateway_url, api_key.to_string(), slots, menu).await?;
    registry::capture_and_store()?;
    if let Err(error) = registry::write_temporary_gateway(
        &proxy.base_url(),
        proxy.local_token(),
        &inference_models,
        discovery_enabled,
    ) {
        let restore = registry::restore_pending();
        proxy.stop().await;
        if let Err(restore_error) = restore {
            return Err(error).context(format!(
                "写入临时 Claude Desktop 配置失败，且恢复原注册表也失败: {restore_error:#}"
            ));
        }
        return Err(error);
    }

    println!(
        "{}",
        i18n::tr(
            "已创建当前 Windows 用户专用的加密注册表备份。",
            "Created an encrypted registry backup for the current Windows user."
        )
    );
    println!(
        "{}:       {}{}",
        i18n::tr("本地安全代理", "Local secure proxy"),
        proxy.base_url(),
        i18n::tr("（仅监听 127.0.0.1）", " (listens only on 127.0.0.1)")
    );
    println!(
        "{}",
        i18n::tr(
            "正在通过本地安全代理启动 Claude Desktop …",
            "Starting Claude Desktop through the local secure proxy …"
        )
    );

    let session_result = run_desktop_session().await;
    proxy.stop().await;
    let restore_result = registry::restore_pending();

    match (session_result, restore_result) {
        (Ok(()), Ok(_)) => {
            println!(
                "{}",
                i18n::tr(
                    "Claude Desktop 已退出；本地代理已停止，原注册表配置已恢复。",
                    "Claude Desktop exited. The local proxy stopped and the original registry settings were restored."
                )
            );
            Ok(())
        }
        (Err(error), Ok(_)) => Err(error),
        (Ok(()), Err(restore_error)) => Err(restore_error).context(
            "Claude Desktop 已退出，但原注册表配置恢复失败；请运行 claude-desktop restore",
        ),
        (Err(error), Err(restore_error)) => Err(error).context(format!(
            "同时无法恢复原注册表配置: {restore_error:#}。请运行 claude-desktop restore"
        )),
    }
}

pub fn restore() -> Result<()> {
    if !platform::running_claude_desktop_processes()?.is_empty() {
        bail!("Claude Desktop 仍在运行。请先从托盘完全退出，再恢复注册表配置。");
    }
    if registry::restore_pending()? {
        println!("已恢复 Claude Desktop 原注册表配置，并删除加密备份。");
    } else {
        println!("没有检测到待恢复的 Claude Desktop 注册表备份。");
    }
    Ok(())
}

async fn run_desktop_session() -> Result<()> {
    let activation_pid = platform::activate_store_claude().await?;
    println!(
        "{}",
        if i18n::language() == i18n::Language::ZhCn {
            format!("已启动 Claude Desktop（Microsoft Store 版激活 PID {activation_pid}）。")
        } else {
            format!("Claude Desktop started (Microsoft Store activation PID {activation_pid}).")
        }
    );

    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    loop {
        let pids = platform::running_claude_desktop_process_ids()?;
        if !pids.is_empty() {
            let pids = pids
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "{}",
                if i18n::language() == i18n::Language::ZhCn {
                    format!(
                        "已检测到 Claude Desktop 进程（PID: {pids}）。启动器会在后台保持本地代理；完全退出 Desktop 后会自动恢复注册表并结束。"
                    )
                } else {
                    format!(
                        "Detected Claude Desktop process(es) (PID: {pids}). The launcher will keep the local proxy running in the background and restore the original registry settings after Desktop fully exits."
                    )
                }
            );
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "{}",
                if i18n::language() == i18n::Language::ZhCn {
                    format!(
                        "启动后 {} 秒内未检测到官方 Claude Desktop 进程。已停止启动并将恢复原注册表。",
                        START_TIMEOUT.as_secs()
                    )
                } else {
                    format!(
                        "The official Claude Desktop process was not detected within {} seconds after launch. Startup was stopped and the original registry will be restored.",
                        START_TIMEOUT.as_secs()
                    )
                }
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let mut missed = 0usize;
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal.context(i18n::tr("监听 Ctrl+C 失败", "Failed to listen for Ctrl+C"))?;
                warn!(
                    "{}",
                    i18n::tr(
                        "收到 Ctrl+C；本地代理将停止并恢复注册表。仍在运行的 Claude Desktop 需要重新启动后才能继续调用网关。",
                        "Received Ctrl+C; the local proxy will stop and the registry will be restored. Restart any still-running Claude Desktop instance before using the gateway again."
                    )
                );
                return Ok(());
            }
            _ = tokio::time::sleep(WATCH_INTERVAL) => {}
        }

        let pids = platform::running_claude_desktop_process_ids()?;
        if pids.is_empty() {
            missed += 1;
            if missed >= MAX_MISSED_POLLS {
                info!(
                    "{}",
                    i18n::tr(
                        "Claude Desktop 已退出，正在停止本地代理并恢复注册表",
                        "Claude Desktop exited; stopping the local proxy and restoring the registry"
                    )
                );
                return Ok(());
            }
        } else {
            missed = 0;
        }
    }
}

fn print_slots(slots: &ModelSlots) {
    println!("  Haiku:  {}", slots.haiku);
    println!("  Sonnet: {}", slots.sonnet);
    println!("  Opus:   {}", slots.opus);
    println!("  Fable:  {}", slots.fable);
}
