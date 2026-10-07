use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsString, c_void};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_CANCELLED, FILETIME, GetLastError, HANDLE, HWND,
    LPARAM, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Globalization::GetUserDefaultUILanguage;
use windows::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0,
    WINTRUST_DATA_PROVIDER_FLAGS, WINTRUST_FILE_INFO, WTD_CHOICE_FILE, WTD_REVOKE_NONE,
    WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE, WTD_UICONTEXT_EXECUTE,
    WinVerifyTrust,
};
use windows::Win32::Storage::Packaging::Appx::GetPackagesByPackageFamily;
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, FreeConsole};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, CreateMutexW, CreateProcessW,
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_INFORMATION, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    ACTIVATEOPTIONS, ApplicationActivationManager, IApplicationActivationManager,
    SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetWindowThreadProcessId, HWND_BROADCAST, IsWindowVisible,
    MB_ICONERROR, MB_OK, MessageBoxW, PostMessageW, SMTO_ABORTIFHUNG, SW_HIDE, SW_RESTORE,
    SW_SHOWNORMAL, SendMessageTimeoutW, SetForegroundWindow, ShowWindow, WM_CLOSE,
    WM_SETTINGCHANGE,
};
use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};

const CREATE_NO_WINDOW_FLAG: u32 = 0x0800_0000;
const WTD_CACHE_ONLY_URL_RETRIEVAL_FLAG: WINTRUST_DATA_PROVIDER_FLAGS =
    WINTRUST_DATA_PROVIDER_FLAGS(0x0000_1000);

#[cfg_attr(test, allow(dead_code))]
pub fn user_prefers_chinese() -> bool {
    // LANGID lower 10 bits are the primary language ID. Chinese is 0x04,
    // covering Simplified and Traditional Windows UI language variants.
    unsafe { GetUserDefaultUILanguage() & 0x03ff == 0x04 }
}

fn gui_startup_error_log_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Agent-Switch")
        .join("logs")
        .join("gui-startup-error.log")
}

pub fn report_gui_startup_failure(message: &str) {
    let log_path = gui_startup_error_log_path();
    if let Some(parent) = log_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default();
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
        let _ = writeln!(
            file,
            "time={timestamp} version={}\n{message}\n",
            env!("CARGO_PKG_VERSION")
        );
    }

    let text = format!(
        "Agent-Switch failed to start the GUI.\r\n\r\n{message}\r\n\r\nLog: {}",
        log_path.display()
    );
    let title_text = crate::i18n::tr("Agent-Switch 启动失败", "Agent-Switch startup failed");
    let title = wide_null(std::ffi::OsStr::new(title_text));
    let body = wide_null(text.as_ref());
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(body.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

pub fn install_gui_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("未知异常");
        let location = info
            .location()
            .map(|value| format!("{}:{}:{}", value.file(), value.line(), value.column()))
            .unwrap_or_else(|| "未知位置".to_owned());
        report_gui_startup_failure(&format!(
            "GUI startup failure: {payload}\nLocation: {location}"
        ));
    }));
}

pub struct NamedMutexGuard(HANDLE);

impl Drop for NamedMutexGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn try_acquire_named_mutex(name: &str) -> Result<Option<NamedMutexGuard>> {
    if name.is_empty() || name.contains('\0') {
        bail!("命名 Mutex 名称无效");
    }
    let wide = wide_null(name.as_ref());
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }
        .with_context(|| format!("无法创建命名 Mutex: {name}"))?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Ok(None);
    }
    Ok(Some(NamedMutexGuard(handle)))
}

pub fn activate_window_by_title(title: &str) -> bool {
    if title.is_empty() || title.contains('\0') {
        return false;
    }
    let wide = wide_null(title.as_ref());
    let Ok(hwnd) = (unsafe { FindWindowW(PCWSTR::null(), PCWSTR(wide.as_ptr())) }) else {
        return false;
    };
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd),
            windows::Win32::UI::WindowsAndMessaging::WM_APP + 74,
            WPARAM(0),
            LPARAM(0),
        );
        SetForegroundWindow(hwnd).as_bool()
    }
}

pub fn process_matches_image(pid: u32, expected: &Path) -> bool {
    let Ok(actual) = process_image_path(pid) else {
        return false;
    };
    normalize_image_path(&actual) == normalize_image_path(&expected.to_string_lossy())
}

fn normalize_image_path(value: &str) -> String {
    value
        .trim()
        .trim_start_matches(r"\\?\")
        .replace('/', "\\")
        .to_ascii_lowercase()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessEntry {
    pid: u32,
    parent_pid: u32,
    name: String,
}

#[derive(Debug, Default)]
pub struct CodexDesktopCloseResult {
    pub window_processes: Vec<(u32, String)>,
    pub process_tree: Vec<(u32, String)>,
    pub window_count: usize,
}

fn process_entries() -> Result<Vec<ProcessEntry>> {
    let mut entries = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).context("无法枚举进程")?;
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|ch| *ch == 0)
                    .unwrap_or(entry.szExeFile.len());
                entries.push(ProcessEntry {
                    pid: entry.th32ProcessID,
                    parent_pid: entry.th32ParentProcessID,
                    name: String::from_utf16_lossy(&entry.szExeFile[..end]),
                });
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    Ok(entries)
}

fn normalized_process_name(name: &str) -> String {
    let normalized = name.trim().to_ascii_lowercase();
    normalized
        .strip_suffix(".exe")
        .unwrap_or(&normalized)
        .to_owned()
}

fn is_codex_companion_name(name: &str) -> bool {
    matches!(
        normalized_process_name(name).as_str(),
        "codex++" | "codexplusplus" | "codex-plus-plus" | "codex-plus-plus-manager"
    )
}

fn is_codex_related(name: &str) -> bool {
    is_official_desktop_name(name) || is_codex_companion_name(name)
}

fn is_official_desktop_name(name: &str) -> bool {
    matches!(normalized_process_name(name).as_str(), "codex" | "chatgpt")
}

pub fn running_codex_processes() -> Result<Vec<String>> {
    matching_processes(is_codex_related)
}

fn is_official_codex_desktop_path(path: &str) -> bool {
    let normalized = path.replace('/', "\\").to_ascii_lowercase();
    let official_package = normalized.contains("\\windowsapps\\openai.codex_")
        || normalized.contains("\\windowsapps\\openai.codexbeta_")
        || normalized.contains("\\windowsapps\\openai.chatgpt-desktop_");
    official_package
        && (normalized.ends_with("\\codex.exe") || normalized.ends_with("\\chatgpt.exe"))
}

pub fn running_codex_desktop_process_ids() -> Result<Vec<u32>> {
    let mut matches = process_entries()?
        .into_iter()
        .filter(|entry| is_official_desktop_name(&entry.name))
        .filter_map(|entry| {
            process_image_path(entry.pid)
                .ok()
                .filter(|path| is_official_codex_desktop_path(path))
                .map(|_| entry.pid)
        })
        .collect::<Vec<_>>();
    matches.sort_unstable();
    matches.dedup();
    Ok(matches)
}

pub fn running_codex_companion_processes() -> Result<Vec<String>> {
    matching_processes(is_codex_companion_name)
}

/// Read the command line with query-only access, without injecting into the client.
fn process_command_line(pid: u32) -> Option<String> {
    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *const u16,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQueryInformationProcess(
            process: HANDLE,
            class: u32,
            buffer: *mut std::ffi::c_void,
            size: u32,
            returned: *mut u32,
        ) -> i32;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()? };
    // u64 storage provides alignment for UNICODE_STRING on both Windows targets.
    let mut storage = vec![0u64; 16384];
    let mut returned = 0;
    let status = unsafe {
        NtQueryInformationProcess(
            handle,
            60,
            storage.as_mut_ptr().cast(),
            (storage.len() * 8) as u32,
            &mut returned,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    if status < 0 {
        return None;
    }
    let value = unsafe { &*storage.as_ptr().cast::<UnicodeString>() };
    let start = storage.as_ptr() as usize;
    let pointer = value.buffer as usize;
    if pointer < start
        || pointer.checked_add(value.length as usize)? > start + storage.len() * 8
        || value.length % 2 != 0
    {
        return None;
    }
    Some(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(value.buffer, value.length as usize / 2)
    }))
}

pub fn running_client_processes() -> Result<Vec<(crate::gui_worker::ClientTarget, u32)>> {
    let entries = process_entries()?;
    let mut result = Vec::new();
    for entry in &entries {
        let name = normalized_process_name(&entry.name);
        if !matches!(name.as_str(), "codex" | "chatgpt" | "claude" | "node") {
            continue;
        }
        let Ok(path) = process_image_path(entry.pid) else {
            continue;
        };
        let command = process_command_line(entry.pid).unwrap_or_default();
        let target = classify_client_process(&name, &path, &command);
        if let Some(target) = target {
            result.push((target, entry.pid));
        }
    }
    Ok(result)
}

