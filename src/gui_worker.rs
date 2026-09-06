use crate::catalog;
use crate::claude_desktop_proxy::{
    ClaudeDesktopModelMode, ClaudeDesktopProxy, DesktopModelMenu, SlotOverrides, select_model_slots,
};
use crate::config::{ClientPaths, InstallOptions};
use crate::gateway::{self, GatewayIdentity, GatewayProtocol};
use crate::i18n;
use crate::network_proxy;
use crate::{claude_desktop, config, desktop, platform};
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
) -> Result<()> {
    let api_key = Zeroizing::new(api_key);
    let gateway = GatewayIdentity::parse(gateway_url)?;
    let required_protocol = match target {
        ClientTarget::CodexCli | ClientTarget::CodexDesktop => GatewayProtocol::OpenAiResponses,
        ClientTarget::ClaudeCode | ClientTarget::ClaudeDesktop => {
            GatewayProtocol::AnthropicMessages
        }
    };
    gateway::ensure_protocol(gateway_url, &api_key, required_protocol).await?;
    match target {
        ClientTarget::CodexCli => {
            let paths = ClientPaths::launcher_codex_cli_profile()?;
            install_codex_cli_configuration(&paths, gateway_url, &api_key, model_list_mode).await?;
            let codex_home = paths.codex_home.to_string_lossy().into_owned();
            let working_directory = paths.codex_home.join("workspace");
            std::fs::create_dir_all(&working_directory).with_context(|| {
                format!(
                    "无法创建 Codex CLI 独立工作目录: {}",
                    working_directory.display()
                )
            })?;
            let proxy_environment = network_proxy::child_proxy_environment(gateway_url)?;
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
            let catalog =
                install_claude_code_configuration(&paths, gateway_url, &api_key, model_list_mode)
                    .await?;
            let proxy = if model_list_mode == ModelListMode::Gateway {
                let slots = select_model_slots(&catalog, &SlotOverrides::default())?;
                let menu = DesktopModelMenu::claude_code_catalog(&catalog, gateway.source_label())?;
                Some(
                    ClaudeDesktopProxy::start(gateway_url, api_key.to_string(), slots, menu)
                        .await?,
                )
            } else {
                None
            };
            let runtime_base_url = proxy
                .as_ref()
                .map(ClaudeDesktopProxy::base_url)
                .unwrap_or_else(|| gateway.base_url.clone());
            let runtime_token = proxy
                .as_ref()
                .map_or(api_key.as_str(), ClaudeDesktopProxy::local_token);
            let proxy_environment = network_proxy::child_proxy_environment(gateway_url)?;
            let mut environment = vec![
                ("ANTHROPIC_BASE_URL", runtime_base_url.as_str()),
                ("ANTHROPIC_AUTH_TOKEN", runtime_token),
                (
                    "CLAUDE_GATEWAY_ALLOW_LOOPBACK",
                    if proxy.is_some() { "1" } else { "0" },
                ),
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
                proxy.stop().await;
            }
        }
        ClientTarget::CodexDesktop => {
            desktop::restart_running_desktop().await?;
            let paths = ClientPaths::discover(None, None)?;
            // Desktop 已由 restart_running_desktop 精确关闭；独立 Codex CLI 不应阻塞启动。
            install_codex_configuration(&paths, gateway_url, &api_key, model_list_mode).await?;
            if model_list_mode == ModelListMode::Gateway {
                desktop::launch_and_inject(&paths, gateway_url, &api_key, None).await?;
            } else {
                desktop::launch_without_injection(&paths, None).await?;
            }
        }
        ClientTarget::ClaudeDesktop => {
            claude_desktop::launch(
                gateway_url,
                api_key.to_string(),
                SlotOverrides::default(),
                model_list_mode.claude_desktop_proxy_mode(),
            )
            .await?;
        }
    }
    Ok(())
}

async fn wait_for_terminal_exit(target: ClientTarget, terminal_pid: u32) -> Result<()> {
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
) -> Result<()> {
    let options = install_options(gateway_url);
    config::install_codex_cli_with_model_list(
        paths,
        &options,
        api_key,
        model_list_mode.uses_gateway_catalog(),
    )
    .await
}

async fn install_codex_configuration(
    paths: &ClientPaths,
    gateway_url: &str,
    api_key: &str,
    model_list_mode: ModelListMode,
) -> Result<()> {
    let options = install_options(gateway_url);
    config::install_codex_with_model_list(
        paths,
        &options,
        api_key,
        model_list_mode.uses_gateway_catalog(),
    )
    .await
}

async fn install_claude_code_configuration(
    paths: &ClientPaths,
    gateway_url: &str,
    api_key: &str,
    model_list_mode: ModelListMode,
) -> Result<serde_json::Value> {
    let options = install_options(gateway_url);
    config::install_claude_code_with_model_list(
        paths,
        &options,
        api_key,
        model_list_mode.uses_gateway_catalog(),
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
