use crate::catalog;
use crate::claude_desktop_proxy::{
    ClaudeDesktopModelMode, ClaudeDesktopProxy, DesktopModelMenu, SlotOverrides, select_model_slots,
};
use crate::config::{ClientPaths, InstallOptions};
use crate::gateway::GatewayIdentity;
use crate::i18n;
use crate::network_proxy;
use crate::{claude_desktop, config, desktop, local_gateway, platform};
use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ModelListMode {
    BuiltIn,
    #[default]
    Gateway,
}

impl ModelListMode {
    pub fn id(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::Gateway => "gateway",
        }
    }

    pub fn from_id(value: &str) -> Self {
        match value {
            "built-in" | "default" | "four-slots" => Self::BuiltIn,
            "gateway" | "full-catalog" => Self::Gateway,
            _ => Self::Gateway,
        }
    }

    fn uses_gateway_catalog(self) -> bool {
        self == Self::Gateway
    }

    fn claude_desktop_proxy_mode(self) -> ClaudeDesktopModelMode {
        match self {
            Self::BuiltIn => ClaudeDesktopModelMode::FourSlots,
            Self::Gateway => ClaudeDesktopModelMode::FullCatalog,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ClientTarget {
    CodexCli,
    CodexDesktop,
    ClaudeCode,
    ClaudeDesktop,
}

impl ClientTarget {
    pub const ALL: [Self; 4] = [
        Self::CodexCli,
        Self::CodexDesktop,
        Self::ClaudeCode,
        Self::ClaudeDesktop,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::CodexCli => "codex-cli",
            Self::CodexDesktop => "codex-desktop",
            Self::ClaudeCode => "claude-code",
            Self::ClaudeDesktop => "claude-desktop",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::CodexCli => "Codex CLI",
            Self::CodexDesktop => "Codex Desktop",
            Self::ClaudeCode => "Claude Code",
            Self::ClaudeDesktop => "Claude Desktop",
        }
    }

    pub fn from_id(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|target| target.id() == value)
            .unwrap_or(Self::CodexDesktop)
    }
}

pub async fn test_gateway(gateway_url: &str, api_key: String, fetch_models: bool) -> Result<()> {
    let api_key = Zeroizing::new(api_key);
    let gateway_url = gateway_url.trim().trim_end_matches('/');
    if fetch_models {
        println!(
            "{}",
            i18n::tr(
                "正在拉取网关模型目录……",
                "Fetching the gateway model catalog…"
            )
        );
    } else {
        println!(
            "{}",
            i18n::tr("正在测试接口连接……", "Testing the gateway connection…")
        );
    }
    let value = catalog::fetch_catalog(gateway_url, &api_key, "0.145.0").await?;
    if fetch_models {
        let visible = catalog::visible_slugs(&value);
        if let Some(log_path) = std::env::var_os("ZNNZ_RUNTIME_LOG_PATH") {
            let result_path = std::path::PathBuf::from(log_path).with_extension("models.json");
            crate::backup::atomic_write(&result_path, &serde_json::to_vec(&visible)?)?;
        }
        println!(
            "{} • {} {} • {}",
            i18n::tr("成功拉取", "Fetched"),
            visible.len(),
            i18n::tr("个可见模型", "visible models"),
            gateway_url
        );
    } else {
        println!("{} • {}", i18n::tr("成功连接", "Connected"), gateway_url);
    }
    Ok(())
}