fn classify_client_process(
    name: &str,
    path: &str,
    command: &str,
) -> Option<crate::gui_worker::ClientTarget> {
    use crate::gui_worker::ClientTarget;
    if is_official_codex_desktop_path(path) {
        Some(ClientTarget::CodexDesktop)
    } else if is_official_claude_desktop_path(path) {
        Some(ClientTarget::ClaudeDesktop)
    } else {
        let command = command.replace('\\', "/").to_ascii_lowercase();
        // Desktop helpers and IDE app-server processes are not terminal sessions.
        if path.to_ascii_lowercase().contains("\\windowsapps\\")
            || command.contains("app-server")
            || command.contains("--type=")
        {
            None
        } else if name == "codex" || (name == "node" && command.contains("@openai/codex/")) {
            Some(ClientTarget::CodexCli)
        } else if name == "claude"
            || (name == "node" && command.contains("@anthropic-ai/claude-code/"))
        {
            Some(ClientTarget::ClaudeCode)
        } else {
            None
        }
    }
}

pub fn process_image_path(pid: u32) -> Result<String> {
    let handle = unsafe {
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .with_context(|| format!("无法打开 PID {pid} 读取程序路径"))?
    };
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    result.with_context(|| format!("Unable to read process image path for PID {pid}"))?;
    buffer.truncate(length as usize);
    Ok(String::from_utf16_lossy(&buffer))
}

fn is_official_claude_desktop_path(path: &str) -> bool {
    let normalized = path.replace('/', "\\").to_ascii_lowercase();
    normalized.contains("\\windowsapps\\claude_") && normalized.ends_with("\\app\\claude.exe")
}

pub fn running_claude_desktop_process_ids() -> Result<Vec<u32>> {
    let mut matches = process_entries()?
        .into_iter()
        .filter(|entry| normalized_process_name(&entry.name) == "claude")
        .filter_map(|entry| {
            process_image_path(entry.pid)
                .ok()
                .filter(|path| is_official_claude_desktop_path(path))
                .map(|_| entry.pid)
        })
        .collect::<Vec<_>>();
    matches.sort_unstable();
    matches.dedup();
    Ok(matches)
}

pub fn running_claude_desktop_processes() -> Result<Vec<String>> {
    let mut matches = Vec::new();
    for entry in process_entries()? {
        if normalized_process_name(&entry.name) != "claude" {
            continue;
        }
        let Ok(image) = process_image_path(entry.pid) else {
            continue;
        };
        if is_official_claude_desktop_path(&image) {
            matches.push(format!("{} (PID {}, {})", entry.name, entry.pid, image));
        }
    }
    matches.sort();
    matches.dedup();
    Ok(matches)
}

fn matching_processes(predicate: fn(&str) -> bool) -> Result<Vec<String>> {
    let mut matches = process_entries()?
        .into_iter()
        .filter(|entry| predicate(&entry.name))
        .map(|entry| format!("{} (PID {})", entry.name, entry.pid))
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    Ok(matches)
}

struct CloseWindowsContext {
    candidates: HashMap<u32, String>,
    matched: HashMap<u32, String>,
    windows: Vec<HWND>,
}

unsafe extern "system" fn collect_codex_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let context = unsafe { &mut *(lparam.0 as *mut CloseWindowsContext) };
    // Electron's hidden tray/message windows must remain alive until app exit.
    // Closing them directly can strand the process with an unresponsive tray.
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return BOOL(1);
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let Some(name) = context.candidates.get(&pid).cloned() else {
        return BOOL(1);
    };
    context.matched.entry(pid).or_insert(name);
    context.windows.push(hwnd);
    BOOL(1)
}

fn codex_desktop_process_tree_from_entries(
    entries: &[ProcessEntry],
    seed_pids: &[u32],
) -> Vec<ProcessEntry> {
    let by_pid = entries
        .iter()
        .map(|entry| (entry.pid, entry))
        .collect::<HashMap<_, _>>();
    let mut roots = HashSet::new();

    for seed_pid in seed_pids.iter().copied() {
        let Some(seed) = by_pid.get(&seed_pid) else {
            continue;
        };
        if !is_official_desktop_name(&seed.name) {
            continue;
        }

        let mut root_pid = seed_pid;
        let mut seen = HashSet::new();
        seen.insert(root_pid);
        while let Some(current) = by_pid.get(&root_pid) {
            let Some(parent) = by_pid.get(&current.parent_pid) else {
                break;
            };
            if !is_official_desktop_name(&parent.name) || !seen.insert(parent.pid) {
                break;
            }
            root_pid = parent.pid;
        }
        roots.insert(root_pid);
    }

    let mut children = HashMap::<u32, Vec<u32>>::new();
    for entry in entries {
        if is_official_desktop_name(&entry.name) {
            children
                .entry(entry.parent_pid)
                .or_default()
                .push(entry.pid);
        }
    }
    for values in children.values_mut() {
        values.sort_unstable();
        values.dedup();
    }

    fn visit(
        pid: u32,
        by_pid: &HashMap<u32, &ProcessEntry>,
        children: &HashMap<u32, Vec<u32>>,
        visiting: &mut HashSet<u32>,
        selected: &mut HashSet<u32>,
        output: &mut Vec<ProcessEntry>,
    ) {
        if selected.contains(&pid) || !visiting.insert(pid) {
            return;
        }
        if let Some(child_pids) = children.get(&pid) {
            for child_pid in child_pids {
                visit(*child_pid, by_pid, children, visiting, selected, output);
            }
        }
        visiting.remove(&pid);
        if let Some(entry) = by_pid.get(&pid)
            && is_official_desktop_name(&entry.name)
            && selected.insert(pid)
        {
            output.push((*entry).clone());
        }
    }

    let mut roots = roots.into_iter().collect::<Vec<_>>();
    roots.sort_unstable();
    let mut output = Vec::new();
    let mut selected = HashSet::new();
    let mut visiting = HashSet::new();
    for root in roots {
        visit(
            root,
            &by_pid,
            &children,
            &mut visiting,
            &mut selected,
            &mut output,
        );
    }
    output
}

/// Retain kernel handles before closing windows: a recycled PID can never cause
/// cleanup to terminate an unrelated process that started during the wait.
struct RestartProcess {
    pid: u32,
    handle: HANDLE,
}

impl Drop for RestartProcess {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

impl RestartProcess {
    fn open(pid: u32) -> Result<Self> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                false,
                pid,
            )
        }
        .with_context(|| format!("无法安全管理客户端进程 PID {pid}"))?;
        Ok(Self { pid, handle })
    }

    fn image(&self) -> Result<String> {
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                self.handle,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
        }
        .context("无法验证客户端进程路径")?;
        Ok(String::from_utf16_lossy(&buffer[..length as usize]))
    }

    fn running(&self) -> Result<bool> {
        match unsafe { WaitForSingleObject(self.handle, 0) } {
            WAIT_TIMEOUT => Ok(true),
            WAIT_OBJECT_0 => Ok(false),
            _ => bail!("无法检查客户端进程 PID {}", self.pid),
        }
    }

    fn created_at(&self) -> Result<u64> {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe {
            GetProcessTimes(
                self.handle,
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            )
        }
        .context("无法验证终端进程创建时间")?;
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}

// Windows process handles can be used from any thread. Keeping the original
// handle prevents PID reuse from redirecting a later terminal close operation.
unsafe impl Send for RestartProcess {}
unsafe impl Sync for RestartProcess {}

pub struct TerminalRestartSession {
    root: RestartProcess,
    target: crate::gui_worker::ClientTarget,
}

fn terminal_descendants(entries: &[ProcessEntry], root_pid: u32) -> Vec<&ProcessEntry> {
    let mut descendants = Vec::new();
    let mut parents = vec![root_pid];
    while !parents.is_empty() {
        let children: Vec<_> = entries
            .iter()
            .filter(|entry| {
                parents.contains(&entry.parent_pid)
                    && entry.pid != root_pid
                    && !descendants
                        .iter()
                        .any(|old: &&ProcessEntry| old.pid == entry.pid)
            })
            .collect();
        parents = children.iter().map(|entry| entry.pid).collect();
        descendants.extend(children);
    }
    descendants
}

