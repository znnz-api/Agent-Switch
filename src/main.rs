#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

// Managed jobs keep the existing human-readable logs without process-wide
// stdout redirection or mutable environment variables shared between clients.
macro_rules! println {
    ($($arg:tt)*) => { crate::background::print_line(format_args!($($arg)*)) };
}

mod account_mode;
mod background;
mod backup;
mod catalog;
mod cdp;
mod claude_desktop;
mod claude_desktop_proxy;
mod claude_desktop_registry;
mod cli;
mod client_installer;
mod config;
mod desktop;
mod diagnostics;
mod gateway;
mod gui;
mod gui_convert;
mod gui_layout;
mod gui_providers;
mod gui_settings;
mod gui_spinner;
mod gui_worker;
mod helper;
mod i18n;
mod injection;
mod local_gateway;
mod network_proxy;
mod platform;
mod protocol;
mod protocol_stream;
mod runtime_state;
mod tool_signatures;
#[cfg(windows)]
mod tray;
mod usage;

use anyhow::{Context, Result, bail};
use clap::Parser;
use cli::{ClaudeDesktopCommand, Cli, Command};
use config::{ClientPaths, InstallOptions};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use tracing::warn;
use zeroize::Zeroize;

fn main() {
    let gui_mode = gui_requested();
    if !gui_mode {
        crate::platform::attach_parent_console();
    }

    if gui_mode {
        crate::platform::install_gui_panic_hook();
        let _gui_mutex =
            match crate::platform::try_acquire_named_mutex(r"Local\Agent-Switch-gui-v1") {
                Ok(Some(guard)) => guard,
                Ok(None) => {
                    let _ = crate::platform::activate_window_by_title(gui::WINDOW_TITLE);
                    return;
                }
                Err(error) => {
                    let message = format!(
                        "{}: {error:#}",
                        i18n::tr("GUI 单实例检查失败", "GUI single-instance check failed")
                    );
                    crate::platform::report_gui_startup_failure(&message);
                    std::process::exit(1);
                }
            };
        if let Err(error) = gui::run() {
            let message = format!(
                "{}: {error:#}",
                i18n::tr("GUI 启动失败", "GUI startup failed")
            );
            crate::platform::report_gui_startup_failure(&message);
            std::process::exit(1);
        }
        return;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            report_cli_failure(
                i18n::tr("无法初始化运行时", "Unable to initialize runtime"),
                &error.to_string(),
            );
            std::process::exit(1);
        }
    };
    if let Err(error) = runtime.block_on(run_cli()) {
        report_cli_failure(i18n::tr("错误", "Error"), &format!("{error:#}"));
        std::process::exit(1);
    }
}

fn report_cli_failure(title: &str, detail: &str) {
    let detail = if std::env::var_os("ZNNZ_RUNTIME_LOG_PATH").is_some() {
        i18n::runtime_error_text(detail)
    } else {
        detail.to_owned()
    };
    if std::env::var_os("ZNNZ_RUNTIME_LOG_PATH").is_some() {
        eprintln!("\n{title}:");
        for line in detail.lines() {
            eprintln!("  {}", line.trim_end());
        }
    } else {
        eprintln!("\n{title}: {detail}");
    }
}

fn gui_requested() -> bool {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    arguments.len() == 1
        || (arguments.len() == 2
            && arguments[1]
                .to_str()
                .is_some_and(|value| value.eq_ignore_ascii_case("gui")))
}