pub async fn run(
    target: ClientTarget,
    gateway_url: &str,
    api_key: String,
    model_list_mode: ModelListMode,
    conversion_enabled: bool,
) -> Result<()> {
    let api_key = Zeroizing::new(api_key);
    let gateway = GatewayIdentity::parse(gateway_url)?;
    let hidden_models =
        crate::gui_settings::hidden_models_for_configuration(gateway_url, &api_key)?;
    let already_running = platform::running_client_processes()?
        .iter()
        .any(|(client, _)| *client == target);
    let local =
        local_gateway::ensure_and_update(target, gateway_url, &api_key, conversion_enabled).await?;
    local_gateway::mark_reconfiguring(target);
    let local_gateway_url = local.base_url.clone();
    let local_api_key = local.client_token.clone();
    match target {
        ClientTarget::CodexCli => {
            let paths = ClientPaths::launcher_codex_cli_profile()?;
            install_codex_cli_configuration(
                &paths,
                &local_gateway_url,
                &local_api_key,
                model_list_mode,
                &hidden_models,
            )
            .await?;
            if already_running {
                anyhow::bail!(
                    "{}",
                    i18n::tr(
                        "Codex CLI • 自动重启 • 失败!：已写入 Agent-Switch 独立配置；现有终端不能安全自动关闭，请退出后再次接入",
                        "Codex CLI • Auto-restart • failed!: the Agent-Switch profile has been saved. The existing terminal cannot be closed safely; exit it and connect again."
                    )
                );
            }
            let codex_home = paths.codex_home.to_string_lossy().into_owned();
            let working_directory = paths.codex_home.join("workspace");
            std::fs::create_dir_all(&working_directory).with_context(|| {
                format!(
                    "无法创建 Codex CLI 独立工作目录: {}",
                    working_directory.display()
                )
            })?;
            let proxy_environment = network_proxy::child_proxy_environment(&local_gateway_url)?;
            let mut environment = vec![("CODEX_HOME", codex_home.as_str())];
            environment.extend(
                proxy_environment
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            );
            let terminal_pid = platform::launch_terminal_client_with_environment_in_directory(
                &format!("{} - Codex CLI", gateway.display_name),
                "codex",
                &environment,
                &working_directory,
            )?;
            crate::background::remember_terminal(target, terminal_pid)?;
            println!(
                "{}",
                if i18n::language() == i18n::Language::ZhCn {
                    format!(
                        "Codex CLI 已在独立配置目录中启动：{}；工作目录：{}（终端 PID {}）",
                        paths.codex_home.display(),
                        working_directory.display(),
                        terminal_pid
                    )
                } else {
                    format!(
                        "Codex CLI started with isolated profile: {}; workspace: {}; terminal PID {}",
                        paths.codex_home.display(),
                        working_directory.display(),
                        terminal_pid
                    )
                }
            );
            wait_for_terminal_exit(ClientTarget::CodexCli, terminal_pid).await?;
        }
        ClientTarget::ClaudeCode => {
            let paths = ClientPaths::discover(None, None)?;
            let catalog = install_claude_code_configuration(
                &paths,
                &local_gateway_url,
                &local_api_key,
                model_list_mode,
                &hidden_models,
            )
            .await?;
            if already_running {
                anyhow::bail!(
                    "{}",
                    i18n::tr(
                        "Claude Code • 自动重启 • 失败!：配置已写入；现有终端不能安全自动关闭，请退出后再次接入",
                        "Claude Code • Auto-restart • failed!: configuration has been saved. The existing terminal cannot be closed safely; exit it and connect again."
                    )
                );
            }
            let proxy = if model_list_mode == ModelListMode::Gateway {
                let slots = select_model_slots(&catalog, &SlotOverrides::default())?;
                let source_label = catalog
                    .get("models")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|models| {
                        models.iter().find_map(|model| {
                            model
                                .get("description")
                                .and_then(serde_json::Value::as_str)
                                .map(str::trim)
                                .filter(|description| description.starts_with("From "))
                                .map(ToOwned::to_owned)
                        })
                    })
                    .unwrap_or_else(|| gateway.source_label());
                let menu = DesktopModelMenu::claude_code_catalog(&catalog, source_label)?;
                Some(
                    ClaudeDesktopProxy::start(
                        &local_gateway_url,
                        local_api_key.clone(),
                        slots,
                        menu,
                    )
                    .await?,
                )
            } else {
                None
            };
            let runtime_base_url = proxy
                .as_ref()
                .map(ClaudeDesktopProxy::base_url)
                .unwrap_or_else(|| local_gateway_url.clone());
            let runtime_token = proxy
                .as_ref()
                .map_or(local_api_key.as_str(), ClaudeDesktopProxy::local_token);
            if crate::background::is_managed() {
                config::save_claude_runtime_endpoint(&paths, &runtime_base_url, runtime_token)?;
            }
            let proxy_environment = network_proxy::child_proxy_environment(&local_gateway_url)?;
            let mut environment = vec![
                ("ANTHROPIC_BASE_URL", runtime_base_url.as_str()),
                ("ANTHROPIC_AUTH_TOKEN", runtime_token),
                ("CLAUDE_GATEWAY_ALLOW_LOOPBACK", "1"),
            ];
            environment.extend(
                proxy_environment
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            );
            let terminal_pid = platform::launch_terminal_client_with_environment(
                &format!("{} - Claude Code", gateway.display_name),
                "claude",
                &environment,
            )?;
            crate::background::remember_terminal(target, terminal_pid)?;
            println!(
                "{}",
                if i18n::language() == i18n::Language::ZhCn {
                    format!(
                        "Claude Code 已在新终端中启动（终端 PID {}）。",
                        terminal_pid
                    )
                } else {
                    format!("Claude Code started in a new terminal (PID {terminal_pid}).")
                }
            );
            wait_for_terminal_exit(ClientTarget::ClaudeCode, terminal_pid).await?;
            if let Some(proxy) = proxy {
                if crate::background::is_managed() {
                    crate::background::retain_proxy(target, proxy);
                } else {
                    proxy.stop().await;
                }
            }
        }
        ClientTarget::CodexDesktop => {
            let paths = ClientPaths::discover(None, None)?;
            install_codex_configuration(
                &paths,
                &local_gateway_url,
                &local_api_key,
                model_list_mode,
                &hidden_models,
            )
            .await?;
            if already_running {
                desktop::close_for_gateway_attach(target)
                    .await
                    .context(i18n::tr(
                        "Codex Desktop • 自动重启 • 失败!",
                        "Codex Desktop • Auto-restart • failed!",
                    ))?;
            }
            if model_list_mode == ModelListMode::Gateway {
                desktop::launch_and_inject(&paths, &local_gateway_url, &local_api_key, None)
                    .await
                    .context(i18n::tr(
                        "Codex Desktop • 自动重启 • 失败!",
                        "Codex Desktop • Auto-restart • failed!",
                    ))?;
            } else {
                desktop::launch_without_injection(&paths, None)
                    .await
                    .context(i18n::tr(
                        "Codex Desktop • 自动重启 • 失败!",
                        "Codex Desktop • Auto-restart • failed!",
                    ))?;
            }
        }
        ClientTarget::ClaudeDesktop => {
            claude_desktop::launch_with_hidden_models(
                &local_gateway_url,
                local_api_key,
                SlotOverrides::default(),
                model_list_mode.claude_desktop_proxy_mode(),
                &hidden_models,
            )
            .await?;
        }
    }
    Ok(())
}