fn terminal_restart_process_matches(
    target: crate::gui_worker::ClientTarget,
    name: &str,
    image: &str,
    command: &str,
) -> bool {
    let console_host = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let normalized_name = normalized_process_name(name);
    if image.eq_ignore_ascii_case(&format!(r"{console_host}\System32\conhost.exe"))
        || image.eq_ignore_ascii_case(&format!(r"{console_host}\System32\OpenConsole.exe"))
    {
        return true;
    }
    if classify_client_process(&normalized_name, image, command) == Some(target) {
        return true;
    }
    // npm/pnpm Windows shims add one cmd.exe layer before the actual Node
    // client. It is safe to include only a shell whose command line names the
    // recorded client; an arbitrary command typed in the terminal still falls
    // through to the manual-restart path.
    if normalized_name == "cmd" || normalized_name == "npm" || normalized_name == "pnpm" {
        let command = command.replace('\\', "/").to_ascii_lowercase();
        let client_word = match target {
            crate::gui_worker::ClientTarget::CodexCli => "codex",
            crate::gui_worker::ClientTarget::ClaudeCode => "claude",
            _ => "",
        };
        return !client_word.is_empty()
            && command.contains(client_word)
            && !command.contains("powershell")
            && !command.contains(" -command ");
    }
    false
}

fn is_terminal_console_host(image: &str) -> bool {
    let console_host = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    image.eq_ignore_ascii_case(&format!(r"{console_host}\System32\conhost.exe"))
        || image.eq_ignore_ascii_case(&format!(r"{console_host}\System32\OpenConsole.exe"))
}

impl TerminalRestartSession {
    fn descendants<'a>(&self, entries: &'a [ProcessEntry]) -> Result<Vec<&'a ProcessEntry>> {
        let mut created = HashMap::from([(self.root.pid, self.root.created_at()?)]);
        let mut verified = Vec::new();
        // Windows snapshots retain parent PIDs after the parent has exited.
        // A recycled parent PID must not make an unrelated older process ours.
        for entry in terminal_descendants(entries, self.root.pid) {
            let Some(parent_created) = created.get(&entry.parent_pid).copied() else {
                continue;
            };
            let process = match RestartProcess::open(entry.pid) {
                Ok(process) => process,
                Err(_) if running_any_process_ids(&[entry.pid])?.is_empty() => continue,
                Err(error) => return Err(error),
            };
            if !process.running()? {
                continue;
            }
            let child_created = process.created_at()?;
            if child_created >= parent_created {
                created.insert(entry.pid, child_created);
                verified.push(entry);
            }
        }
        Ok(verified)
    }

    pub fn record(pid: u32, target: crate::gui_worker::ClientTarget) -> Result<Self> {
        if !matches!(
            target,
            crate::gui_worker::ClientTarget::CodexCli | crate::gui_worker::ClientTarget::ClaudeCode
        ) {
            bail!("拒绝记录非终端客户端");
        }
        let root = RestartProcess::open(pid)?;
        if !root.image()?.eq_ignore_ascii_case(
            &std::env::var("COMSPEC").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into()),
        ) {
            bail!("终端会话身份无效");
        }
        Ok(Self { root, target })
    }

    pub fn close(&self) -> Result<()> {
        if !self.root.running()? {
            return Ok(());
        }
        for _ in 0..3 {
            let entries = process_entries()?;
            let descendants = self.descendants(&entries)?;
            let mut processes = Vec::new();
            // Acquire and validate all handles before closing anything. Active user
            // commands and other client sessions require a manual restart.
            for entry in descendants.iter().rev() {
                let process = match RestartProcess::open(entry.pid) {
                    Ok(process) => process,
                    Err(error) if running_any_process_ids(&[entry.pid])?.is_empty() => {
                        let _ = error;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if !process.running()? {
                    continue;
                }
                let image = process.image()?;
                let command = process_command_line(entry.pid).unwrap_or_default();
                if !terminal_restart_process_matches(self.target, &entry.name, &image, &command) {
                    bail!(
                        "{} ({image})",
                        crate::i18n::tr(
                            "终端中有其他子程序运行，请手动重启",
                            "Other programs are running in the terminal; restart it manually"
                        )
                    );
                }
                if !is_terminal_console_host(&image) {
                    processes.push(process);
                }
            }
            // A tool started while handles were being checked also requires a manual restart.
            if self
                .descendants(&process_entries()?)?
                .iter()
                .filter(|entry| {
                    process_image_path(entry.pid)
                        .map(|image| !is_terminal_console_host(&image))
                        .unwrap_or(true)
                })
                .any(|entry| !processes.iter().any(|process| process.pid == entry.pid))
            {
                // Windows may still be creating its console host. Revalidate every
                // new process before retrying; an unknown child still aborts above.
                continue;
            }
            for process in processes.iter().chain(std::iter::once(&self.root)) {
                if process.running()? {
                    unsafe { TerminateProcess(process.handle, 0) }
                        .context("无法结束已记录的终端客户端")?;
                    if unsafe { WaitForSingleObject(process.handle, 5000) } != WAIT_OBJECT_0 {
                        bail!("终端客户端未完整退出");
                    }
                }
            }
            return Ok(());
        }
        bail!(
            "{}",
            crate::i18n::tr(
                "终端进程已变化，请手动重启",
                "Terminal processes changed; restart it manually"
            )
        )
    }

    pub fn verify_client_sessions(&self) -> Result<()> {
        let entries = process_entries()?;
        let descendants = if self.root.running()? {
            self.descendants(&entries)?
        } else {
            Vec::new()
        };
        if running_client_processes()?.iter().any(|(target, pid)| {
            *target == self.target && !descendants.iter().any(|entry| entry.pid == *pid)
        }) {
            bail!(
                "{}",
                crate::i18n::tr(
                    "仍有其他终端会话运行",
                    "Other terminal sessions are still running"
                )
            );
        }
        Ok(())
    }
}

pub struct DesktopRestartSession {
    target: crate::gui_worker::ClientTarget,
    processes: Vec<RestartProcess>,
}

fn desktop_image_matches(target: crate::gui_worker::ClientTarget, path: &str) -> bool {
    use crate::gui_worker::ClientTarget;
    match target {
        ClientTarget::CodexDesktop => is_official_codex_desktop_path(path),
        ClientTarget::ClaudeDesktop => is_official_claude_desktop_path(path),
        _ => false,
    }
}

impl DesktopRestartSession {
    pub fn begin(target: crate::gui_worker::ClientTarget) -> Result<Self> {
        use crate::gui_worker::ClientTarget;
        let pids = match target {
            ClientTarget::CodexDesktop => running_codex_desktop_process_ids()?,
            ClientTarget::ClaudeDesktop => running_claude_desktop_process_ids()?,
            _ => bail!("终端会话不能通过桌面重启流程关闭"),
        };
        let mut processes = Vec::new();
        for pid in pids {
            let process = match RestartProcess::open(pid) {
                Ok(process) => process,
                Err(error) => {
                    if running_any_process_ids(&[pid])?.is_empty() {
                        continue;
                    }
                    return Err(error);
                }
            };
            if !process.running()? {
                continue;
            }
            if !desktop_image_matches(target, &process.image()?) {
                bail!("客户端进程身份已变化，取消自动重启");
            }
            processes.push(process);
        }
        let session = Self { target, processes };
        // Acquire every handle before sending any close message. If access is
        // denied, leave the UI and tray untouched and request a manual restart.
        let context = session.visible_windows()?;
        for hwnd in context.windows {
            unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }
                .context("无法发送客户端窗口关闭请求")?;
        }
        Ok(session)
    }

    fn visible_windows(&self) -> Result<CloseWindowsContext> {
        let mut context = CloseWindowsContext {
            candidates: self
                .processes
                .iter()
                .filter_map(|process| match process.running() {
                    Ok(false) => None,
                    _ => Some((process.pid, self.target.title().to_owned())),
                })
                .collect(),
            matched: HashMap::new(),
            windows: Vec::new(),
        };
        unsafe {
            EnumWindows(
                Some(collect_codex_window),
                LPARAM((&mut context as *mut CloseWindowsContext) as isize),
            )
        }
        .context("无法检查客户端窗口是否已关闭")?;
        Ok(context)
    }

    pub fn has_visible_windows(&self) -> Result<bool> {
        Ok(!self.visible_windows()?.windows.is_empty())
    }

    pub fn running(&self) -> Result<bool> {
        for process in &self.processes {
            if process.running()? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn terminate_background(&self) -> Result<()> {
        if self.has_visible_windows()? {
            bail!("客户端窗口仍打开，已保留进程，请手动重启");
        }
        for process in &self.processes {
            if process.running()? {
                // Recheck before every termination, including close-cancel and
                // confirmation dialogs that appeared during the grace period.
                if self.has_visible_windows()? {
                    bail!("客户端重新显示窗口，已停止后台清理");
                }
                if let Err(error) = unsafe { TerminateProcess(process.handle, 0) }
                    && process.running()?
                {
                    return Err(error)
                        .with_context(|| format!("无法清理客户端后台 PID {}", process.pid));
                }
            }
        }
        Ok(())
    }
}

/// 先记录官方 Codex/ChatGPT Desktop 的窗口和同一官方进程树，再发送 WM_CLOSE。
/// 进程树记录发生在关闭请求之前，避免 Electron 窗口进程先退出后丢失父子关系。
pub fn request_codex_desktop_close() -> Result<CodexDesktopCloseResult> {
    let entries = process_entries()?;
    let candidates = entries
        .iter()
        .filter(|entry| {
            is_official_desktop_name(&entry.name)
                && process_image_path(entry.pid)
                    .is_ok_and(|path| is_official_codex_desktop_path(&path))
        })
        .map(|entry| (entry.pid, entry.name.clone()))
        .collect::<HashMap<_, _>>();
    if candidates.is_empty() {
        return Ok(CodexDesktopCloseResult::default());
    }

    let mut context = CloseWindowsContext {
        candidates,
        matched: HashMap::new(),
        windows: Vec::new(),
    };
    unsafe {
        EnumWindows(
            Some(collect_codex_window),
            LPARAM((&mut context as *mut CloseWindowsContext) as isize),
        )
        .context("无法枚举 Codex Desktop 窗口")?;
    }

    let mut window_processes = context.matched.into_iter().collect::<Vec<_>>();
    window_processes.sort_by_key(|(pid, _)| *pid);
    if window_processes.is_empty() {
        return Ok(CodexDesktopCloseResult::default());
    }

    let seed_pids = window_processes
        .iter()
        .map(|(pid, _)| *pid)
        .collect::<Vec<_>>();
    let process_tree = codex_desktop_process_tree_from_entries(&entries, &seed_pids)
        .into_iter()
        .map(|entry| (entry.pid, entry.name))
        .collect::<Vec<_>>();

    let mut sent = 0usize;
    let mut errors = Vec::new();
    for hwnd in context.windows {
        match unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) } {
            Ok(()) => sent += 1,
            Err(error) => errors.push(error.to_string()),
        }
    }
    if sent == 0 {
        bail!(
            "已找到 Codex Desktop 窗口，但无法发送正常关闭请求: {}",
            errors.join("；")
        );
    }

    Ok(CodexDesktopCloseResult {
        window_processes,
        process_tree,
        window_count: sent,
    })
}

