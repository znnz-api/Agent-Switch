use crate::gui_worker::{ClientTarget, ModelListMode};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "znnz-agent-launcher",
    version,
    about = "Agent-Switch local AI gateway manager for Codex and Claude clients"
)]
pub struct Cli {
    /// 默认网关地址，可被子命令中的同名参数覆盖。
    #[arg(long, global = true, default_value = crate::gateway::DEFAULT_GATEWAY_URL)]
    pub gateway_url: String,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// 只读检查本机配置，不修改文件、不显示完整 Key。
    Diagnose {
        #[arg(long)]
        codex_home: Option<PathBuf>,
        #[arg(long)]
        claude_home: Option<PathBuf>,
        #[arg(long, default_value = crate::gateway::DEFAULT_GATEWAY_URL)]
        gateway_url: String,
        /// 使用已有 Key 验证网关和模型接口。
        #[arg(long)]
        online: bool,
    },
    /// 合并配置、拉取模型目录并配置 Codex/Claude。
    Install {
        #[arg(long)]
        codex_home: Option<PathBuf>,
        #[arg(long)]
        claude_home: Option<PathBuf>,
        #[arg(long, default_value = crate::gateway::DEFAULT_GATEWAY_URL)]
        gateway_url: String,
        #[arg(long, env = "ZNNZ_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
        /// 可选默认模型；未指定时从网关的可见对话模型中自动选择。
        #[arg(long)]
        default_model: Option<String>,
        /// 仅展示计划，不写文件。
        #[arg(long)]
        dry_run: bool,
        /// 不写当前用户环境变量，供隔离测试使用。
        #[arg(long)]
        no_user_env: bool,
        /// 配置成功后启动 Codex Desktop 并注入模型目录。
        #[arg(long)]
        launch: bool,
        /// 允许在检测到 Codex 进程时写默认配置（危险，不建议）。
        #[arg(long, hide = true)]
        allow_running: bool,
    },
    /// 仅刷新 Codex 模型目录。
    Refresh {
        #[arg(long)]
        codex_home: Option<PathBuf>,
        #[arg(long, default_value = crate::gateway::DEFAULT_GATEWAY_URL)]
        gateway_url: String,
        #[arg(long, env = "ZNNZ_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, hide = true)]
        allow_running: bool,
    },
    /// 通过 CDP 启动官方 Codex Desktop，并解锁网关模型选择器。
    Launch {
        #[arg(long)]
        codex_home: Option<PathBuf>,
        #[arg(long, default_value = crate::gateway::DEFAULT_GATEWAY_URL)]
        gateway_url: String,
        #[arg(long, env = "ZNNZ_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
        /// 独立安装版 Codex Desktop 的可执行文件；Store 版通常不需要。
        #[arg(long)]
        desktop_exe: Option<PathBuf>,
        #[arg(long)]
        no_refresh: bool,
        /// 先正常关闭并等待保存；必要时只清理关闭前记录的官方 Desktop 进程树。
        #[arg(long)]
        restart: bool,
    },
    /// 配置、诊断并启动 Claude Desktop 的第三方推理网关模式。
    ClaudeDesktop {
        #[command(subcommand)]
        command: ClaudeDesktopCommand,
    },
    /// GUI 内部工作进程：配置并启动选定客户端。
    #[command(hide = true)]
    InternalRun {
        #[arg(value_enum)]
        client: ClientTarget,
        #[arg(long, default_value = crate::gateway::DEFAULT_GATEWAY_URL)]
        gateway_url: String,
        #[arg(long, value_enum, default_value = "gateway")]
        model_list_mode: ModelListMode,
    },
    /// GUI 内部工作进程：安装选定客户端，不读取或继承 API Key。
    #[command(hide = true)]
    InternalInstallClient {
        #[arg(value_enum)]
        client: ClientTarget,
    },
    /// GUI 内部工作进程：验证网关和模型目录。
    #[command(hide = true)]
    InternalTestGateway {
        #[arg(long, default_value = crate::gateway::DEFAULT_GATEWAY_URL)]
        gateway_url: String,
        /// GUI 的“拉取模型”操作；未指定时为“测试连接”。
        #[arg(long)]
        fetch_models: bool,
    },
    /// GUI 内部工作进程：更新常驻本地网关的上游路由，不重启客户端。
    #[command(hide = true)]
    InternalGatewayUpdate {
        #[arg(value_enum)]
        client: ClientTarget,
        #[arg(long, default_value = crate::gateway::DEFAULT_GATEWAY_URL)]
        gateway_url: String,
    },
    /// GUI 内部常驻本地网关服务。
    #[command(hide = true)]
    InternalGateway,
    /// Read-only process detection diagnostics; never changes client configuration.
    #[command(hide = true)]
    InternalClientStatus,
    /// 开发用：只启动本地 Helper。
    #[command(hide = true)]
    HelperTest {
        #[arg(long)]
        catalog: PathBuf,
        #[arg(long, default_value_t = 30)]
        seconds: u64,
    },
}

#[derive(Debug, Subcommand)]
pub enum ClaudeDesktopCommand {
    /// 只读检查 Claude Desktop、注册表与网关模型，不修改任何历史数据。
    Diagnose {
        #[arg(long, default_value = crate::gateway::DEFAULT_GATEWAY_URL)]
        gateway_url: String,
        #[arg(long, env = "ZNNZ_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
        /// 同时连接网关，验证模型目录并显示自动选择的四个模型槽位。
        #[arg(long)]
        online: bool,
    },
    /// 临时配置本地安全代理并启动 Claude Desktop；退出后自动恢复原注册表。
    Launch {
        #[arg(long, default_value = crate::gateway::DEFAULT_GATEWAY_URL)]
        gateway_url: String,
        #[arg(long, env = "ZNNZ_API_KEY", hide_env_values = true)]
        api_key: Option<String>,
        #[arg(long)]
        haiku_model: Option<String>,
        #[arg(long)]
        sonnet_model: Option<String>,
        #[arg(long)]
        opus_model: Option<String>,
        #[arg(long)]
        fable_model: Option<String>,
        /// 实验：在 Claude Desktop 模型选择器中显示网关返回的完整模型列表。
        #[arg(long)]
        full_model_list: bool,
    },
    /// 恢复上次异常退出前加密备份的 Claude Desktop 注册表配置。
    Restore,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_restart_is_parsed() {
        let cli = Cli::try_parse_from(["znnz-client", "launch", "--restart"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Launch { restart: true, .. })
        ));
    }

    #[test]
    fn internal_run_model_list_mode_is_parsed() {
        let cli = Cli::try_parse_from([
            "znnz-client",
            "internal-run",
            "claude-desktop",
            "--model-list-mode",
            "built-in",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::InternalRun {
                client: ClientTarget::ClaudeDesktop,
                model_list_mode: ModelListMode::BuiltIn,
                ..
            })
        ));
    }

    #[test]
    fn internal_install_client_is_parsed() {
        let cli =
            Cli::try_parse_from(["znnz-client", "internal-install-client", "claude-code"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::InternalInstallClient {
                client: ClientTarget::ClaudeCode,
            })
        ));
    }

    #[test]
    fn claude_desktop_full_model_list_is_parsed() {
        let cli = Cli::try_parse_from([
            "znnz-client",
            "claude-desktop",
            "launch",
            "--full-model-list",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::ClaudeDesktop {
                command: ClaudeDesktopCommand::Launch {
                    full_model_list: true,
                    ..
                }
            })
        ));
    }
}
