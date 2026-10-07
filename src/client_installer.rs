use crate::gui_worker::ClientTarget;
use crate::i18n;
use crate::network_proxy;
use crate::platform;
use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_DOWNLOAD_BYTES: u64 = 1024 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);
// Claude 官方地址在无代理网络下可能长时间无响应；失败后切换到已验证的 x64 R2 备用源。
const CLAUDE_OFFICIAL_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);
// Claude MSIX 先尝试当前用户部署；只有系统明确拒绝后才请求管理员权限。
const CLAUDE_ELEVATED_INSTALL_TIMEOUT_MS: u32 = 10 * 60 * 1000;
const INSTALL_WAIT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

// 终端客户端安装：先快速判断官方源是否可用，避免国内网络在官方源上无限等待。
const SOURCE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const NPM_OFFICIAL_INSTALL_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const NPM_MIRROR_INSTALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const NODE_MIRROR_INDEX_TIMEOUT: Duration = Duration::from_secs(30);
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(10);
const CODEX_AUTO_LAUNCH_DETECT_TIMEOUT: Duration = Duration::from_secs(3);
const CODEX_AUTO_LAUNCH_CLOSE_GRACE: Duration = Duration::from_millis(1200);

const NPM_OFFICIAL_REGISTRY: &str = "https://registry.npmjs.org";
const NPM_MIRROR_REGISTRY: &str = "https://registry.npmmirror.com";
const NODE_OFFICIAL_INDEX: &str = "https://nodejs.org/dist/index.json";
const NODE_MIRROR_INDEX: &str = "https://npmmirror.com/mirrors/node/index.json";
const CODEX_DESKTOP_INSTALLER: &str = "https://get.microsoft.com/installer/download/9PLM9XGG6VKS";
const CLAUDE_DESKTOP_X64: &str = "https://claude.ai/api/desktop/win32/x64/msix/latest/redirect";
const CLAUDE_DESKTOP_ARM64: &str = "https://claude.ai/api/desktop/win32/arm64/msix/latest/redirect";
const CLAUDE_DESKTOP_FALLBACK: &str = "https://r.networkpe.top/znnz-claude-desktop";

const SENSITIVE_ENVIRONMENT_KEYS: &[&str] = &[
    "ZNNZ_API_KEY",
    "LITEAPI_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
];

#[derive(Debug, Deserialize)]
struct NodeRelease {
    version: String,
    lts: serde_json::Value,
    #[serde(default)]
    files: Vec<String>,
}

struct DownloadedFile {
    path: PathBuf,
    final_url: String,
    content_type: String,
}

pub async fn install(target: ClientTarget) -> Result<()> {
    // 安装流程不需要任何模型网关凭据；即使用户直接调用内部命令，也先从当前进程清除。
    for name in SENSITIVE_ENVIRONMENT_KEYS {
        unsafe { std::env::remove_var(name) };
    }

    if client_installed(target) {
        println!(
            "{} • {}",
            target.title(),
            i18n::tr("已安装，无需重复安装。", "Already installed.")
        );
        return Ok(());
    }

    if matches!(
        target,
        ClientTarget::CodexDesktop | ClientTarget::ClaudeDesktop
    ) {
        println!(
            "{} {} …",
            i18n::tr("开始安装", "Starting installation of"),
            target.title()
        );
    } else {
        println!(
            "{} {}. {}",
            i18n::tr("开始安装", "Starting installation of"),
            target.title(),
            i18n::tr(
                "安装过程不会读取或记录 API Key。",
                "The installation does not read or record the API key."
            )
        );
    }
    match target {
        ClientTarget::CodexCli | ClientTarget::ClaudeCode => {
            install_terminal_client(target).await?
        }
        ClientTarget::CodexDesktop => install_codex_desktop().await?,
        ClientTarget::ClaudeDesktop => install_claude_desktop().await?,
    }

    if !client_installed(target) {
        bail!("{} 安装流程已结束，但复检仍未发现客户端", target.title());
    }
    println!(
        "{} • {}",
        target.title(),
        i18n::tr("安装完成。", "Installation complete.")
    );
    Ok(())
}

fn client_installed(target: ClientTarget) -> bool {
    match target {
        ClientTarget::CodexCli => platform::terminal_client_installed("codex"),
        ClientTarget::CodexDesktop => platform::store_codex_installed(),
        ClientTarget::ClaudeCode => platform::terminal_client_installed("claude"),
        ClientTarget::ClaudeDesktop => platform::store_claude_installed(),
    }
}