/// 仅关闭安装前不存在的官方 Codex/ChatGPT Desktop 进程。
/// 用于阻止 Microsoft 安装引导程序在安装完成后自动打开客户端，不影响安装前已运行的实例。
pub fn request_new_codex_desktop_close(before_pids: &[u32]) -> Result<CodexDesktopCloseResult> {
    let before = before_pids.iter().copied().collect::<HashSet<_>>();
    let verified_new_pids = running_codex_desktop_process_ids()?
        .into_iter()
        .filter(|pid| !before.contains(pid))
        .collect::<HashSet<_>>();
    let entries = process_entries()?;
    let candidates = entries
        .iter()
        .filter(|entry| verified_new_pids.contains(&entry.pid))
        .map(|entry| (entry.pid, entry.name.clone()))
        .collect::<HashMap<_, _>>();
    if candidates.is_empty() {
        return Ok(CodexDesktopCloseResult::default());
    }

    let seed_pids = candidates.keys().copied().collect::<Vec<_>>();
    let process_tree = codex_desktop_process_tree_from_entries(&entries, &seed_pids)
        .into_iter()
        .filter(|entry| !before.contains(&entry.pid))
        .map(|entry| (entry.pid, entry.name))
        .collect::<Vec<_>>();

    let mut context = CloseWindowsContext {
        candidates,
        matched: HashMap::new(),
        windows: Vec::new(),
    };
    unsafe {
        EnumWindows(
            Some(collect_codex_window),
            LPARAM((&mut context as *mut CloseWindowsContext) as isize),
        )
        .context("无法枚举安装后自动启动的 Codex Desktop 窗口")?;
    }

    let mut window_processes = context.matched.into_iter().collect::<Vec<_>>();
    window_processes.sort_by_key(|(pid, _)| *pid);
    let mut sent = 0usize;
    let mut errors = Vec::new();
    for hwnd in context.windows {
        match unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) } {
            Ok(()) => sent += 1,
            Err(error) => errors.push(error.to_string()),
        }
    }
    if sent == 0 && !errors.is_empty() {
        bail!(
            "无法关闭安装后自动启动的 Codex Desktop 窗口: {}",
            errors.join("；")
        );
    }

    Ok(CodexDesktopCloseResult {
        window_processes,
        process_tree,
        window_count: sent,
    })
}

pub fn running_process_ids(pids: &[u32]) -> Result<Vec<u32>> {
    let wanted = pids.iter().copied().collect::<HashSet<_>>();
    let mut running = process_entries()?
        .into_iter()
        .filter(|entry| wanted.contains(&entry.pid) && is_official_desktop_name(&entry.name))
        .map(|entry| entry.pid)
        .collect::<Vec<_>>();
    running.sort_unstable();
    running.dedup();
    Ok(running)
}

pub fn running_any_process_ids(pids: &[u32]) -> Result<Vec<u32>> {
    let wanted = pids.iter().copied().collect::<HashSet<_>>();
    let mut running = process_entries()?
        .into_iter()
        .filter(|entry| wanted.contains(&entry.pid))
        .map(|entry| entry.pid)
        .collect::<Vec<_>>();
    running.sort_unstable();
    running.dedup();
    Ok(running)
}

/// 精确终止关闭前记录到的官方 Desktop 进程树。
/// PID 会在终止前再次按进程名验证，且调用方传入的顺序为子进程优先。
pub fn terminate_codex_desktop_process_tree(pids: &[u32]) -> Result<Vec<u32>> {
    let wanted = pids.iter().copied().collect::<HashSet<_>>();
    let current = process_entries()?
        .into_iter()
        .filter(|entry| wanted.contains(&entry.pid) && is_official_desktop_name(&entry.name))
        .map(|entry| (entry.pid, entry.name))
        .collect::<HashMap<_, _>>();
    let mut terminated = Vec::new();
    let mut errors = Vec::new();

    for pid in pids.iter().copied() {
        if pid == std::process::id() || !current.contains_key(&pid) {
            continue;
        }
        let result = unsafe {
            match OpenProcess(PROCESS_TERMINATE, false, pid) {
                Ok(handle) => {
                    let terminated =
                        TerminateProcess(handle, 0).with_context(|| format!("无法终止 PID {pid}"));
                    let _ = CloseHandle(handle);
                    terminated
                }
                Err(error) => Err(error).with_context(|| format!("无法打开 PID {pid}")),
            }
        };
        match result {
            Ok(()) => terminated.push(pid),
            Err(error) => {
                let still_running = process_entries()?
                    .into_iter()
                    .any(|entry| entry.pid == pid && is_official_desktop_name(&entry.name));
                if still_running {
                    errors.push(format!("PID {pid}: {error:#}"));
                }
            }
        }
    }

    if !errors.is_empty() {
        bail!(
            "Unable to clean up some Codex Desktop background processes: {}",
            errors.join(", ")
        );
    }
    Ok(terminated)
}

pub fn attach_parent_console() {
    // Release builds use the Windows GUI subsystem to prevent a console flash on double-click.
    // Reattach only for explicit CLI commands so their output still goes to PowerShell/cmd.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

pub fn hide_current_console() {
    // Detach instead of hiding the console window: when launched from an existing terminal,
    // hiding GetConsoleWindow() would also hide the user's PowerShell/cmd window.
    unsafe {
        let _ = FreeConsole();
    }
}

pub fn open_external_url(url: &str) -> Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("拒绝打开非 HTTP(S) 网址");
    }
    let operation = wide_null(std::ffi::OsStr::new("open"));
    let target = wide_null(std::ffi::OsStr::new(url));
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0 as isize;
    if code <= 32 {
        bail!("Windows 无法打开网址（ShellExecuteW={code}）");
    }
    Ok(())
}

pub fn launch_terminal_client_with_environment(
    title: &str,
    command: &str,
    environment: &[(&str, &str)],
) -> Result<u32> {
    launch_terminal_client_with_environment_and_directory(title, command, environment, None)
}

pub fn launch_terminal_client_with_environment_in_directory(
    title: &str,
    command: &str,
    environment: &[(&str, &str)],
    current_directory: &Path,
) -> Result<u32> {
    launch_terminal_client_with_environment_and_directory(
        title,
        command,
        environment,
        Some(current_directory),
    )
}