pub async fn update_gateway(
    target: ClientTarget,
    gateway_url: &str,
    api_key: String,
    conversion_enabled: bool,
) -> Result<()> {
    let expected = local_gateway::configuration_fingerprint(gateway_url, &api_key)?;
    if local_gateway::attached_configuration_fingerprint(target).as_deref()
        != Some(expected.as_str())
    {
        anyhow::bail!(
            "{}",
            i18n::tr(
                "地址或 Key 已改变，请重新配置并重启客户端",
                "The URL or Key changed; reconfigure and restart the client"
            )
        );
    }
    local_gateway::update_attached(target, gateway_url, &api_key, conversion_enabled).await?;
    println!(
        "{}",
        i18n::tr(
            "协议转换已即时更新，客户端无需重启。",
            "Protocol conversion was updated immediately; the client does not need to restart."
        )
    );
    Ok(())
}

/// Restart in account mode without registering a gateway worker or injecting a
/// model catalog. CLI uses its own restored account home, not Desktop's home.
pub async fn restart_account_client(target: ClientTarget) -> Result<()> {
    match target {
        ClientTarget::CodexDesktop => {
            platform::activate_store_codex("").await?;
        }
        ClientTarget::ClaudeDesktop => {
            platform::activate_store_claude().await?;
        }
        ClientTarget::CodexCli | ClientTarget::ClaudeCode => {
            let paths = crate::account_mode::paths(target)?;
            let home = paths.codex_home.to_string_lossy().into_owned();
            let environment = if target == ClientTarget::CodexCli {
                vec![
                    ("CODEX_HOME", home.as_str()),
                    ("OPENAI_BASE_URL", ""),
                    ("OPENAI_API_KEY", ""),
                ]
            } else {
                vec![
                    ("ANTHROPIC_BASE_URL", ""),
                    ("ANTHROPIC_AUTH_TOKEN", ""),
                    ("ANTHROPIC_API_KEY", ""),
                    ("CLAUDE_GATEWAY_ALLOW_LOOPBACK", ""),
                    ("CLAUDE_CODE_OAUTH_TOKEN", ""),
                ]
            };
            let command = if target == ClientTarget::CodexCli {
                "codex"
            } else {
                "claude"
            };
            let pid = platform::launch_terminal_client_with_environment(
                &format!(
                    "Agent-Switch - {} - {}",
                    target.title(),
                    i18n::tr("账号模式", "Account mode")
                ),
                command,
                &environment,
            )?;
            // Retain ownership so a later Start • Connect can restart this terminal.
            crate::background::remember_terminal(target, pid)?;
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !platform::running_client_processes()?
        .iter()
        .any(|(client, _)| *client == target)
    {
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "{}",
                i18n::tr(
                    "未检测到重新启动的客户端",
                    "The restarted client was not detected"
                )
            );
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    println!(
        "{} • {}",
        target.title(),
        i18n::tr("已在账号模式下启动", "Started in account mode")
    );
    Ok(())
}

async fn wait_for_terminal_exit(target: ClientTarget, terminal_pid: u32) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !platform::running_client_processes()?
        .iter()
        .any(|(client, _)| *client == target)
    {
        if tokio::time::Instant::now() >= deadline
            || platform::running_any_process_ids(&[terminal_pid])?.is_empty()
        {
            anyhow::bail!(
                "{} • {}",
                target.title(),
                i18n::tr(
                    "自动重启 • 失败!：启动后未检测到客户端进程",
                    "Auto-restart • failed!: no client process was detected after launch."
                )
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    local_gateway::confirm_current_worker(target)?;
    loop {
        if !platform::running_any_process_ids(&[terminal_pid])?.contains(&terminal_pid) {
            println!(
                "{} {}.",
                target.title(),
                i18n::tr("终端已关闭", "terminal closed")
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn install_codex_cli_configuration(
    paths: &ClientPaths,
    gateway_url: &str,
    api_key: &str,
    model_list_mode: ModelListMode,
    hidden_models: &[String],
) -> Result<()> {
    let options = install_options(gateway_url);
    config::install_codex_cli_with_hidden_models(
        paths,
        &options,
        api_key,
        model_list_mode.uses_gateway_catalog(),
        hidden_models,
    )
    .await
}

async fn install_codex_configuration(
    paths: &ClientPaths,
    gateway_url: &str,
    api_key: &str,
    model_list_mode: ModelListMode,
    hidden_models: &[String],
) -> Result<()> {
    let options = install_options(gateway_url);
    config::install_codex_with_hidden_models(
        paths,
        &options,
        api_key,
        model_list_mode.uses_gateway_catalog(),
        hidden_models,
    )
    .await
}

async fn install_claude_code_configuration(
    paths: &ClientPaths,
    gateway_url: &str,
    api_key: &str,
    model_list_mode: ModelListMode,
    hidden_models: &[String],
) -> Result<serde_json::Value> {
    let options = install_options(gateway_url);
    config::install_claude_code_with_hidden_models(
        paths,
        &options,
        api_key,
        model_list_mode.uses_gateway_catalog(),
        hidden_models,
    )
    .await
}

fn install_options(gateway_url: &str) -> InstallOptions {
    InstallOptions {
        gateway_url: gateway_url.to_owned(),
        default_model: None,
        dry_run: false,
        set_user_environment: true,
    }
}

pub fn key_from_environment() -> Result<String> {
    std::env::var("ZNNZ_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .context("内部工作进程没有收到 API Key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ids_round_trip() {
        for target in ClientTarget::ALL {
            assert_eq!(ClientTarget::from_id(target.id()), target);
        }
        assert_eq!(ClientTarget::from_id("unknown"), ClientTarget::CodexDesktop);
    }

    #[test]
    fn model_list_mode_accepts_current_and_legacy_ids() {
        assert_eq!(ModelListMode::from_id("built-in"), ModelListMode::BuiltIn);
        assert_eq!(ModelListMode::from_id("four-slots"), ModelListMode::BuiltIn);
        assert_eq!(ModelListMode::from_id("gateway"), ModelListMode::Gateway);
        assert_eq!(
            ModelListMode::from_id("full-catalog"),
            ModelListMode::Gateway
        );
        assert_eq!(ModelListMode::from_id("unknown"), ModelListMode::Gateway);
    }
}