async fn install_terminal_client(target: ClientTarget) -> Result<()> {
    let package = match target {
        ClientTarget::CodexCli => "@openai/codex",
        ClientTarget::ClaudeCode => "@anthropic-ai/claude-code",
        _ => bail!("{} 不是终端客户端", target.title()),
    };

    let npm = match find_npm() {
        Some(path) => path,
        None => {
            println!(
                "{}",
                i18n::tr(
                    "未检测到 Node.js/npm，正在自动安装 Node.js LTS……",
                    "Node.js/npm was not detected. Installing Node.js LTS…"
                )
            );
            install_node_lts().await?;
            find_npm().context("Node.js 安装结束后仍未找到 npm.cmd")?
        }
    };
    let node = find_node_for_npm(&npm).with_context(|| {
        format!(
            "已找到 npm.cmd，但没有找到配套的 node.exe：{}",
            npm.display()
        )
    })?;
    println!("Node.js: {}", node.display());
    println!("npm:     {}", npm.display());

    let client = http_client()?;
    let mut errors = Vec::new();
    for (label, registry, install_timeout, probe_first) in [
        (
            i18n::tr("npm 官方源", "official npm registry"),
            NPM_OFFICIAL_REGISTRY,
            NPM_OFFICIAL_INSTALL_TIMEOUT,
            true,
        ),
        (
            i18n::tr("npmmirror 国内镜像", "npmmirror registry"),
            NPM_MIRROR_REGISTRY,
            NPM_MIRROR_INSTALL_TIMEOUT,
            false,
        ),
    ] {
        if probe_first {
            println!(
                "{}",
                i18n::tr(
                    "正在检测 npm 官方源……",
                    "Checking the official npm registry…"
                )
            );
            if let Err(error) = probe_npm_registry(&client, registry).await {
                println!(
                    "{}: {}",
                    i18n::tr(
                        "npm 官方源不可用，正在切换 npmmirror 国内镜像",
                        "The official npm registry is unavailable; switching to npmmirror"
                    ),
                    i18n::runtime_error(&error)
                );
                errors.push(format!("{label}: {error:#}"));
                continue;
            }
        }

        println!(
            "{} {package} {} {label}…",
            i18n::tr("正在安装", "Installing"),
            i18n::tr("，来源", "from")
        );
        match run_npm_install(&npm, &node, package, registry, install_timeout).await {
            Ok(()) => {
                if wait_until(|| client_installed(target), Duration::from_secs(20)).await {
                    return Ok(());
                }
                errors.push(format!(
                    "{label}: npm 返回成功，但没有检测到 {} 命令",
                    target.title()
                ));
            }
            Err(error) => {
                println!(
                    "{label} {}: {}",
                    i18n::tr("安装失败，将尝试下一来源", "failed; trying the next source"),
                    i18n::runtime_error(&error)
                );
                errors.push(format!("{label}: {error:#}"));
            }
        }
    }
    bail!("{} 安装失败：{}", target.title(), errors.join("；"))
}

async fn probe_npm_registry(client: &reqwest::Client, registry: &str) -> Result<()> {
    let ping_url = format!("{}/-/ping", registry.trim_end_matches('/'));
    let probe = async {
        client
            .get(&ping_url)
            .send()
            .await
            .with_context(|| format!("无法访问 {ping_url}"))?
            .error_for_status()
            .with_context(|| format!("npm Registry 返回错误: {ping_url}"))?;
        Ok::<(), anyhow::Error>(())
    };
    tokio::time::timeout(SOURCE_PROBE_TIMEOUT, probe)
        .await
        .with_context(|| format!("npm 官方源连接超过 {} 秒", SOURCE_PROBE_TIMEOUT.as_secs()))??;
    Ok(())
}

async fn run_npm_install(
    npm: &Path,
    node: &Path,
    package: &str,
    registry: &str,
    install_timeout: Duration,
) -> Result<()> {
    let arguments = npm_install_arguments(package, registry)?;
    let cmd = std::env::var_os("COMSPEC")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));
    let mut command = sanitized_command(&cmd);
    command.args(["/D", "/C", "call"]).arg(npm).args(&arguments);
    for (name, value) in network_proxy::child_proxy_environment(registry)? {
        command.env(name, value);
    }

    // MSI 安装完成后，当前 GUI 进程继承的 PATH 不会自动刷新。npm 的 postinstall
    // 会再次执行 node install.cjs，因此必须显式把新安装的 Node.js 目录放在最前面。
    let runtime_path = npm_runtime_path(npm, node)?;
    command.env("PATH", runtime_path);
    command.kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("无法启动 npm: {}", npm.display()))?;
    let status = match tokio::time::timeout(install_timeout, child.wait()).await {
        Ok(result) => result.context("等待 npm 安装进程失败")?,
        Err(_) => {
            stop_timed_out_process_tree(&mut child).await;
            bail!(
                "npm 安装超过 {} 秒，已停止本次安装",
                install_timeout.as_secs()
            )
        }
    };
    if !status.success() {
        bail!("npm 退出码 {:?}", status.code());
    }
    Ok(())
}

fn npm_install_arguments(package: &str, registry: &str) -> Result<Vec<String>> {
    if !matches!(package, "@openai/codex" | "@anthropic-ai/claude-code") {
        bail!("拒绝安装未知 npm 包: {package}");
    }
    if !matches!(registry, NPM_OFFICIAL_REGISTRY | NPM_MIRROR_REGISTRY) {
        bail!("拒绝使用未知 npm Registry");
    }

    let mut arguments = vec![
        "install".to_owned(),
        "-g".to_owned(),
        package.to_owned(),
        format!("--registry={registry}"),
        "--no-audit".to_owned(),
        "--no-fund".to_owned(),
    ];
    if package == "@anthropic-ai/claude-code" {
        // Claude Code 2.x 通过 postinstall 将平台原生程序放入 bin/claude.exe。
        // 新版 npm 默认可能阻止该脚本，导致表面安装成功、实际客户端不可运行。
        arguments.push("--allow-scripts=@anthropic-ai/claude-code".to_owned());
    }
    Ok(arguments)
}