fn launch_terminal_client_with_environment_and_directory(
    title: &str,
    command: &str,
    environment: &[(&str, &str)],
    current_directory: Option<&Path>,
) -> Result<u32> {
    if !matches!(command, "codex" | "claude") {
        bail!("拒绝启动未知终端客户端: {command}");
    }

    let executable = resolve_terminal_client(command)?;
    let cmd_exe = std::env::var_os("COMSPEC")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));
    if !cmd_exe.is_file() {
        bail!("无法找到 Windows 命令解释器 cmd.exe");
    }

    // GUI worker 没有控制台，它的 stdin/stdout 指向空设备或日志文件。
    // std::process::Command 会把这些句柄继续传给新进程，即使指定 CREATE_NEW_CONSOLE，
    // 交互式 CLI 仍会判断 stdin 不是 TTY。这里直接调用 CreateProcessW，并且不继承句柄。
    // Codex refuses to start its shared Windows daemon from an elevated
    // terminal. Agent-Switch may itself be elevated, so always use the
    // daemon-free mode for the isolated Codex CLI profile. Claude Code keeps
    // its normal invocation.
    let arguments = if command == "codex" {
        ["--no-daemon"].as_slice()
    } else {
        &[]
    };
    let command_line =
        build_terminal_command_line(&cmd_exe, title, &executable, environment, arguments)?;
    let environment_block = if environment.is_empty() {
        None
    } else {
        Some(build_environment_block(environment)?)
    };
    create_interactive_console_process(
        &cmd_exe,
        &command_line,
        environment_block.as_deref(),
        current_directory,
    )
}

#[cfg_attr(test, allow(dead_code))]
pub fn read_codex_bundled_catalog(codex_home: &Path) -> Result<String> {
    let executable = resolve_terminal_client("codex")?;
    let extension = executable
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let mut command =
        if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat") {
            let cmd_exe = std::env::var_os("COMSPEC")
                .map(PathBuf::from)
                .filter(|path| path.is_file())
                .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));
            batch_command(&cmd_exe, &executable, &["debug", "models", "--bundled"])?
        } else {
            let mut command = std::process::Command::new(&executable);
            command.args(["debug", "models", "--bundled"]);
            command
        };
    command.creation_flags(CREATE_NO_WINDOW_FLAG);
    let output = command
        .env("CODEX_HOME", codex_home)
        .output()
        .with_context(|| {
            format!(
                "Unable to read Codex bundled model catalog: {}",
                executable.display()
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "Codex 自带模型目录读取失败（退出码 {:?}）：{}",
            output.status.code(),
            stderr.trim().chars().take(500).collect::<String>()
        );
    }
    String::from_utf8(output.stdout).context("Codex 自带模型目录不是 UTF-8 JSON")
}

fn batch_command(
    cmd_exe: &Path,
    batch_file: &Path,
    arguments: &[&str],
) -> Result<std::process::Command> {
    let working_dir = batch_file
        .parent()
        .context("Terminal client path has no parent directory")?;
    let file_name = batch_file
        .file_name()
        .context("Terminal client path has no file name")?;
    let mut command = std::process::Command::new(cmd_exe);
    // 不把完整批处理路径拼进 cmd /C 的单个字符串，由 Rust 负责参数转义。
    command
        .current_dir(working_dir)
        .args(["/D", "/C", "call"])
        .arg(file_name)
        .args(arguments);
    Ok(command)
}

pub fn verify_authenticode(path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("待验证的安装文件不存在: {}", path.display());
    }

    let path_wide = wide_null(path.as_os_str());
    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(path_wide.as_ptr()),
        hFile: HANDLE::default(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut trust_data = WINTRUST_DATA {
        cbStruct: size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        // 仅使用本机缓存进行吊销检查，避免受限网络让安装校验长时间卡住。
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL_FLAG,
        dwUIContext: WTD_UICONTEXT_EXECUTE,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let status = unsafe {
        WinVerifyTrust(
            HWND(std::ptr::null_mut()),
            &mut action,
            (&mut trust_data as *mut WINTRUST_DATA).cast::<c_void>(),
        )
    };

    trust_data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        let _ = WinVerifyTrust(
            HWND(std::ptr::null_mut()),
            &mut action,
            (&mut trust_data as *mut WINTRUST_DATA).cast::<c_void>(),
        );
    }

    if status != 0 {
        bail!(
            "Windows Authenticode 校验失败（0x{:08X}）: {}",
            status as u32,
            path.display()
        );
    }
    Ok(())
}

pub async fn run_elevated_powershell_encoded_command(
    encoded_command: &str,
    timeout_ms: u32,
) -> Result<u32> {
    if encoded_command.is_empty()
        || !encoded_command
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
    {
        bail!("待提权执行的 PowerShell EncodedCommand 格式无效");
    }

    let encoded_command = encoded_command.to_owned();
    tokio::task::spawn_blocking(move || {
        run_elevated_powershell_encoded_command_blocking(&encoded_command, timeout_ms)
    })
    .await
    .context("等待管理员安装进程失败")?
}
fn run_elevated_powershell_encoded_command_blocking(
    encoded_command: &str,
    timeout_ms: u32,
) -> Result<u32> {
    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let powershell = system_root
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !powershell.is_file() {
        bail!("未找到 Windows PowerShell: {}", powershell.display());
    }

    let verb = wide_null(std::ffi::OsStr::new("runas"));
    let executable = wide_null(powershell.as_os_str());
    let parameters = format!(
        "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {encoded_command}"
    );
    let parameters = wide_null(parameters.as_ref());
    let mut execute = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: HWND(std::ptr::null_mut()),
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(executable.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        lpDirectory: PCWSTR::null(),
        nShow: SW_HIDE.0,
        ..Default::default()
    };

    if let Err(error) = unsafe { ShellExecuteExW(&mut execute) } {
        let cancelled =
            error.code().0 as u32 == 0x8007_04C7 || unsafe { GetLastError() } == ERROR_CANCELLED;
        if cancelled {
            bail!("Claude Desktop 安装需要管理员权限，用户取消了授权");
        }
        return Err(error).context("无法启动管理员 PowerShell 安装进程");
    }
    if execute.hProcess.is_invalid() {
        bail!("管理员 PowerShell 已启动，但 Windows 未返回进程句柄");
    }

    let process = execute.hProcess;
    let result = (|| {
        let wait = unsafe { WaitForSingleObject(process, timeout_ms) };
        if wait == WAIT_TIMEOUT {
            bail!(
                "Waiting for administrator installation process timed out after {} seconds",
                timeout_ms / 1000
            );
        }
        if wait == WAIT_FAILED {
            bail!(
                "Administrator installation process failed (Windows error {})",
                unsafe { GetLastError().0 }
            );
        }
        if wait != WAIT_OBJECT_0 {
            bail!("管理员安装进程返回未知等待状态 0x{:08X}", wait.0);
        }

        let mut exit_code = 0u32;
        unsafe { GetExitCodeProcess(process, &mut exit_code) }
            .context("无法读取管理员安装进程退出码")?;
        Ok(exit_code)
    })();
    unsafe {
        let _ = CloseHandle(process);
    }
    result
}

pub fn open_installer_package(path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("待打开的安装文件不存在: {}", path.display());
    }
    let operation = wide_null(std::ffi::OsStr::new("open"));
    let file = wide_null(path.as_os_str());
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0 as isize;
    if code <= 32 {
        bail!(
            "Windows 无法打开安装程序（ShellExecuteW={}）: {}",
            code,
            path.display()
        );
    }
    Ok(())
}

pub fn terminal_client_installed(command: &str) -> bool {
    matches!(command, "codex" | "claude") && resolve_terminal_client(command).is_ok()
}

pub fn store_codex_installed() -> bool {
    store_package_installed(&[
        "OpenAI.Codex_2p2nqsd0c76g0",
        "OpenAI.CodexBeta_2p2nqsd0c76g0",
        "OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0",
    ])
}

pub fn store_claude_installed() -> bool {
    store_package_installed(&["Claude_pzs8sxrjxfjjc"])
}

fn store_package_installed(package_family_names: &[&str]) -> bool {
    package_family_names.iter().any(|package_family_name| {
        let mut package_count = 0u32;
        let mut buffer_length = 0u32;
        let family = HSTRING::from(*package_family_name);
        // The sizing call is enough for installation detection: Windows fills package_count
        // before returning ERROR_INSUFFICIENT_BUFFER when at least one package is registered.
        unsafe {
            let _ = GetPackagesByPackageFamily(
                &family,
                &mut package_count,
                None,
                &mut buffer_length,
                None,
            );
        }
        package_count > 0
    })
}

fn terminal_npm_package(command: &str) -> Option<&'static str> {
    match command {
        "codex" => Some("@openai/codex"),
        "claude" => Some("@anthropic-ai/claude-code"),
        _ => None,
    }
}

fn npm_package_directory(npm_bin_directory: &Path, package: &str) -> PathBuf {
    package
        .split('/')
        .filter(|component| !component.is_empty())
        .fold(npm_bin_directory.join("node_modules"), |path, component| {
            path.join(component)
        })
}