async fn run_cli() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "znnz_agent_launcher=info".into()),
        )
        .with_target(false)
        .with_ansi(std::io::stderr().is_terminal())
        .without_time()
        .compact()
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Diagnose {
        codex_home: None,
        claude_home: None,
        gateway_url: cli.gateway_url.clone(),
        online: false,
    }) {
        Command::Diagnose {
            codex_home,
            claude_home,
            gateway_url,
            online,
        } => {
            let paths = ClientPaths::discover(codex_home, claude_home)?;
            diagnostics::run(&paths, &gateway_url, online).await?;
        }
        Command::Install {
            codex_home,
            claude_home,
            gateway_url,
            api_key,
            default_model,
            dry_run,
            no_user_env,
            launch,
            allow_running,
        } => {
            let paths = ClientPaths::discover(codex_home, claude_home)?;
            ensure_safe_profile(&paths, allow_running)?;
            let mut key = acquire_key(api_key, &paths)?;
            let options = InstallOptions {
                gateway_url: gateway_url.clone(),
                default_model,
                dry_run,
                set_user_environment: !no_user_env,
            };
            let result = config::install(&paths, &options, &key).await;
            key.zeroize();
            result?;
            if launch && !dry_run {
                launch_desktop(&paths, &gateway_url).await?;
            }
        }
        Command::Refresh {
            codex_home,
            gateway_url,
            api_key,
            dry_run,
            allow_running,
        } => {
            let paths = ClientPaths::discover(codex_home, None)?;
            ensure_safe_profile(&paths, allow_running)?;
            let mut key = acquire_key(api_key, &paths)?;
            let result = config::refresh_catalog(&paths, &gateway_url, &key, dry_run).await;
            key.zeroize();
            result?;
        }
        Command::Launch {
            codex_home,
            gateway_url,
            api_key,
            desktop_exe,
            no_refresh,
            restart,
        } => {
            let paths = ClientPaths::discover(codex_home, None)?;
            if restart {
                desktop::restart_running_desktop().await?;
            }
            // --restart 已先请求窗口正常关闭，并在必要时精确清理已记录的 Desktop 进程树；
            // 此时不再用宽泛的进程名检查阻塞仍在运行的 Codex CLI。
            ensure_safe_profile(&paths, restart)?;
            let mut key = acquire_key(api_key, &paths)?;
            if !no_refresh
                && let Err(error) = config::refresh_catalog(&paths, &gateway_url, &key, false).await
            {
                warn!(
                    "{}: {}",
                    i18n::tr(
                        "启动前刷新模型失败，将使用本地目录",
                        "Failed to refresh models before launch; using the local catalog"
                    ),
                    i18n::runtime_error(&error)
                );
            }
            let result =
                desktop::launch_and_inject(&paths, &gateway_url, &key, desktop_exe.as_deref())
                    .await;
            key.zeroize();
            result?;
        }
        Command::ClaudeDesktop { command } => match command {
            ClaudeDesktopCommand::Diagnose {
                gateway_url,
                api_key,
                online,
            } => {
                let paths = ClientPaths::discover(None, None)?;
                let mut key = if online {
                    Some(acquire_key(api_key, &paths)?)
                } else {
                    None
                };
                let result = claude_desktop::diagnose(&gateway_url, key.as_deref(), online).await;
                if let Some(value) = key.as_mut() {
                    value.zeroize();
                }
                result?;
            }
            ClaudeDesktopCommand::Launch {
                gateway_url,
                api_key,
                haiku_model,
                sonnet_model,
                opus_model,
                fable_model,
                full_model_list,
            } => {
                let paths = ClientPaths::discover(None, None)?;
                let key = acquire_key(api_key, &paths)?;
                let overrides = claude_desktop_proxy::SlotOverrides {
                    haiku: haiku_model,
                    sonnet: sonnet_model,
                    opus: opus_model,
                    fable: fable_model,
                };
                let mode = if full_model_list {
                    claude_desktop_proxy::ClaudeDesktopModelMode::FullCatalog
                } else {
                    claude_desktop_proxy::ClaudeDesktopModelMode::FourSlots
                };
                claude_desktop::launch(&gateway_url, key, overrides, mode).await?;
            }
            ClaudeDesktopCommand::Restore => {
                claude_desktop::restore()?;
            }
        },
        Command::InternalRun {
            client,
            gateway_url,
            model_list_mode,
        } => {
            let key = gui_worker::key_from_environment()?;
            runtime_state::run_registered(
                client,
                model_list_mode,
                gui_worker::run(client, &gateway_url, key, model_list_mode, false),
            )
            .await?;
        }
        Command::InternalInstallClient { client } => {
            client_installer::install(client).await?;
        }
        Command::InternalTestGateway {
            gateway_url,
            fetch_models,
        } => {
            let key = gui_worker::key_from_environment()?;
            gui_worker::test_gateway(&gateway_url, key, fetch_models).await?;
        }
        Command::InternalGatewayUpdate {
            client,
            gateway_url,
        } => {
            let key = gui_worker::key_from_environment()?;
            gui_worker::update_gateway(client, &gateway_url, key, false).await?;
        }
        Command::InternalGateway => {
            local_gateway::run_daemon().await?;
        }
        Command::InternalClientStatus => {
            let processes = platform::running_client_processes()?;
            for target in gui_worker::ClientTarget::ALL {
                let pids = processes
                    .iter()
                    .filter(|(client, _)| *client == target)
                    .map(|(_, pid)| *pid)
                    .collect::<Vec<_>>();
                println!(
                    "{}",
                    serde_json::json!({"client": target.id(), "running": !pids.is_empty(), "pids": pids})
                );
            }
        }
        Command::HelperTest { catalog, seconds } => {
            let value = catalog::read_catalog(&catalog)?;
            let helper = helper::Helper::start(value).await?;
            println!("Helper: {}", helper.catalog_url());
            tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
            helper.stop().await;
        }
    }
    Ok(())
}

fn ensure_safe_profile(paths: &ClientPaths, allow_running: bool) -> Result<()> {
    let default_home = ClientPaths::default_codex_home()?;
    if paths.codex_home == default_home && !allow_running {
        let running = platform::running_codex_processes()?;
        if !running.is_empty() {
            bail!(
                "检测到 Codex 相关进程仍在运行: {}。为保护历史任务，写配置前请从托盘退出 Codex Desktop；或先用 --codex-home 指向隔离测试目录。",
                running.join("、")
            );
        }
    }
    Ok(())
}

fn acquire_key(cli_key: Option<String>, paths: &ClientPaths) -> Result<String> {
    if let Some(value) = cli_key.filter(|value| !value.trim().is_empty()) {
        return Ok(value);
    }
    for name in ["ZNNZ_API_KEY", "LITEAPI_API_KEY", "ANTHROPIC_AUTH_TOKEN"] {
        if let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            return Ok(value);
        }
    }
    if let Some(value) = config::read_codex_api_key(&paths.codex_auth)? {
        return Ok(value);
    }
    let value = rpassword::prompt_password("请输入网关 API Key（不会显示）: ")
        .context("读取 API Key 失败")?;
    if value.trim().is_empty() {
        bail!("API Key 不能为空");
    }
    Ok(value)
}

async fn launch_desktop(paths: &ClientPaths, gateway_url: &str) -> Result<()> {
    let mut key = config::read_codex_api_key(&paths.codex_auth)?
        .context("安装完成但 auth.json 中没有 API Key")?;
    let result = desktop::launch_and_inject(paths, gateway_url, &key, None).await;
    key.zeroize();
    result
}

#[allow(dead_code)]
fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[allow(dead_code)]
fn absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}