fn npm_runtime_path(npm: &Path, node: &Path) -> Result<std::ffi::OsString> {
    let mut directories = Vec::new();
    if let Some(parent) = node.parent() {
        directories.push(parent.to_path_buf());
    }
    if let Some(parent) = npm.parent()
        && !directories.iter().any(|directory| directory == parent)
    {
        directories.push(parent.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(directories).context("无法为 npm 构造 Node.js 运行环境 PATH")
}

async fn stop_timed_out_process_tree(child: &mut Child) {
    if let Some(pid) = child.id() {
        #[cfg(windows)]
        {
            let mut taskkill = sanitized_command(Path::new(r"C:\Windows\System32\taskkill.exe"));
            let pid_argument = pid.to_string();
            taskkill.args(["/PID", pid_argument.as_str(), "/T", "/F"]);
            let _ = tokio::time::timeout(PROCESS_STOP_TIMEOUT, taskkill.status()).await;
        }
    }
    let _ = child.start_kill();
    let _ = tokio::time::timeout(PROCESS_STOP_TIMEOUT, child.wait()).await;
}

async fn install_node_lts() -> Result<()> {
    let client = http_client()?;
    let architecture = windows_architecture()?;
    let mut errors = Vec::new();

    for (label, index_url, base_url, index_timeout, download_timeout) in [
        (
            i18n::tr("Node.js 官方源", "official Node.js source"),
            NODE_OFFICIAL_INDEX,
            "https://nodejs.org/dist",
            SOURCE_PROBE_TIMEOUT,
            NPM_OFFICIAL_INSTALL_TIMEOUT,
        ),
        (
            i18n::tr("npmmirror Node 镜像", "npmmirror Node.js source"),
            NODE_MIRROR_INDEX,
            "https://npmmirror.com/mirrors/node",
            NODE_MIRROR_INDEX_TIMEOUT,
            NPM_MIRROR_INSTALL_TIMEOUT,
        ),
    ] {
        println!(
            "{} {label}…",
            i18n::tr(
                "正在查询最新 LTS 版本，来源",
                "Checking the latest LTS release from"
            )
        );
        let attempt = async {
            let releases = fetch_node_releases(&client, index_url, index_timeout).await?;
            let release = releases
                .into_iter()
                .find(|release| {
                    !release.lts.is_null()
                        && release.lts != serde_json::Value::Bool(false)
                        && (release.files.is_empty()
                            || release
                                .files
                                .iter()
                                .any(|file| file == &format!("win-{architecture}-msi")))
                })
                .context("版本目录中没有适用于当前架构的 Node.js LTS MSI")?;
            let version = release.version.trim_start_matches('v');
            if version.is_empty()
                || !version
                    .chars()
                    .all(|character| character.is_ascii_digit() || character == '.')
            {
                bail!("Node 版本号格式异常: {}", release.version);
            }
            let file_name = format!("node-v{version}-{architecture}.msi");
            let url = format!("{base_url}/v{version}/{file_name}");
            let destination = downloads_directory()?.join(file_name);
            let downloaded =
                download_to_with_timeout(&client, &url, &destination, download_timeout).await?;
            platform::verify_authenticode(&downloaded.path)?;
            println!(
                "{}",
                i18n::tr(
                    "Node.js 安装包签名校验通过，正在安装……",
                    "Node.js package signature verified. Installing…"
                )
            );
            run_msi_installer(&downloaded.path).await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;

        match attempt {
            Ok(()) if find_npm().is_some() => return Ok(()),
            Ok(()) => errors.push(format!("{label}: 安装完成后没有找到 npm.cmd")),
            Err(error) => {
                println!(
                    "{label} {}: {}",
                    i18n::tr(
                        "安装 Node.js 失败，将尝试下一来源",
                        "failed to install Node.js; trying the next source"
                    ),
                    i18n::runtime_error(&error)
                );
                errors.push(format!("{label}: {error:#}"));
            }
        }
    }

    bail!("Node.js 自动安装失败：{}", errors.join("；"))
}

async fn run_msi_installer(path: &Path) -> Result<()> {
    let mut command = sanitized_command(Path::new(r"C:\Windows\System32\msiexec.exe"));
    command
        .args(["/i"])
        .arg(path)
        .args(["/passive", "/norestart"]);
    let status = command
        .status()
        .await
        .with_context(|| format!("无法启动 Windows Installer: {}", path.display()))?;
    match status.code() {
        Some(0) | Some(3010) => Ok(()),
        code => bail!("Windows Installer 退出码 {code:?}"),
    }
}

const CLAUDE_MSIX_VALIDATION_SCRIPT: &str = r#"
$package = $env:ZNNZ_INSTALL_PACKAGE
if ([string]::IsNullOrWhiteSpace($package) -or -not (Test-Path -LiteralPath $package -PathType Leaf)) {
    throw 'Claude Desktop MSIX 文件不存在'
}
$signature = Get-AuthenticodeSignature -LiteralPath $package
if ([string]$signature.Status -ne 'Valid' -or $null -eq $signature.SignerCertificate) {
    throw ('Claude Desktop MSIX Authenticode 状态不是 Valid：' + [string]$signature.Status)
}
$subject = [string]$signature.SignerCertificate.Subject
if ($subject -notlike '*Anthropic, PBC*') {
    throw ('Claude Desktop MSIX 签名者不是 Anthropic：' + $subject)
}
Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [System.IO.Compression.ZipFile]::OpenRead($package)
try {
    $entry = $archive.GetEntry('AppxManifest.xml')
    if ($null -eq $entry) { throw 'Claude Desktop MSIX 缺少 AppxManifest.xml' }
    $reader = New-Object System.IO.StreamReader($entry.Open())
    try { [xml]$manifest = $reader.ReadToEnd() } finally { $reader.Dispose() }
    $identity = $manifest.SelectSingleNode("/*[local-name()='Package']/*[local-name()='Identity']")
    if ($null -eq $identity) { throw 'Claude Desktop MSIX 缺少 Identity' }
    $name = $identity.GetAttribute('Name')
    $publisher = $identity.GetAttribute('Publisher')
    if ($name -ne 'Claude') { throw ('Claude Desktop MSIX 包名异常：' + $name) }
    if ($publisher -notlike '*Anthropic, PBC*') {
        throw ('Claude Desktop MSIX Publisher 不是 Anthropic：' + $publisher)
    }
} finally {
    $archive.Dispose()
}
"#;

fn windows_powershell() -> PathBuf {
    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    system_root
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

async fn run_claude_powershell(path: &Path, script: &str, operation: &str) -> Result<()> {
    let powershell = windows_powershell();
    let mut command = sanitized_command(&powershell);
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ])
        .env("ZNNZ_INSTALL_PACKAGE", path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command
        .output()
        .await
        .with_context(|| format!("无法启动 Windows PowerShell：{}", powershell.display()))?;
    if !output.status.success() {
        let detail = powershell_output_detail(&output.stdout, &output.stderr);
        bail!(
            "{operation}失败（退出码 {:?}）{}",
            output.status.code(),
            if detail.is_empty() {
                String::new()
            } else {
                format!("：{detail}")
            }
        );
    }
    Ok(())
}

fn powershell_output_detail(stdout: &[u8], stderr: &[u8]) -> String {
    let text = format!(
        "{} {}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    let normalized = text
        .trim_start_matches('\u{feff}')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    normalized.chars().take(1000).collect()
}

async fn verify_claude_msix(path: &Path) -> Result<()> {
    platform::verify_authenticode(path)?;
    run_claude_powershell(
        path,
        CLAUDE_MSIX_VALIDATION_SCRIPT,
        "Claude Desktop 安装包身份校验",
    )
    .await
}

async fn run_msix_installer(path: &Path) -> Result<()> {
    let script = format!(
        "{}\nAdd-AppxPackage -Path $package -ForceApplicationShutdown",
        CLAUDE_MSIX_VALIDATION_SCRIPT
    );
    run_claude_powershell(path, &script, "Windows 应用部署服务安装").await
}

fn powershell_single_quoted_path(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .context("Claude Desktop 安装包路径不是有效 Unicode")?;
    if value.contains(['\0', '\r', '\n']) {
        bail!("Claude Desktop 安装包路径包含不支持的控制字符");
    }
    Ok(format!("'{}'", value.replace('\'', "''")))
}

fn encode_powershell_command(script: &str) -> String {
    let mut utf16le = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        utf16le.extend_from_slice(&unit.to_le_bytes());
    }
    STANDARD.encode(utf16le)
}

async fn run_msix_installer_elevated(path: &Path) -> Result<()> {
    let error_path = path.with_file_name("claude-desktop-install-error.txt");
    let _ = fs::remove_file(&error_path);
    let package = powershell_single_quoted_path(path)?;
    let error_file = powershell_single_quoted_path(&error_path)?;
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
try {{
    $env:ZNNZ_INSTALL_PACKAGE = {package}
    {validation}
    Add-AppxPackage -Path $package -ForceApplicationShutdown
}} catch {{
    Set-Content -LiteralPath {error_file} -Value ($_ | Out-String) -Encoding UTF8
    exit 1
}}
exit 0
"#,
        validation = CLAUDE_MSIX_VALIDATION_SCRIPT
    );
    let encoded = encode_powershell_command(&script);
    let exit_code = platform::run_elevated_powershell_encoded_command(
        &encoded,
        CLAUDE_ELEVATED_INSTALL_TIMEOUT_MS,
    )
    .await?;
    if exit_code != 0 {
        let detail = fs::read_to_string(&error_path)
            .ok()
            .map(|value| {
                value
                    .trim_start_matches('\u{feff}')
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("管理员安装进程退出码 {exit_code}"));
        let _ = fs::remove_file(&error_path);
        bail!("管理员权限安装 Claude Desktop 失败：{detail}");
    }
    let _ = fs::remove_file(&error_path);
    Ok(())
}

async fn install_codex_desktop() -> Result<()> {
    let processes_before_install = match platform::running_codex_desktop_process_ids() {
        Ok(processes) => Some(processes),
        Err(error) => {
            if i18n::language() == i18n::Language::ZhCn {
                println!("无法记录安装前的 Codex Desktop 进程，将跳过自动关闭保护：{error:#}");
            } else {
                println!(
                    "Unable to record Codex Desktop processes before installation; automatic close protection will be skipped: {}",
                    i18n::runtime_error(&error)
                );
            }
            None
        }
    };
    let client = http_client()?;
    let destination = downloads_directory()?.join("codex-desktop-installer.exe");
    let official = async {
        println!(
            "{}",
            i18n::tr(
                "正在下载 Microsoft 官方 Codex Desktop 安装引导程序 …",
                "Downloading the official Microsoft Codex Desktop installer …"
            )
        );
        let downloaded = download_to(&client, CODEX_DESKTOP_INSTALLER, &destination).await?;
        platform::verify_authenticode(&downloaded.path)?;
        println!(
            "{}",
            i18n::tr(
                "Microsoft 安装引导程序签名校验通过 …",
                "Microsoft installer signature verified …"
            )
        );
        println!(
            "{}",
            i18n::tr("正在安装 Codex Desktop …", "Installing Codex Desktop …")
        );
        platform::open_installer_package(&downloaded.path)?;
        if wait_until(platform::store_codex_installed, INSTALL_WAIT_TIMEOUT).await {
            Ok(())
        } else {
            bail!("等待 Codex Desktop 官方安装程序完成超时")
        }
    }
    .await;

    if official.is_ok() {
        if let Some(before_pids) = &processes_before_install {
            suppress_codex_desktop_auto_launch(before_pids).await;
        }
        return Ok(());
    }
    let official_error = official.unwrap_err();
    println!(
        "{}: {}",
        i18n::tr(
            "Microsoft 官方安装引导程序未完成",
            "The official Microsoft installer did not complete"
        ),
        i18n::runtime_error(&official_error)
    );
    println!(
        "{}",
        i18n::tr(
            "正在尝试使用 WinGet 的 Microsoft Store 源安装 Codex Desktop……",
            "Trying the Microsoft Store source through WinGet…"
        )
    );
    match install_codex_with_winget().await {
        Ok(()) => Ok(()),
        Err(winget_error) => bail!(
            "Codex Desktop 自动安装失败。官方安装引导程序：{official_error:#}；WinGet：{winget_error:#}"
        ),
    }
}

async fn suppress_codex_desktop_auto_launch(before_pids: &[u32]) {
    let started = Instant::now();
    let mut detected_new_process = false;
    while started.elapsed() < CODEX_AUTO_LAUNCH_DETECT_TIMEOUT {
        match platform::running_codex_desktop_process_ids() {
            Ok(current) => {
                detected_new_process = current.iter().any(|pid| !before_pids.contains(pid));
                if detected_new_process {
                    break;
                }
            }
            Err(error) => {
                if i18n::language() == i18n::Language::ZhCn {
                    println!("无法检查安装后自动启动的 Codex Desktop：{error:#}");
                } else {
                    println!(
                        "Unable to check whether Codex Desktop started automatically after installation: {}",
                        i18n::runtime_error(&error)
                    );
                }
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    if !detected_new_process {
        return;
    }

    let graceful_close_requested = match platform::request_new_codex_desktop_close(before_pids) {
        Ok(result) => result.window_count > 0,
        Err(error) => {
            if i18n::language() == i18n::Language::ZhCn {
                println!("无法正常关闭安装后自动启动的 Codex Desktop，将尝试清理新进程：{error:#}");
            } else {
                println!(
                    "Unable to close the automatically started Codex Desktop normally; attempting to clean up new processes: {}",
                    i18n::runtime_error(&error)
                );
            }
            false
        }
    };
    if graceful_close_requested {
        tokio::time::sleep(CODEX_AUTO_LAUNCH_CLOSE_GRACE).await;
    }

    let remaining = match platform::running_codex_desktop_process_ids() {
        Ok(current) => current
            .into_iter()
            .filter(|pid| !before_pids.contains(pid))
            .collect::<Vec<_>>(),
        Err(error) => {
            if i18n::language() == i18n::Language::ZhCn {
                println!("无法复检安装后自动启动的 Codex Desktop：{error:#}");
            } else {
                println!(
                    "Unable to recheck the automatically started Codex Desktop: {}",
                    i18n::runtime_error(&error)
                );
            }
            return;
        }
    };
    if !remaining.is_empty()
        && let Err(error) = platform::terminate_codex_desktop_process_tree(&remaining)
    {
        if i18n::language() == i18n::Language::ZhCn {
            println!("Codex Desktop 已安装，但无法关闭安装程序自动启动的客户端：{error:#}");
        } else {
            println!(
                "Codex Desktop was installed, but the client started by the installer could not be closed: {}",
                i18n::runtime_error(&error)
            );
        }
        return;
    }
    println!(
        "{}",
        i18n::tr(
            "已阻止 Codex Desktop 安装程序自动打开客户端。",
            "Prevented the Codex Desktop installer from opening the client automatically."
        )
    );
}

async fn install_codex_with_winget() -> Result<()> {
    let mut command = sanitized_command(Path::new("winget.exe"));
    command.args([
        "install",
        "--id",
        "9PLM9XGG6VKS",
        "--source",
        "msstore",
        "--accept-source-agreements",
        "--accept-package-agreements",
        "--silent",
        "--disable-interactivity",
    ]);
    let status = command
        .status()
        .await
        .context("当前系统没有可用的 WinGet")?;
    if !status.success() {
        bail!("WinGet 退出码 {:?}", status.code());
    }
    if !wait_until(platform::store_codex_installed, Duration::from_secs(90)).await {
        bail!("WinGet 返回成功，但没有检测到 Codex Desktop")
    }
    Ok(())
}

async fn install_claude_desktop() -> Result<()> {
    let client = http_client()?;
    let architecture = windows_architecture()?;
    let official_source = match architecture {
        "x64" => CLAUDE_DESKTOP_X64,
        "arm64" => CLAUDE_DESKTOP_ARM64,
        architecture => bail!("Claude Desktop 不支持当前 Windows 架构: {architecture}"),
    };
    let destination = downloads_directory()?.join("claude-desktop.msix");

    let downloaded = if destination.is_file() {
        match verify_claude_msix(&destination).await {
            Ok(()) => {
                println!(
                    "{}",
                    i18n::tr(
                        "检测到已下载且签名有效的 Claude Desktop 安装包，将直接复用。",
                        "A downloaded Claude Desktop package with a valid signature was found and will be reused."
                    )
                );
                Some(DownloadedFile {
                    path: destination.clone(),
                    final_url: "cached://claude-desktop.msix".to_owned(),
                    content_type: "application/msix".to_owned(),
                })
            }
            Err(error) => {
                println!(
                    "{}",
                    if i18n::language() == i18n::Language::ZhCn {
                        format!("已下载的 Claude Desktop 安装包签名无效，将重新下载：{error:#}")
                    } else {
                        format!(
                            "The downloaded Claude Desktop package has an invalid signature; downloading it again: {}",
                            i18n::runtime_error(&error)
                        )
                    }
                );
                fs::remove_file(&destination).with_context(|| {
                    format!("无法删除签名无效的安装包: {}", destination.display())
                })?;
                None
            }
        }
    } else {
        None
    };

    let downloaded = if let Some(downloaded) = downloaded {
        downloaded
    } else {
        let mut official_error = None;
        println!(
            "{}",
            i18n::tr(
                "正在从官方 claude.ai 下载 Claude Desktop 安装包 …",
                "Downloading the Claude Desktop package from official claude.ai …"
            )
        );
        let downloaded = match download_to_with_timeout(
            &client,
            official_source,
            &destination,
            CLAUDE_OFFICIAL_DOWNLOAD_TIMEOUT,
        )
        .await
        {
            Ok(downloaded) => match validate_claude_download(&downloaded) {
                Ok(()) => Some(downloaded),
                Err(error) => {
                    let _ = fs::remove_file(&downloaded.path);
                    official_error = Some(format!("{error:#}"));
                    None
                }
            },
            Err(error) => {
                official_error = Some(format!("{error:#}"));
                None
            }
        };

        if let Some(downloaded) = downloaded {
            downloaded
        } else {
            if architecture != "x64" {
                bail!(
                    "Claude Desktop 自动下载失败。官方源：{}；当前 Claude Desktop 备用下载源仅提供 x64 安装包，暂不支持 ARM64。原因：{}",
                    official_source,
                    official_error.as_deref().unwrap_or("未知错误")
                )
            }

            println!(
                "{}",
                if i18n::language() == i18n::Language::ZhCn {
                    format!(
                        "Claude 官方下载未完成，正在切换备用下载源：{}",
                        official_error.as_deref().unwrap_or("返回内容不可用")
                    )
                } else {
                    format!(
                        "The official Claude download did not complete; switching to the fallback source: {}",
                        official_error
                            .as_deref()
                            .map(i18n::runtime_error_text)
                            .unwrap_or_else(|| "The response was unusable.".to_owned())
                    )
                }
            );
            println!(
                "{}",
                i18n::tr(
                    "正在通过中继下载 Claude Desktop 安装包 …",
                    "Downloading the Claude Desktop package through the fallback source …"
                )
            );
            match download_to(&client, CLAUDE_DESKTOP_FALLBACK, &destination).await {
                Ok(downloaded) => match validate_claude_download(&downloaded) {
                    Ok(()) => downloaded,
                    Err(error) => {
                        let _ = fs::remove_file(&downloaded.path);
                        bail!(
                            "Claude Desktop 自动下载失败。官方源：{}；中继源：{}；官方源原因：{}；中继源原因：{error:#}",
                            official_source,
                            CLAUDE_DESKTOP_FALLBACK,
                            official_error.as_deref().unwrap_or("未知错误")
                        )
                    }
                },
                Err(relay_error) => bail!(
                    "Claude Desktop 自动下载失败。官方源：{}；中继源：{}；官方源原因：{}；中继源原因：{relay_error:#}",
                    official_source,
                    CLAUDE_DESKTOP_FALLBACK,
                    official_error.as_deref().unwrap_or("未知错误")
                ),
            }
        }
    };

    verify_claude_msix(&downloaded.path).await?;
    println!(
        "{}",
        i18n::tr(
            "Claude Desktop 安装包签名校验通过 …",
            "Claude Desktop package signature verified …"
        )
    );
    println!(
        "{}",
        i18n::tr("正在安装 Claude Desktop …", "Installing Claude Desktop …")
    );
    if let Err(error) = run_msix_installer(&downloaded.path).await {
        if i18n::language() == i18n::Language::ZhCn {
            println!("当前用户权限安装未完成：{error:#}");
        } else {
            println!(
                "Installation with current-user privileges did not complete: {}",
                i18n::runtime_error(&error)
            );
        }
        println!(
            "{}",
            i18n::tr(
                "Claude Desktop需要管理员权限，请允许 Windows 授权 …",
                "Claude Desktop requires administrator privileges. Approve the Windows prompt …"
            )
        );
        run_msix_installer_elevated(&downloaded.path).await?;
    }
    if !wait_until(platform::store_claude_installed, Duration::from_secs(30)).await {
        bail!(
            "Windows 应用部署服务已结束，但未检测到 Claude Desktop。安装包已保留：{}",
            downloaded.path.display()
        )
    }

    if let Err(error) = fs::remove_file(&downloaded.path) {
        if i18n::language() == i18n::Language::ZhCn {
            println!(
                "Claude Desktop 已安装，但无法删除安装包 {}：{error}",
                downloaded.path.display()
            );
        } else {
            println!(
                "Claude Desktop was installed, but the package could not be deleted {}: {error}",
                downloaded.path.display()
            );
        }
    }
    Ok(())
}

fn validate_claude_download(downloaded: &DownloadedFile) -> Result<()> {
    if downloaded.final_url.contains("app-unavailable-in-region") {
        bail!("下载地址返回区域不可用页面")
    }
    let content_type = downloaded.content_type.to_ascii_lowercase();
    if content_type.contains("text/html") {
        bail!("下载地址返回 HTML 页面而不是 MSIX 安装包")
    }
    if content_type.contains("application/json") {
        bail!("下载地址返回 JSON 错误而不是 MSIX 安装包")
    }
    Ok(())
}

async fn fetch_node_releases(
    client: &reqwest::Client,
    index_url: &str,
    limit: Duration,
) -> Result<Vec<NodeRelease>> {
    let request = async {
        client
            .get(index_url)
            .send()
            .await
            .with_context(|| format!("无法访问 {index_url}"))?
            .error_for_status()
            .with_context(|| format!("Node 版本目录返回错误: {index_url}"))?
            .json::<Vec<NodeRelease>>()
            .await
            .context("Node 版本目录不是有效 JSON")
    };
    tokio::time::timeout(limit, request)
        .await
        .with_context(|| format!("访问 Node 版本目录超过 {} 秒: {index_url}", limit.as_secs()))?
}

async fn download_to_with_timeout(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    limit: Duration,
) -> Result<DownloadedFile> {
    match tokio::time::timeout(limit, download_to(client, url, destination)).await {
        Ok(result) => result,
        Err(_) => {
            if let Ok(part) = part_path(destination) {
                let _ = fs::remove_file(part);
            }
            bail!("下载安装包超过 {} 秒: {url}", limit.as_secs())
        }
    }
}

fn http_client() -> Result<reqwest::Client> {
    network_proxy::configure_reqwest_builder(reqwest::Client::builder())?
        .user_agent(concat!("Agent-Switch/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(DOWNLOAD_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .context("无法初始化安装下载器")
}

async fn download_to(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
) -> Result<DownloadedFile> {
    let parent = destination.parent().context("下载目标缺少父目录")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("无法创建下载目录: {}", parent.display()))?;
    let part = part_path(destination)?;
    let _ = fs::remove_file(&part);

    let result = async {
        let response = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("无法下载 {url}"))?
            .error_for_status()
            .with_context(|| format!("下载地址返回错误: {url}"))?;
        let final_url = response.url().as_str().to_owned();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if let Some(length) = response.content_length()
            && length > MAX_DOWNLOAD_BYTES
        {
            bail!("安装包超过 1 GiB 安全上限（{length} 字节）")
        }

        let mut file = File::create(&part)
            .with_context(|| format!("无法创建临时下载文件: {}", part.display()))?;
        let mut stream = response.bytes_stream();
        let mut hasher = Sha256::new();
        let mut total = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("读取安装包下载流失败")?;
            total = total
                .checked_add(chunk.len() as u64)
                .context("安装包大小溢出")?;
            if total > MAX_DOWNLOAD_BYTES {
                bail!("安装包超过 1 GiB 安全上限")
            }
            hasher.update(&chunk);
            file.write_all(&chunk).context("写入安装包失败")?;
        }
        file.flush().context("刷新安装包文件失败")?;
        drop(file);
        if total == 0 {
            bail!("下载结果为空")
        }
        if destination.exists() {
            fs::remove_file(destination)
                .with_context(|| format!("无法替换旧安装包: {}", destination.display()))?;
        }
        fs::rename(&part, destination)
            .with_context(|| format!("无法完成安装包下载: {}", destination.display()))?;
        println!(
            "{}",
            if i18n::language() == i18n::Language::ZhCn {
                format!("下载完成：{} 字节，SHA256={:x}", total, hasher.finalize())
            } else {
                format!(
                    "Download complete: {} bytes, SHA256={:x}",
                    total,
                    hasher.finalize()
                )
            }
        );
        Ok(DownloadedFile {
            path: destination.to_path_buf(),
            final_url,
            content_type,
        })
    }
    .await;

    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

fn part_path(destination: &Path) -> Result<PathBuf> {
    let file_name = destination
        .file_name()
        .context("下载目标缺少文件名")?
        .to_string_lossy();
    Ok(destination.with_file_name(format!("{file_name}.part")))
}

fn downloads_directory() -> Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    Ok(local.join("Agent-Switch").join("downloads"))
}

fn windows_architecture() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("x64"),
        "aarch64" => Ok("arm64"),
        other => bail!("不支持的 Windows 架构: {other}"),
    }
}

fn find_node_for_npm(npm: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(parent) = npm.parent() {
        candidates.push(parent.join("node.exe"));
    }
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        candidates.push(PathBuf::from(program_files).join("nodejs").join("node.exe"));
    }
    if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
        candidates.push(
            PathBuf::from(program_files_x86)
                .join("nodejs")
                .join("node.exe"),
        );
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            candidates.push(directory.join("node.exe"));
        }
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn find_npm() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        candidates.push(PathBuf::from(program_files).join("nodejs").join("npm.cmd"));
    }
    if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
        candidates.push(
            PathBuf::from(program_files_x86)
                .join("nodejs")
                .join("npm.cmd"),
        );
    }
    if let Some(app_data) = std::env::var_os("APPDATA") {
        candidates.push(PathBuf::from(app_data).join("npm").join("npm.cmd"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            candidates.push(directory.join("npm.cmd"));
        }
    }
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn sanitized_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    for name in SENSITIVE_ENVIRONMENT_KEYS {
        command.env_remove(name);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

async fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration) -> bool {
    let started = Instant::now();
    loop {
        if predicate() {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_install_timeouts_match_network_fallback_policy() {
        assert_eq!(SOURCE_PROBE_TIMEOUT, Duration::from_secs(10));
        assert_eq!(NPM_OFFICIAL_INSTALL_TIMEOUT, Duration::from_secs(120));
        assert_eq!(NPM_MIRROR_INSTALL_TIMEOUT, Duration::from_secs(300));
    }

    #[test]
    fn terminal_package_names_are_fixed() {
        assert!(matches!(
            "@openai/codex",
            "@openai/codex" | "@anthropic-ai/claude-code"
        ));
        assert!(matches!(
            "@anthropic-ai/claude-code",
            "@openai/codex" | "@anthropic-ai/claude-code"
        ));
    }

    #[test]
    fn claude_code_install_explicitly_allows_its_postinstall() {
        let arguments =
            npm_install_arguments("@anthropic-ai/claude-code", NPM_MIRROR_REGISTRY).unwrap();
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--allow-scripts=@anthropic-ai/claude-code")
        );
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--registry=https://registry.npmmirror.com")
        );
    }

    #[test]
    fn codex_install_does_not_enable_unrelated_install_scripts() {
        let arguments = npm_install_arguments("@openai/codex", NPM_OFFICIAL_REGISTRY).unwrap();
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.starts_with("--allow-scripts="))
        );
    }

    #[test]
    fn npm_runtime_path_prioritizes_node_and_npm_directories() {
        let npm = Path::new(r"C:\Program Files\nodejs\npm.cmd");
        let node = Path::new(r"C:\Program Files\nodejs\node.exe");
        let runtime_path = npm_runtime_path(npm, node).unwrap();
        let directories = std::env::split_paths(&runtime_path).collect::<Vec<_>>();
        assert_eq!(directories.first().map(PathBuf::as_path), node.parent());
    }

    #[test]
    fn claude_desktop_download_policy_uses_fast_official_fallback() {
        assert_eq!(CLAUDE_OFFICIAL_DOWNLOAD_TIMEOUT, Duration::from_secs(30));
        assert_eq!(CLAUDE_ELEVATED_INSTALL_TIMEOUT_MS, 10 * 60 * 1000);
        assert_eq!(
            CLAUDE_DESKTOP_FALLBACK,
            "https://r.networkpe.top/znnz-claude-desktop"
        );
    }

    #[test]
    fn powershell_single_quoted_path_escapes_apostrophes() {
        let quoted =
            powershell_single_quoted_path(Path::new(r"C:\\Temp\\Claude's Desktop.msix")).unwrap();
        assert_eq!(quoted, r"'C:\\Temp\\Claude''s Desktop.msix'");
    }

    #[test]
    fn encoded_powershell_command_is_utf16le_base64() {
        let encoded = encode_powershell_command("输出 OK");
        let decoded = STANDARD.decode(encoded).unwrap();
        let expected = "输出 OK"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn claude_msix_validation_checks_signature_and_manifest_identity() {
        for expected in [
            "Get-AuthenticodeSignature",
            "Anthropic, PBC",
            "AppxManifest.xml",
            "$name -ne 'Claude'",
        ] {
            assert!(CLAUDE_MSIX_VALIDATION_SCRIPT.contains(expected));
        }
    }

    #[test]
    fn claude_download_rejects_error_pages() {
        let html = DownloadedFile {
            path: PathBuf::from("test.msix"),
            final_url: "https://claude.ai/app-unavailable-in-region".to_owned(),
            content_type: "text/html; charset=utf-8".to_owned(),
        };
        assert!(validate_claude_download(&html).is_err());

        let json = DownloadedFile {
            path: PathBuf::from("test.msix"),
            final_url: CLAUDE_DESKTOP_FALLBACK.to_owned(),
            content_type: "application/json".to_owned(),
        };
        assert!(validate_claude_download(&json).is_err());
    }

    #[test]
    fn partial_download_path_keeps_original_extension() {
        assert_eq!(
            part_path(Path::new(r"C:\Temp\Claude Desktop.msix")).unwrap(),
            PathBuf::from(r"C:\Temp\Claude Desktop.msix.part")
        );
    }

    #[test]
    fn supported_windows_architecture_is_known() {
        if cfg!(target_arch = "x86_64") || cfg!(target_arch = "aarch64") {
            assert!(windows_architecture().is_ok());
        }
    }
}