fn npm_package_bin_exists(npm_bin_directory: &Path, package: &str, command: &str) -> bool {
    let package_directory = npm_package_directory(npm_bin_directory, package);
    let package_json_path = package_directory.join("package.json");
    let Ok(raw) = fs::read_to_string(package_json_path) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    if manifest.get("name").and_then(serde_json::Value::as_str) != Some(package) {
        return false;
    }

    let bin_path = match manifest.get("bin") {
        Some(serde_json::Value::String(path)) => Some(path.as_str()),
        Some(serde_json::Value::Object(entries)) => {
            entries.get(command).and_then(serde_json::Value::as_str)
        }
        _ => None,
    };
    let Some(bin_path) = bin_path else {
        return false;
    };
    let relative = Path::new(bin_path);
    !relative.is_absolute() && package_directory.join(relative).is_file()
}

fn terminal_client_candidate_valid(command: &str, candidate: &Path) -> bool {
    if !candidate.is_file() {
        return false;
    }

    let extension = candidate
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !extension.eq_ignore_ascii_case("cmd") && !extension.eq_ignore_ascii_case("bat") {
        return true;
    }

    let Some(package) = terminal_npm_package(command) else {
        return false;
    };
    let Some(parent) = candidate.parent() else {
        return false;
    };
    let looks_like_npm_bin = parent.join("node_modules").is_dir()
        || parent
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("npm"));
    if looks_like_npm_bin {
        return npm_package_bin_exists(parent, package, command);
    }

    // Non-npm directories may contain manually managed shims; allow them.
    true
}

fn resolve_terminal_client(command: &str) -> Result<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(app_data) = std::env::var_os("APPDATA") {
        let npm = PathBuf::from(app_data).join("npm");
        candidates.push(npm.join(format!("{command}.cmd")));
        candidates.push(npm.join(format!("{command}.exe")));
    }
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        let user_profile = PathBuf::from(user_profile);
        candidates.push(
            user_profile
                .join(".local")
                .join("bin")
                .join(format!("{command}.exe")),
        );
        candidates.push(
            user_profile
                .join(".local")
                .join("bin")
                .join(format!("{command}.cmd")),
        );
    }
    if let Some(path_value) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path_value) {
            candidates.push(directory.join(format!("{command}.cmd")));
            candidates.push(directory.join(format!("{command}.exe")));
            candidates.push(directory.join(format!("{command}.bat")));
        }
    }

    let mut seen = HashSet::new();
    for candidate in candidates {
        let key = candidate.to_string_lossy().to_ascii_lowercase();
        if seen.insert(key) && terminal_client_candidate_valid(command, &candidate) {
            return Ok(candidate);
        }
    }

    bail!("未找到 {command} 命令。请先安装对应客户端，或确认其 .cmd/.exe 已加入当前用户 PATH。")
}
fn build_terminal_command_line(
    cmd_exe: &Path,
    title: &str,
    executable: &Path,
    environment: &[(&str, &str)],
    arguments: &[&str],
) -> Result<String> {
    let mut setup = Vec::new();
    for (name, value) in environment {
        if name.is_empty() || name.contains('=') || name.contains('\0') {
            bail!("invalid terminal environment variable name: {name:?}");
        }
        if value.contains(['\0', '\r', '\n', '"']) {
            bail!("terminal environment variable {name} contains unsupported characters");
        }

        // CreateProcess receives the same override through its Unicode environment block,
        // while this explicit SET also covers cmd/npm wrapper chains that discard it.
        setup.push(format!("set \"{name}={}\"", value.replace('%', "%%")));
    }

    let setup = if setup.is_empty() {
        String::new()
    } else {
        format!("{} & ", setup.join(" & "))
    };
    let arguments = arguments
        .iter()
        .map(|argument| {
            if argument.contains(['\0', '\r', '\n', '"']) {
                Err(anyhow::anyhow!(
                    "terminal argument contains unsupported characters"
                ))
            } else {
                Ok(format!(" \"{}\"", argument.replace('%', "%%")))
            }
        })
        .collect::<Result<String>>()?;
    Ok(format!(
        "\"{}\" /D /K title {} & {}call \"{}\"{}",
        cmd_exe.display(),
        title,
        setup,
        executable.display(),
        arguments,
    ))
}

fn build_environment_block(overrides: &[(&str, &str)]) -> Result<Vec<u16>> {
    let mut entries = std::env::vars_os().collect::<Vec<(OsString, OsString)>>();
    if crate::background::is_managed() {
        // Match the previous worker-process boundary: unrelated parent keys must
        // not override the local credentials applied for this particular client.
        entries.retain(|(name, _)| {
            !matches!(
                name.to_string_lossy().to_ascii_uppercase().as_str(),
                "ZNNZ_API_KEY"
                    | "LITEAPI_API_KEY"
                    | "ANTHROPIC_AUTH_TOKEN"
                    | "OPENAI_API_KEY"
                    | "OPENAI_BASE_URL"
                    | "ANTHROPIC_API_KEY"
            )
        });
    }
    for (name, value) in overrides {
        if name.is_empty() || name.contains('=') || name.contains('\0') {
            bail!("无效的终端环境变量名称: {name:?}");
        }
        if value.contains('\0') {
            bail!("终端环境变量 {name} 包含空字符");
        }

        if let Some((existing_name, existing_value)) = entries
            .iter_mut()
            .find(|(existing_name, _)| existing_name.to_string_lossy().eq_ignore_ascii_case(name))
        {
            *existing_name = OsString::from(name);
            *existing_value = OsString::from(value);
        } else {
            entries.push((OsString::from(name), OsString::from(value)));
        }
    }

    entries.sort_by(|(left, _), (right, _)| {
        left.to_string_lossy()
            .to_ascii_uppercase()
            .cmp(&right.to_string_lossy().to_ascii_uppercase())
    });

    let mut block = Vec::new();
    for (name, value) in entries {
        block.extend(name.as_os_str().encode_wide());
        block.push('=' as u16);
        block.extend(value.as_os_str().encode_wide());
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn resolve_terminal_working_directory(requested: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(path) = requested {
        if !path.is_dir() {
            bail!("终端工作目录不存在或不是目录: {}", path.display());
        }
        return Ok(Some(path.to_path_buf()));
    }

    Ok(std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .filter(|path| path.is_dir()))
}

fn create_interactive_console_process(
    application: &Path,
    command_line: &str,
    environment: Option<&[u16]>,
    current_directory: Option<&Path>,
) -> Result<u32> {
    let application_wide = wide_null(application.as_os_str());
    let mut command_line_wide = wide_null(command_line.as_ref());
    let current_directory = resolve_terminal_working_directory(current_directory)?;
    let current_directory_wide = current_directory
        .as_ref()
        .map(|path| wide_null(path.as_os_str()));
    let current_directory_ptr = current_directory_wide
        .as_ref()
        .map_or(PCWSTR::null(), |value| PCWSTR(value.as_ptr()));
    let environment_ptr = environment.map(|block| block.as_ptr().cast::<c_void>());
    let creation_flags = if environment.is_some() {
        CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT
    } else {
        CREATE_NEW_CONSOLE
    };

    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR(application_wide.as_ptr()),
            Some(PWSTR(command_line_wide.as_mut_ptr())),
            None,
            None,
            false,
            creation_flags,
            environment_ptr,
            current_directory_ptr,
            &startup,
            &mut process,
        )
        .with_context(|| format!("无法启动交互式终端: {}", application.display()))?;
        let process_id = process.dwProcessId;
        let _ = CloseHandle(process.hThread);
        let _ = CloseHandle(process.hProcess);
        Ok(process_id)
    }
}

fn wide_null(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

pub fn set_user_environment(values: &[(&str, &str)]) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (environment, _) = hkcu
        .create_subkey_with_flags("Environment", KEY_SET_VALUE)
        .context("无法打开当前用户环境变量注册表")?;
    for (name, value) in values {
        environment
            .set_value(*name, value)
            .with_context(|| format!("无法设置用户环境变量 {name}"))?;
        // 同步当前启动器进程；新终端会继承注册表中的值。
        unsafe { std::env::set_var(name, value) };
    }
    broadcast_environment_change();
    Ok(())
}

pub async fn activate_store_claude() -> Result<u32> {
    tokio::task::spawn_blocking(activate_store_claude_blocking)
        .await
        .context("Claude Store activation task failed")?
}
fn activate_store_claude_blocking() -> Result<u32> {
    const AUMID: &str = "Claude_pzs8sxrjxfjjc!Claude";
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let result = (|| {
            let manager: IApplicationActivationManager =
                CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER)
                    .context("无法创建 Windows 应用激活管理器")?;
            match manager.ActivateApplication(
                &HSTRING::from(AUMID),
                &HSTRING::new(),
                ACTIVATEOPTIONS(0),
            ) {
                Ok(pid) if pid != 0 => Ok(pid),
                Ok(_) => bail!("Claude Desktop 激活返回 PID 0"),
                Err(error) => Err(error).context(format!("无法激活 Claude Desktop ({AUMID})")),
            }
        })();
        if initialized {
            CoUninitialize();
        }
        result
    }
}

