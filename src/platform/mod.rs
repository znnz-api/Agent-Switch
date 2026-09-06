#[cfg(windows)]
mod windows_impl;

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(not(windows))]
mod portable {
    use anyhow::{Result, bail};

    pub struct NamedMutexGuard;

    pub fn try_acquire_named_mutex(_name: &str) -> Result<Option<NamedMutexGuard>> {
        Ok(Some(NamedMutexGuard))
    }

    pub fn activate_window_by_title(_title: &str) -> bool {
        false
    }

    pub fn process_matches_image(_pid: u32, _expected: &std::path::Path) -> bool {
        false
    }

    #[derive(Debug, Default)]
    pub struct CodexDesktopCloseResult {
        pub window_processes: Vec<(u32, String)>,
        pub process_tree: Vec<(u32, String)>,
        pub window_count: usize,
    }

    pub fn running_codex_processes() -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    pub fn running_codex_desktop_process_ids() -> Result<Vec<u32>> {
        Ok(Vec::new())
    }

    pub fn running_codex_companion_processes() -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    pub fn running_claude_desktop_processes() -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    pub fn running_claude_desktop_process_ids() -> Result<Vec<u32>> {
        Ok(Vec::new())
    }

    pub fn request_codex_desktop_close() -> Result<CodexDesktopCloseResult> {
        Ok(CodexDesktopCloseResult::default())
    }

    pub fn request_new_codex_desktop_close(
        _before_pids: &[u32],
    ) -> Result<CodexDesktopCloseResult> {
        Ok(CodexDesktopCloseResult::default())
    }

    pub fn running_process_ids(_pids: &[u32]) -> Result<Vec<u32>> {
        Ok(Vec::new())
    }

    pub fn running_any_process_ids(_pids: &[u32]) -> Result<Vec<u32>> {
        Ok(Vec::new())
    }

    pub fn terminate_codex_desktop_process_tree(_pids: &[u32]) -> Result<Vec<u32>> {
        Ok(Vec::new())
    }

    pub fn attach_parent_console() {}

    pub fn hide_current_console() {}

    pub fn user_prefers_chinese() -> bool {
        std::env::var("LANG")
            .ok()
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("zh"))
    }

    pub fn open_external_url(_url: &str) -> Result<()> {
        bail!("当前平台尚未实现打开外部网址")
    }

    pub fn install_gui_panic_hook() {}

    pub fn report_gui_startup_failure(message: &str) {
        eprintln!("{message}");
    }

    pub fn verify_authenticode(_path: &std::path::Path) -> Result<()> {
        bail!("当前平台不支持 Windows Authenticode 校验")
    }

    pub fn open_installer_package(_path: &std::path::Path) -> Result<()> {
        bail!("当前平台不支持打开 Windows 安装包")
    }

    pub fn terminal_client_installed(_command: &str) -> bool {
        false
    }

    pub fn store_codex_installed() -> bool {
        false
    }

    pub fn store_claude_installed() -> bool {
        false
    }

    pub fn launch_terminal_client_with_environment(
        _title: &str,
        _command: &str,
        _environment: &[(&str, &str)],
    ) -> Result<u32> {
        bail!("当前平台尚未实现终端客户端启动")
    }

    pub fn launch_terminal_client_with_environment_in_directory(
        _title: &str,
        _command: &str,
        _environment: &[(&str, &str)],
        _current_directory: &std::path::Path,
    ) -> Result<u32> {
        bail!("当前平台尚未实现终端客户端启动")
    }

    pub fn read_codex_bundled_catalog(_codex_home: &std::path::Path) -> Result<String> {
        bail!("当前平台尚未实现 Codex 自带模型目录读取")
    }

    pub fn set_user_environment(_values: &[(&str, &str)]) -> Result<()> {
        Ok(())
    }

    pub async fn activate_store_codex(_arguments: &str) -> Result<u32> {
        bail!("当前平台不支持 Windows Store Codex 激活")
    }

    pub async fn activate_store_claude() -> Result<u32> {
        bail!("当前平台不支持 Windows Store Claude 激活")
    }
}

#[cfg(not(windows))]
pub use portable::*;