pub async fn activate_store_codex(arguments: &str) -> Result<u32> {
    let arguments = arguments.to_owned();
    tokio::task::spawn_blocking(move || activate_store_codex_blocking(&arguments))
        .await
        .context("Store activation task failed")?
}
fn activate_store_codex_blocking(arguments: &str) -> Result<u32> {
    const AUMIDS: &[&str] = &[
        "OpenAI.Codex_2p2nqsd0c76g0!App",
        "OpenAI.CodexBeta_2p2nqsd0c76g0!App",
        "OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0!App",
    ];
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let result = (|| {
            let manager: IApplicationActivationManager =
                CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER)
                    .context("无法创建 Windows 应用激活管理器")?;
            let mut errors = Vec::new();
            for aumid in AUMIDS {
                match manager.ActivateApplication(
                    &HSTRING::from(*aumid),
                    &HSTRING::from(arguments),
                    ACTIVATEOPTIONS(0),
                ) {
                    Ok(pid) if pid != 0 => return Ok(pid),
                    Ok(_) => errors.push(format!("{aumid}: 返回 PID 0")),
                    Err(error) => errors.push(format!("{aumid}: {error}")),
                }
            }
            bail!(
                "Unable to activate official Codex Desktop: {}",
                errors.join(", ")
            )
        })();
        if initialized {
            CoUninitialize();
        }
        result
    }
}

fn broadcast_environment_change() {
    let wide: Vec<u16> = "Environment\0".encode_utf16().collect();
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(wide.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            5000,
            None,
        );
    }
}

#[allow(dead_code)]
fn _types() -> (HWND, PCWSTR) {
    (HWND(std::ptr::null_mut()), PCWSTR::null())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_restart_preserves_unrelated_processes_and_rejects_active_tools() {
        use std::process::Stdio;
        use std::time::Duration;
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let cmd =
            std::env::var("COMSPEC").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into());
        let spawn = |arguments: &[&str]| {
            ChildGuard(
                std::process::Command::new(&cmd)
                    .args(arguments)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .creation_flags(CREATE_NO_WINDOW_FLAG)
                    .spawn()
                    .unwrap(),
            )
        };
        let mut owned = spawn(&["/D", "/Q", "/K"]);
        let unrelated = spawn(&["/D", "/Q", "/K"]);
        let session =
            TerminalRestartSession::record(owned.0.id(), crate::gui_worker::ClientTarget::CodexCli)
                .unwrap();
        session.close().unwrap();
        owned.0.wait().unwrap();
        session.close().unwrap();
        assert!(
            RestartProcess::open(unrelated.0.id())
                .unwrap()
                .running()
                .unwrap()
        );

        let active = spawn(&[
            "/D",
            "/Q",
            "/K",
            "powershell.exe -NoProfile -NonInteractive -Command \"Start-Sleep -Seconds 60\"",
        ]);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let child_pid = loop {
            let entries = process_entries().unwrap();
            if let Some(entry) = terminal_descendants(&entries, active.0.id())
                .into_iter()
                .find(|entry| normalized_process_name(&entry.name) == "powershell")
            {
                break entry.pid;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        };
        // Keep a handle for fixture cleanup even if an assertion fails.
        struct ProcessGuard(RestartProcess);
        impl Drop for ProcessGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = TerminateProcess(self.0.handle, 0);
                }
            }
        }
        let child = ProcessGuard(RestartProcess::open(child_pid).unwrap());
        let session = TerminalRestartSession::record(
            active.0.id(),
            crate::gui_worker::ClientTarget::ClaudeCode,
        )
        .unwrap();
        assert!(session.close().is_err());
        assert!(session.root.running().unwrap());
        assert!(child.0.running().unwrap());
    }

    #[test]
    fn terminal_restart_tree_and_process_identity_exclude_unrelated_commands() {
        use crate::gui_worker::ClientTarget::*;
        let entries = vec![
            entry(10, 1, "cmd.exe"),
            entry(11, 10, "node.exe"),
            entry(12, 11, "codex.exe"),
            entry(20, 1, "cmd.exe"),
        ];
        assert_eq!(
            terminal_descendants(&entries, 10)
                .iter()
                .map(|entry| entry.pid)
                .collect::<Vec<_>>(),
            vec![11, 12]
        );
        assert!(terminal_restart_process_matches(
            CodexCli,
            "node.exe",
            r"C:\node\node.exe",
            r"node.exe C:\npm\node_modules\@openai\codex\bin\codex.js"
        ));
        assert!(!terminal_restart_process_matches(
            CodexCli,
            "node.exe",
            r"C:\node\node.exe",
            "node.exe user-tool.js"
        ));
        assert!(!terminal_restart_process_matches(
            CodexCli,
            "claude.exe",
            r"C:\tools\claude.exe",
            "claude.exe"
        ));
        assert!(!terminal_restart_process_matches(
            ClaudeCode,
            "conhost.exe",
            r"C:\other\conhost.exe",
            "conhost.exe"
        ));
    }

    #[test]
    fn restart_closes_visible_windows_but_preserves_hidden_tray_windows() {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_OVERLAPPED, WS_VISIBLE,
        };
        use windows::core::w;
        // STATIC is a system class; no message pump or registered class is needed.
        // Both windows are owned by this test thread and destroyed below.
        let hidden = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("restart-hidden-fixture"),
                WS_OVERLAPPED,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        let visible = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("restart-visible-fixture"),
                WS_OVERLAPPED | WS_VISIBLE,
                -30000,
                -30000,
                1,
                1,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        let mut context = CloseWindowsContext {
            candidates: HashMap::from([(std::process::id(), "fixture".to_owned())]),
            matched: HashMap::new(),
            windows: Vec::new(),
        };
        unsafe {
            let _ = collect_codex_window(
                hidden,
                LPARAM((&mut context as *mut CloseWindowsContext) as isize),
            );
            let _ = collect_codex_window(
                visible,
                LPARAM((&mut context as *mut CloseWindowsContext) as isize),
            );
            DestroyWindow(hidden).unwrap();
            DestroyWindow(visible).unwrap();
        }
        assert_eq!(context.windows, vec![visible]);
    }

    #[test]
    fn restart_cleanup_waits_on_original_handle_and_leaves_other_processes_alone() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let spawn = || {
            ChildGuard(
                std::process::Command::new("powershell.exe")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "Start-Sleep -Seconds 60",
                    ])
                    .creation_flags(CREATE_NO_WINDOW_FLAG)
                    .spawn()
                    .unwrap(),
            )
        };
        let mut owned = spawn();
        let unrelated = spawn();
        let process = RestartProcess::open(owned.0.id()).unwrap();
        let unrelated_process = RestartProcess::open(unrelated.0.id()).unwrap();
        assert!(process.running().unwrap());
        assert!(!desktop_image_matches(
            crate::gui_worker::ClientTarget::CodexDesktop,
            &process.image().unwrap()
        ));
        let session = DesktopRestartSession {
            target: crate::gui_worker::ClientTarget::CodexDesktop,
            processes: vec![process],
        };
        assert!(!session.has_visible_windows().unwrap());
        session.terminate_background().unwrap();
        owned.0.wait().unwrap();
        assert!(!session.running().unwrap());
        // A completed session must not act on any later process reusing its PID.
        session.terminate_background().unwrap();
        assert!(unrelated_process.running().unwrap());
    }

    #[test]
    fn process_detection_distinguishes_terminal_sessions_from_desktop_helpers() {
        use crate::gui_worker::ClientTarget::*;
        assert_eq!(
            classify_client_process(
                "chatgpt",
                r"C:\Program Files\WindowsApps\OpenAI.Codex_1_x64\app\ChatGPT.exe",
                "ChatGPT.exe"
            ),
            Some(CodexDesktop)
        );
        assert_eq!(
            classify_client_process(
                "claude",
                r"C:\Program Files\WindowsApps\Claude_1_x64\app\claude.exe",
                "claude.exe"
            ),
            Some(ClaudeDesktop)
        );
        assert_eq!(
            classify_client_process("codex", r"C:\tools\codex.exe", "codex.exe"),
            Some(CodexCli)
        );
        assert_eq!(
            classify_client_process("claude", r"C:\tools\claude.exe", "claude.exe"),
            Some(ClaudeCode)
        );
        assert_eq!(
            classify_client_process(
                "node",
                r"C:\node\node.exe",
                r#"node.exe "C:\npm\node_modules\@openai\codex\bin\codex.js""#
            ),
            Some(CodexCli)
        );
        assert_eq!(
            classify_client_process(
                "node",
                r"C:\node\node.exe",
                r#"node.exe "C:\npm\node_modules\@anthropic-ai\claude-code\cli.js""#
            ),
            Some(ClaudeCode)
        );
        assert_eq!(
            classify_client_process(
                "codex",
                r"C:\OpenAI\Codex\bin\codex.exe",
                "codex.exe app-server --stdio"
            ),
            None
        );
        assert_eq!(
            classify_client_process("node", r"C:\node\node.exe", "node.exe unrelated.js"),
            None
        );
    }

    #[test]
    fn command_line_query_works_with_query_only_access() {
        let command = process_command_line(std::process::id()).unwrap();
        assert!(command.to_ascii_lowercase().contains("znnz_agent_launcher"));
    }

    fn entry(pid: u32, parent_pid: u32, name: &str) -> ProcessEntry {
        ProcessEntry {
            pid,
            parent_pid,
            name: name.to_owned(),
        }
    }

    fn terminal_candidate_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "znnz-terminal-candidate-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn stale_npm_shim_without_package_is_not_installed() {
        let root = terminal_candidate_root("stale-shim");
        let npm = root.join("npm");
        fs::create_dir_all(&npm).unwrap();
        let shim = npm.join("claude.cmd");
        fs::write(&shim, "@echo off\r\nnode missing.js\r\n").unwrap();

        assert!(!terminal_client_candidate_valid("claude", &shim));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn npm_shim_with_matching_package_and_bin_is_installed() {
        let root = terminal_candidate_root("valid-shim");
        let npm = root.join("npm");
        let package = npm.join("node_modules/@anthropic-ai/claude-code");
        fs::create_dir_all(&package).unwrap();
        let shim = npm.join("claude.cmd");
        fs::write(&shim, "@echo off\r\nnode cli.js\r\n").unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name":"@anthropic-ai/claude-code","bin":{"claude":"cli.js"}}"#,
        )
        .unwrap();
        fs::write(package.join("cli.js"), "console.log('ok');\n").unwrap();

        assert!(terminal_client_candidate_valid("claude", &shim));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn npm_shim_with_missing_bin_target_is_not_installed() {
        let root = terminal_candidate_root("missing-bin");
        let npm = root.join("npm");
        let package = npm.join("node_modules/@openai/codex");
        fs::create_dir_all(&package).unwrap();
        let shim = npm.join("codex.cmd");
        fs::write(&shim, "@echo off\r\nnode bin/codex.js\r\n").unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name":"@openai/codex","bin":{"codex":"bin/codex.js"}}"#,
        )
        .unwrap();

        assert!(!terminal_client_candidate_valid("codex", &shim));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn standalone_terminal_executable_is_installed() {
        let root = terminal_candidate_root("standalone-exe");
        let executable = root.join("codex.exe");
        fs::write(&executable, b"MZ").unwrap();

        assert!(terminal_client_candidate_valid("codex", &executable));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn requested_terminal_working_directory_overrides_user_profile() {
        let unique = format!(
            "znnz terminal cwd test {} {}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let directory = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&directory).unwrap();

        let resolved = resolve_terminal_working_directory(Some(&directory)).unwrap();
        assert_eq!(resolved.as_deref(), Some(directory.as_path()));

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn terminal_command_sets_codex_home_before_launching_client() {
        let command = build_terminal_command_line(
            Path::new(r"C:\Windows\System32\cmd.exe"),
            "znnz.net - Codex CLI",
            Path::new(r"C:\Program Files\Codex CLI\codex.cmd"),
            &[("CODEX_HOME", r"C:\isolated profile\codex-cli")],
            &["--no-daemon"],
        )
        .unwrap();

        assert!(command.contains(r#"set "CODEX_HOME=C:\isolated profile\codex-cli" & call"#));
        assert!(command.ends_with(r#"call "C:\Program Files\Codex CLI\codex.cmd" "--no-daemon""#));
    }

    #[test]
    fn environment_block_overrides_codex_home_once() {
        let block = build_environment_block(&[("CODEX_HOME", r"C:\isolated-codex-cli")]).unwrap();
        assert_eq!(block.last(), Some(&0));
        let entries = block
            .split(|unit| *unit == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| String::from_utf16(entry).unwrap())
            .filter(|entry| {
                entry
                    .split_once('=')
                    .is_some_and(|(name, _)| name.eq_ignore_ascii_case("CODEX_HOME"))
            })
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![r"CODEX_HOME=C:\isolated-codex-cli"]);
    }

    #[test]
    fn batch_command_runs_from_a_directory_with_spaces() {
        let unique = format!(
            "znnz batch command test {} {}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("probe.cmd");
        std::fs::write(&script, "@echo off\r\necho %1 %2\r\n").unwrap();
        let cmd_exe = std::env::var_os("COMSPEC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));

        let output = batch_command(&cmd_exe, &script, &["hello", "world"])
            .unwrap()
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "hello world"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn codex_desktop_path_classification_rejects_cli_executables() {
        assert!(is_official_codex_desktop_path(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_1.2.3.0_x64__2p2nqsd0c76g0\app\Codex.exe"
        ));
        assert!(is_official_codex_desktop_path(
            r"C:\Program Files\WindowsApps\OpenAI.ChatGPT-Desktop_1.2.3.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe"
        ));
        assert!(!is_official_codex_desktop_path(
            r"C:\Users\test\AppData\Roaming\npm\node_modules\@openai\codex\bin\codex.exe"
        ));
        assert!(!is_official_codex_desktop_path(
            r"C:\Tools\Codex++\Codex.exe"
        ));
    }

    #[test]
    fn claude_desktop_path_classification_rejects_cli_executables() {
        assert!(is_official_claude_desktop_path(
            r"C:\Program Files\WindowsApps\Claude_1.24012.9.0_x64__pzs8sxrjxfjjc\app\Claude.exe"
        ));
        assert!(!is_official_claude_desktop_path(
            r"C:\Users\test\AppData\Roaming\npm\claude.exe"
        ));
        assert!(!is_official_claude_desktop_path(
            r"C:\Program Files\Claude\Claude.exe"
        ));
    }

    #[test]
    fn process_name_classification_is_narrow_and_case_insensitive() {
        assert_eq!(normalized_process_name("Codex.exe"), "codex");
        assert_eq!(normalized_process_name("CHATGPT.EXE"), "chatgpt");

        assert!(is_official_desktop_name("Codex.exe"));
        assert!(is_official_desktop_name("ChatGPT.exe"));
        assert!(!is_official_desktop_name("Codex++.exe"));

        assert!(is_codex_companion_name("Codex++.exe"));
        assert!(is_codex_companion_name("CodexPlusPlus.exe"));
        assert!(!is_codex_companion_name("Codex.exe"));

        assert!(is_codex_related("codex-plus-plus-manager.exe"));
        assert!(!is_codex_related("znnz-client.exe"));
    }

    #[test]
    fn desktop_tree_contains_only_connected_official_processes_child_first() {
        let entries = vec![
            entry(1, 0, "explorer.exe"),
            entry(10, 1, "ChatGPT.exe"),
            entry(11, 10, "CHATGPT.EXE"),
            entry(12, 10, "Codex.exe"),
            entry(13, 11, "ChatGPT.exe"),
            entry(20, 1, "powershell.exe"),
            entry(21, 20, "Codex.exe"),
            entry(30, 1, "Codex++.exe"),
            entry(31, 30, "ChatGPT.exe"),
            entry(40, 10, "znnz-client.exe"),
        ];

        let tree = codex_desktop_process_tree_from_entries(&entries, &[13, 12]);
        let pids = tree.iter().map(|entry| entry.pid).collect::<Vec<_>>();
        assert_eq!(pids, vec![13, 11, 12, 10]);
        assert!(!pids.contains(&21));
        assert!(!pids.contains(&30));
        assert!(!pids.contains(&31));
        assert!(!pids.contains(&40));
    }

    #[test]
    fn desktop_tree_does_not_cross_non_official_parent_boundary() {
        let entries = vec![
            entry(50, 1, "Codex++.exe"),
            entry(51, 50, "ChatGPT.exe"),
            entry(52, 51, "ChatGPT.exe"),
            entry(60, 1, "ChatGPT.exe"),
        ];

        let tree = codex_desktop_process_tree_from_entries(&entries, &[52]);
        let pids = tree.iter().map(|entry| entry.pid).collect::<Vec<_>>();
        assert_eq!(pids, vec![52, 51]);
        assert!(!pids.contains(&50));
        assert!(!pids.contains(&60));
    }

    #[test]
    fn desktop_tree_ignores_non_official_or_missing_seeds() {
        let entries = vec![entry(70, 1, "powershell.exe"), entry(71, 70, "Codex.exe")];
        assert!(codex_desktop_process_tree_from_entries(&entries, &[70, 999]).is_empty());
    }
}
