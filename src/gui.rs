use crate::gui_layout as layout;
use crate::gui_settings::{self, GuiPreferences};
use crate::gui_worker::{ClientTarget, ModelListMode};
use crate::i18n;
use crate::runtime_state::{self, ClientRuntimeState};
use anyhow::{Context, Result, bail};
use eframe::egui::{
    self, Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, Rect, RichText, Sense,
    Stroke, TextureHandle, TextureOptions, Vec2,
};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::Zeroizing;

#[cfg(windows)]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(windows)]
use std::cell::Cell;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::{
        Dwm::{
            DWMNCRP_DISABLED, DWMWA_BORDER_COLOR, DWMWA_NCRENDERING_POLICY,
            DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
        },
        Gdi::{
            CombineRgn, CreateRectRgn, DeleteObject, GetMonitorInfoW, HGDIOBJ,
            MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow, RGN_DIFF, SetRectRgn,
            SetWindowRgn,
        },
    },
    System::Threading::GetCurrentThreadId,
    UI::{
        HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow,
            SetProcessDpiAwarenessContext,
        },
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            CBT_CREATEWNDW, CWPSTRUCT, CallNextHookEx, GWL_EXSTYLE, GWL_STYLE, GetClientRect,
            GetWindowLongPtrW, GetWindowRect, HCBT_CREATEWND, HHOOK, IsIconic, STYLESTRUCT,
            SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetWindowLongPtrW,
            SetWindowPos, SetWindowsHookExW, UnhookWindowsHookEx, WH_CALLWNDPROC, WH_CBT,
            WM_DPICHANGED, WM_NCACTIVATE, WM_NCCALCSIZE, WM_NCDESTROY, WM_NCPAINT, WM_SHOWWINDOW,
            WM_STYLECHANGING, WS_BORDER, WS_CAPTION, WS_CLIPCHILDREN, WS_EX_CLIENTEDGE,
            WS_EX_DLGMODALFRAME, WS_EX_STATICEDGE, WS_EX_WINDOWEDGE, WS_MAXIMIZEBOX,
            WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
        },
    },
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const LOG_SECTION_FOOTER: &str = "------------------------------";
const LOG_SECTION_SEPARATOR: &str = "\n\n\n";
pub const WINDOW_TITLE: &str = "znnz • Agent • Launcher • v1.0";
const LOGO_URL: &str = "https://r.networkpe.top/znnz-Logo";
const GATEWAY_INFO_URL: &str = "https://r.networkpe.top/znnz-API";
const UPDATE_URL: &str = "https://r.networkpe.top/znnz-Update";
const GITHUB_URL: &str = "https://r.networkpe.top/znnz-GitHub";
const RUNTIME_SCAN_INTERVAL: Duration = Duration::from_secs(1);
const INSTALLATION_SCAN_INTERVAL: Duration = Duration::from_secs(1);
const INPUT_AUTOSAVE_DELAY: Duration = Duration::from_millis(700);

#[cfg(all(feature = "renderer-glow", feature = "renderer-wgpu"))]
compile_error!("renderer-glow 与 renderer-wgpu 不能同时启用");
#[cfg(not(any(feature = "renderer-glow", feature = "renderer-wgpu")))]
compile_error!("必须启用 renderer-glow 或 renderer-wgpu");

#[cfg(feature = "renderer-glow")]
const GUI_RENDERER: eframe::Renderer = eframe::Renderer::Glow;
#[cfg(feature = "renderer-glow")]
const GUI_RENDERER_NAME: &str = "Glow/OpenGL";
#[cfg(feature = "renderer-wgpu")]
const GUI_RENDERER: eframe::Renderer = eframe::Renderer::Wgpu;
#[cfg(feature = "renderer-wgpu")]
const GUI_RENDERER_NAME: &str = "WGPU/DirectX";

pub fn run() -> Result<()> {
    #[cfg(windows)]
    unsafe {
        // 在 eframe / winit 创建 HWND 之前设置 Per-Monitor V2，避免 Windows 在 DPI
        // 切换时先用系统缩放重建一次带标题栏的过渡窗口。
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    crate::platform::hide_current_console();
    let icon = native_icon_data()?;
    let options = eframe::NativeOptions {
        renderer: GUI_RENDERER,
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_icon(icon)
            .with_inner_size([layout::WINDOW_WIDTH, layout::WINDOW_HEIGHT])
            .with_min_inner_size([layout::WINDOW_WIDTH, layout::WINDOW_HEIGHT])
            .with_max_inner_size([layout::WINDOW_WIDTH, layout::WINDOW_HEIGHT])
            .with_resizable(false)
            // Windows 下从 CreateWindowExW 开始保持隐藏；eframe 会在首帧完成绘制后再显示。
            // 这样初始化期间即使系统处理非客户区消息，也没有半成品窗口暴露到桌面。
            .with_visible(false)
            .with_decorations(false)
            .with_transparent(false)
            .with_has_shadow(false),
        // 不保存也不恢复 eframe 的历史窗口位置；每次启动都由 Win32 重新贴靠右下角。
        persist_window: false,
        ..Default::default()
    };
    #[cfg(windows)]
    // winit 即使在 decorations=false 时也会把 WS_CAPTION / WS_BORDER 传给
    // CreateWindowExW，随后才通过 WM_NCCALCSIZE 模拟无边框。这里在线程级 CBT
    // 创建钩子中提前改成真正的 WS_POPUP，避免 DWM 曾经看到系统标题栏和边框。
    let _native_create_hook =
        NativeCreateHook::install().context("无法安装 Windows 原生窗口创建钩子")?;

    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(|creation| Ok(Box::new(LauncherApp::new(creation)))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
    .with_context(|| format!("无法初始化 {GUI_RENDERER_NAME} 图形渲染器"))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    GatewayTest,
    Client(ClientTarget),
    Install(ClientTarget),
}

struct RunningTask {
    sequence: u64,
    kind: TaskKind,
    child: Option<Child>,
    worker_pid: u32,
    log_path: PathBuf,
    label: String,
    log_title: String,
    log_text: String,
    last_log_read: SystemTime,
}

struct CompletedLogSection {
    sequence: u64,
    text: String,
}

struct GuiAssets {
    client_card_normal: TextureHandle,
    client_card_hover: TextureHandle,
    client_card_selected: TextureHandle,
    client_card_running: TextureHandle,
    logo: TextureHandle,
    update: TextureHandle,
    github: TextureHandle,
    url: TextureHandle,
    key: TextureHandle,
    link: TextureHandle,
    models: TextureHandle,
    install: TextureHandle,
    installing: TextureHandle,
    start: TextureHandle,
    prompt: TextureHandle,
    prompt_warn: TextureHandle,
    codex_cli: TextureHandle,
    codex_desktop: TextureHandle,
    claude_code: TextureHandle,
    claude_desktop: TextureHandle,
    badge_cli: TextureHandle,
    badge_desktop: TextureHandle,
    eye_show: TextureHandle,
    eye_hide: TextureHandle,
    select: TextureHandle,
    log: TextureHandle,
    // 三个窗口控制符号合并成一张小型 SVG sprite，只需光栅化一次。
    window_control_glyphs: TextureHandle,
    // 当前纹理对应的 egui 物理像素倍率；显示器 DPI 改变时会按新倍率重建。
    pixels_per_point: f32,
}

impl GuiAssets {
    fn load(context: &egui::Context) -> Self {
        let pixels_per_point = svg_pixels_per_point(context);
        let client_icon_size =
            Vec2::splat(layout::CLIENT_CARD_WIDTH.min(layout::CLIENT_CARD_HEIGHT) * 0.60);
        Self {
            client_card_normal: load_embedded_svg_texture(
                context,
                "gui-client-card-normal-svg",
                include_bytes!("../assets/gui/svg/client-card-normal.svg"),
                Vec2::new(layout::CLIENT_CARD_WIDTH, layout::CLIENT_CARD_HEIGHT),
            ),
            client_card_hover: load_embedded_svg_texture(
                context,
                "gui-client-card-hover-svg",
                include_bytes!("../assets/gui/svg/client-card-hover.svg"),
                Vec2::new(layout::CLIENT_CARD_WIDTH, layout::CLIENT_CARD_HEIGHT),
            ),
            client_card_selected: load_embedded_svg_texture(
                context,
                "gui-client-card-selected-svg",
                include_bytes!("../assets/gui/svg/client-card-selected.svg"),
                Vec2::new(layout::CLIENT_CARD_WIDTH, layout::CLIENT_CARD_HEIGHT),
            ),
            client_card_running: load_embedded_svg_texture(
                context,
                "gui-client-card-running-svg",
                include_bytes!("../assets/gui/svg/client-card-running.svg"),
                Vec2::new(layout::CLIENT_CARD_WIDTH, layout::CLIENT_CARD_HEIGHT),
            ),
            logo: load_embedded_svg_texture(
                context,
                "gui-logo-svg",
                include_bytes!("../assets/gui/svg/logo.svg"),
                Vec2::new(layout::HEADER_LOGO_WIDTH, layout::HEADER_LOGO_HEIGHT),
            ),
            update: load_embedded_svg_texture(
                context,
                "gui-update-svg",
                include_bytes!("../assets/gui/svg/Update.svg"),
                Vec2::splat(layout::HEADER_SUBTITLE_ICON_SIZE),
            ),
            github: load_embedded_svg_texture(
                context,
                "gui-github-svg",
                include_bytes!("../assets/gui/svg/GitHub.svg"),
                Vec2::splat(layout::HEADER_SUBTITLE_ICON_SIZE),
            ),
            url: load_embedded_svg_texture(
                context,
                "gui-url-svg",
                include_bytes!("../assets/gui/svg/url.svg"),
                Vec2::splat(16.0),
            ),
            key: load_embedded_svg_texture(
                context,
                "gui-key-svg",
                include_bytes!("../assets/gui/svg/key.svg"),
                Vec2::splat(16.0),
            ),
            link: load_embedded_svg_texture(
                context,
                "gui-link-svg",
                include_bytes!("../assets/gui/svg/link.svg"),
                Vec2::new(
                    layout::CONNECT_ACTION_ICON_WIDTH,
                    layout::CONNECT_ACTION_ICON_HEIGHT,
                ),
            ),
            models: load_embedded_svg_texture(
                context,
                "gui-models-svg",
                include_bytes!("../assets/gui/svg/models.svg"),
                Vec2::new(
                    layout::MODEL_FETCH_ACTION_ICON_WIDTH,
                    layout::MODEL_FETCH_ACTION_ICON_HEIGHT,
                ),
            ),
            install: load_embedded_svg_texture(
                context,
                "gui-install-svg",
                include_bytes!("../assets/gui/svg/install.svg"),
                Vec2::new(
                    layout::MODEL_INSTALL_ICON_WIDTH,
                    layout::MODEL_INSTALL_ICON_HEIGHT,
                ),
            ),
            installing: load_embedded_svg_texture(
                context,
                "gui-installing-svg",
                include_bytes!("../assets/gui/svg/Installing.svg"),
                Vec2::new(
                    layout::MODEL_INSTALL_ICON_WIDTH,
                    layout::MODEL_INSTALL_ICON_HEIGHT,
                ),
            ),
            start: load_embedded_svg_texture(
                context,
                "gui-start-svg",
                include_bytes!("../assets/gui/svg/start.svg"),
                Vec2::new(
                    layout::MODEL_LAUNCH_ICON_WIDTH,
                    layout::MODEL_LAUNCH_ICON_HEIGHT,
                ),
            ),
            prompt: load_embedded_svg_texture(
                context,
                "gui-prompt-svg",
                include_bytes!("../assets/gui/svg/prompt.svg"),
                Vec2::splat(layout::STATUS_ICON_SIZE),
            ),
            prompt_warn: load_embedded_svg_texture(
                context,
                "gui-prompt-warn-svg",
                include_bytes!("../assets/gui/svg/promptwarn.svg"),
                Vec2::splat(layout::STATUS_ICON_SIZE),
            ),
            codex_cli: load_embedded_svg_texture(
                context,
                "gui-codex-cli-svg",
                include_bytes!("../assets/gui/svg/codex-cli.svg"),
                client_icon_size,
            ),
            codex_desktop: load_embedded_svg_texture(
                context,
                "gui-codex-desktop-svg",
                include_bytes!("../assets/gui/svg/codex-desktop.svg"),
                client_icon_size,
            ),
            claude_code: load_embedded_svg_texture(
                context,
                "gui-claude-code-svg",
                include_bytes!("../assets/gui/svg/claude-code.svg"),
                client_icon_size,
            ),
            claude_desktop: load_embedded_svg_texture(
                context,
                "gui-claude-desktop-svg",
                include_bytes!("../assets/gui/svg/claude-desktop.svg"),
                client_icon_size,
            ),
            badge_cli: load_embedded_svg_texture(
                context,
                "gui-badge-cli-svg",
                include_bytes!("../assets/gui/svg/badge-cli.svg"),
                Vec2::splat(24.0),
            ),
            badge_desktop: load_embedded_svg_texture(
                context,
                "gui-badge-desktop-svg",
                include_bytes!("../assets/gui/svg/badge-desktop.svg"),
                Vec2::splat(24.0),
            ),
            eye_show: load_embedded_svg_texture(
                context,
                "gui-eye-show-svg",
                include_bytes!("../assets/gui/svg/eye-show.svg"),
                Vec2::splat(16.0),
            ),
            eye_hide: load_embedded_svg_texture(
                context,
                "gui-eye-hide-svg",
                include_bytes!("../assets/gui/svg/eye-hide.svg"),
                Vec2::splat(16.0),
            ),
            select: load_embedded_svg_texture(
                context,
                "gui-select-svg",
                include_bytes!("../assets/gui/svg/select.svg"),
                Vec2::new(layout::MODEL_CHECK_WIDTH, layout::MODEL_CHECK_HEIGHT),
            ),
            log: load_embedded_svg_texture(
                context,
                "gui-log-svg",
                include_bytes!("../assets/gui/svg/log.svg"),
                Vec2::splat(18.0),
            ),
            window_control_glyphs: load_embedded_svg_texture(
                context,
                "gui-window-control-glyphs-svg",
                include_bytes!("../assets/gui/svg/window-control-glyphs.svg"),
                Vec2::new(
                    layout::HEADER_TRAFFIC_DOT_SIZE * 3.0,
                    layout::HEADER_TRAFFIC_DOT_SIZE,
                ),
            ),
            pixels_per_point,
        }
    }

    fn ensure_pixels_per_point(&mut self, context: &egui::Context) {
        let current = svg_pixels_per_point(context);
        if (self.pixels_per_point - current).abs() >= 0.01 {
            *self = Self::load(context);
        }
    }

    fn client_card_background(
        &self,
        selected: bool,
        running: bool,
        hovered: bool,
    ) -> &TextureHandle {
        if running {
            &self.client_card_running
        } else if selected {
            &self.client_card_selected
        } else if hovered {
            &self.client_card_hover
        } else {
            &self.client_card_normal
        }
    }

    fn client_icon(&self, target: ClientTarget) -> &TextureHandle {
        match target {
            ClientTarget::CodexCli => &self.codex_cli,
            ClientTarget::CodexDesktop => &self.codex_desktop,
            ClientTarget::ClaudeCode => &self.claude_code,
            ClientTarget::ClaudeDesktop => &self.claude_desktop,
        }
    }

    fn client_badge(&self, target: ClientTarget) -> &TextureHandle {
        match target {
            ClientTarget::CodexCli | ClientTarget::ClaudeCode => &self.badge_cli,
            ClientTarget::CodexDesktop | ClientTarget::ClaudeDesktop => &self.badge_desktop,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientInstallations {
    codex_cli: bool,
    codex_desktop: bool,
    claude_code: bool,
    claude_desktop: bool,
}

impl ClientInstallations {
    fn detect() -> Self {
        Self {
            codex_cli: crate::platform::terminal_client_installed("codex"),
            codex_desktop: crate::platform::store_codex_installed(),
            claude_code: crate::platform::terminal_client_installed("claude"),
            claude_desktop: crate::platform::store_claude_installed(),
        }
    }

    fn is_installed(self, target: ClientTarget) -> bool {
        match target {
            ClientTarget::CodexCli => self.codex_cli,
            ClientTarget::CodexDesktop => self.codex_desktop,
            ClientTarget::ClaudeCode => self.claude_code,
            ClientTarget::ClaudeDesktop => self.claude_desktop,
        }
    }
}

struct LauncherApp {
    gateway_url: String,
    api_key: Zeroizing<String>,
    show_key: bool,
    remember_key: bool,
    selected: ClientTarget,
    client_selected: bool,
    codex_cli_model_mode: ModelListMode,
    codex_desktop_model_mode: ModelListMode,
    claude_code_model_mode: ModelListMode,
    claude_desktop_model_mode: ModelListMode,
    running: Vec<RunningTask>,
    installed_clients: ClientInstallations,
    status: String,
    status_is_error: bool,
    log_text: String,
    completed_logs: Vec<CompletedLogSection>,
    next_log_sequence: u64,
    session_log_path: Option<PathBuf>,
    assets: GuiAssets,
    show_logs: bool,
    align_log_window_on_open: bool,
    #[cfg(windows)]
    native_window: Option<NativeWindowState>,
    last_runtime_scan: SystemTime,
    last_installation_scan: SystemTime,
    preferences_dirty: bool,
    preferences_changed_at: Option<Instant>,
}

impl LauncherApp {
    fn new(creation: &eframe::CreationContext<'_>) -> Self {
        #[cfg(windows)]
        let native_window = NativeWindowState::configure(creation);
        install_fonts(&creation.egui_ctx);
        configure_style(&creation.egui_ctx);
        let assets = GuiAssets::load(&creation.egui_ctx);
        let (preferences, key, warning) = match gui_settings::load() {
            Ok((preferences, key)) => (preferences, key, None),
            Err(error) => (
                GuiPreferences::default(),
                None,
                Some(format!(
                    "{}: {error:#}",
                    i18n::tr(
                        "读取本地设置失败，将使用默认值",
                        "Failed to load local settings; defaults will be used"
                    )
                )),
            ),
        };
        let selected = ClientTarget::from_id(&preferences.selected_client);
        let codex_cli_model_mode = ModelListMode::from_id(&preferences.codex_cli_model_mode);
        let codex_desktop_model_mode =
            ModelListMode::from_id(&preferences.codex_desktop_model_mode);
        let claude_code_model_mode = ModelListMode::from_id(&preferences.claude_code_model_mode);
        let claude_desktop_model_mode =
            ModelListMode::from_id(&preferences.claude_desktop_model_mode);
        let installed_clients = ClientInstallations::detect();
        let session_log_path = match create_session_log_file() {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::warn!("GUI 无法创建会话汇总日志：{error:#}");
                None
            }
        };
        let had_settings_warning = warning.is_some();
        let mut app = Self {
            gateway_url: preferences.gateway_url,
            api_key: key.unwrap_or_else(|| Zeroizing::new(String::new())),
            show_key: false,
            remember_key: true,
            selected,
            client_selected: false,
            codex_cli_model_mode,
            codex_desktop_model_mode,
            claude_code_model_mode,
            claude_desktop_model_mode,
            running: Vec::new(),
            installed_clients,
            status: warning.unwrap_or_else(|| default_status().to_owned()),
            status_is_error: had_settings_warning,
            log_text: String::new(),
            completed_logs: Vec::new(),
            next_log_sequence: 1,
            session_log_path,
            assets,
            show_logs: false,
            align_log_window_on_open: false,
            #[cfg(windows)]
            native_window,
            last_runtime_scan: UNIX_EPOCH,
            last_installation_scan: SystemTime::now(),
            preferences_dirty: false,
            preferences_changed_at: None,
        };
        if let Err(error) = app.sync_runtime_states()
            && !had_settings_warning
        {
            app.status = format!(
                "{}: {error:#}",
                i18n::tr(
                    "读取客户端运行状态失败",
                    "Failed to read client runtime state"
                )
            );
            app.status_is_error = true;
        }
        app
    }

    fn validate_inputs(&self) -> Result<()> {
        validate_gateway_inputs(&self.gateway_url, &self.api_key)
    }

    fn model_list_mode(&self, target: ClientTarget) -> ModelListMode {
        match target {
            ClientTarget::CodexCli => self.codex_cli_model_mode,
            ClientTarget::CodexDesktop => self.codex_desktop_model_mode,
            ClientTarget::ClaudeCode => self.claude_code_model_mode,
            ClientTarget::ClaudeDesktop => self.claude_desktop_model_mode,
        }
    }

    fn set_model_list_mode(&mut self, target: ClientTarget, mode: ModelListMode) {
        match target {
            ClientTarget::CodexCli => self.codex_cli_model_mode = mode,
            ClientTarget::CodexDesktop => self.codex_desktop_model_mode = mode,
            ClientTarget::ClaudeCode => self.claude_code_model_mode = mode,
            ClientTarget::ClaudeDesktop => self.claude_desktop_model_mode = mode,
        }
    }

    fn allocate_log_sequence(&mut self) -> u64 {
        let sequence = self.next_log_sequence;
        self.next_log_sequence = self.next_log_sequence.saturating_add(1);
        sequence
    }

    fn attach_runtime_state(&mut self, state: ClientRuntimeState) {
        let kind = TaskKind::Client(state.client);
        if self.is_running(kind) {
            return;
        }
        self.set_model_list_mode(state.client, state.model_list_mode);
        let sequence = self.allocate_log_sequence();
        self.running.push(RunningTask {
            sequence,
            kind,
            child: None,
            worker_pid: state.worker_pid,
            log_path: state.log_path,
            label: state.client.title().to_owned(),
            log_title: task_log_title(kind, state.client.title()),
            log_text: String::new(),
            last_log_read: UNIX_EPOCH,
        });
    }

    fn sync_runtime_states(&mut self) -> Result<()> {
        let mut states = runtime_state::load_active_states()?;
        states.sort_by_key(|state| state.started_at_ms);
        self.last_runtime_scan = SystemTime::now();

        let mut completed_logs = Vec::new();
        self.running.retain_mut(|task| {
            if task.child.is_some() || !matches!(task.kind, TaskKind::Client(_)) {
                return true;
            }
            let active = states.iter().any(|state| {
                task.kind == TaskKind::Client(state.client) && task.worker_pid == state.worker_pid
            });
            if !active {
                refresh_task_log(task);
                completed_logs.push((
                    task.sequence,
                    format_log_section(&task.log_title, &task.log_text),
                ));
            }
            active
        });
        for (sequence, log) in completed_logs {
            self.append_completed_log(sequence, log);
        }

        for state in states {
            self.attach_runtime_state(state);
        }
        Ok(())
    }

    fn scan_runtime_states_if_due(&mut self) {
        if SystemTime::now()
            .duration_since(self.last_runtime_scan)
            .unwrap_or_default()
            < RUNTIME_SCAN_INTERVAL
        {
            return;
        }
        if let Err(error) = self.sync_runtime_states() {
            tracing::warn!("GUI 无法刷新客户端运行状态：{error:#}");
        }
    }
    fn scan_installations_if_due(&mut self) {
        if SystemTime::now()
            .duration_since(self.last_installation_scan)
            .unwrap_or_default()
            < INSTALLATION_SCAN_INTERVAL
        {
            return;
        }
        self.last_installation_scan = SystemTime::now();

        let previous = self.installed_clients;
        let detected = ClientInstallations::detect();
        if detected == previous {
            return;
        }
        self.installed_clients = detected;

        // 未选择客户端时只刷新安装缓存，不用上次保存的客户端覆盖默认提示。
        if !self.client_selected {
            return;
        }

        // 安装任务和网关检查运行时，状态栏用于展示实时进度，不在这里覆盖。
        if self.is_running(TaskKind::Install(self.selected))
            || self.is_running(TaskKind::GatewayTest)
        {
            return;
        }
        self.status = selected_client_status(
            self.selected,
            self.model_list_mode(self.selected),
            self.client_is_installed(self.selected),
            self.client_is_running(self.selected),
        );
        self.status_is_error = false;
    }

    fn save_preferences(&self) -> Result<()> {
        let preferences = GuiPreferences {
            gateway_url: normalized_gateway_url(&self.gateway_url),
            selected_client: self.selected.id().to_owned(),
            remember_key: self.remember_key,
            codex_cli_model_mode: self.codex_cli_model_mode.id().to_owned(),
            codex_desktop_model_mode: self.codex_desktop_model_mode.id().to_owned(),
            claude_code_model_mode: self.claude_code_model_mode.id().to_owned(),
            claude_desktop_model_mode: self.claude_desktop_model_mode.id().to_owned(),
        };
        gui_settings::save(&preferences, &self.api_key)
    }

    fn mark_preferences_dirty(&mut self) {
        self.preferences_dirty = true;
        self.preferences_changed_at = Some(Instant::now());
    }

    fn maybe_autosave_preferences(&mut self) {
        if !self.preferences_dirty
            || !self
                .preferences_changed_at
                .is_some_and(|changed| changed.elapsed() >= INPUT_AUTOSAVE_DELAY)
        {
            return;
        }
        match self.save_preferences() {
            Ok(()) => {
                self.preferences_dirty = false;
                self.preferences_changed_at = None;
            }
            Err(error) => tracing::warn!("GUI 输入自动保存失败: {error:#}"),
        }
    }

    fn start_gateway_test(&mut self, fetch_models: bool) {
        if self.is_running(TaskKind::GatewayTest) {
            self.set_error(anyhow::anyhow!(i18n::tr(
                "网关检查已经在运行",
                "A gateway check is already running"
            )));
            return;
        }
        if let Err(error) = self.validate_inputs().and_then(|_| self.save_preferences()) {
            self.set_error(error);
            return;
        }
        let mut arguments = vec![
            "internal-test-gateway".to_owned(),
            "--gateway-url".to_owned(),
            normalized_gateway_url(&self.gateway_url),
        ];
        if fetch_models {
            arguments.push("--fetch-models".to_owned());
        }
        let (label, status) = if fetch_models {
            (
                i18n::tr("拉取模型", "Fetch models"),
                i18n::tr(
                    "正在拉取网关模型目录 …",
                    "Fetching the gateway model catalog …",
                ),
            )
        } else {
            (
                i18n::tr("连接测试", "Connection test"),
                i18n::tr("正在测试接口连接 …", "Testing the gateway connection …"),
            )
        };
        match self.spawn_worker(TaskKind::GatewayTest, label, &arguments, true) {
            Ok(task) => {
                self.running.push(task);
                self.status = status.to_owned();
                self.status_is_error = false;
            }
            Err(error) => self.set_error(error),
        }
    }

    fn start_selected_client(&mut self) {
        if !self.client_selected {
            self.status = default_status().to_owned();
            self.status_is_error = false;
            return;
        }
        let target = self.selected;
        if !self.client_is_installed(target) {
            self.status = client_launch_requires_install_status(target);
            self.status_is_error = true;
            return;
        }
        if let Err(error) = self.validate_inputs().and_then(|_| self.save_preferences()) {
            self.set_error(error);
            return;
        }
        let kind = TaskKind::Client(target);
        if self.is_running(kind) {
            self.set_error(anyhow::anyhow!(
                "{} {}",
                target.title(),
                i18n::tr(
                    "已经由本启动器运行",
                    "is already running from this launcher"
                )
            ));
            return;
        }
        let arguments =
            client_worker_arguments(target, &self.gateway_url, self.model_list_mode(target));
        match self.spawn_worker(kind, target.title(), &arguments, true) {
            Ok(task) => {
                self.running.push(task);
                self.status =
                    selected_client_status(target, self.model_list_mode(target), true, true);
                self.status_is_error = false;
            }
            Err(error) => self.set_error(error),
        }
    }

    fn start_selected_client_install(&mut self) {
        if !self.client_selected {
            self.status = default_status().to_owned();
            self.status_is_error = false;
            return;
        }
        let target = self.selected;
        let mode = self.model_list_mode(target);
        if self.client_is_installed(target) {
            self.status =
                selected_client_status(target, mode, true, self.client_is_running(target));
            self.status_is_error = false;
            return;
        }

        let kind = TaskKind::Install(target);
        if self.is_running(kind) {
            self.status = format!(
                "{} {} ...",
                i18n::tr("正在安装", "Installing"),
                target.title()
            );
            self.status_is_error = false;
            return;
        }

        let arguments = vec!["internal-install-client".to_owned(), target.id().to_owned()];
        let label = format!("{} {}", i18n::tr("安装", "Install"), target.title());
        match self.spawn_worker(kind, &label, &arguments, false) {
            Ok(task) => {
                self.running.push(task);
                self.status = format!(
                    "{} {} ...",
                    i18n::tr("正在安装", "Installing"),
                    target.title()
                );
                self.status_is_error = false;
            }
            Err(error) => self.set_error(error),
        }
    }

    fn spawn_worker(
        &mut self,
        kind: TaskKind,
        label: &str,
        arguments: &[String],
        pass_api_key: bool,
    ) -> Result<RunningTask> {
        let executable = std::env::current_exe().context("无法确定当前启动器路径")?;
        let log_title = task_log_title(kind, label);
        let log_path = next_task_log_path(&task_log_slug(kind, label))?;
        let mut log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&log_path)
            .with_context(|| format!("无法创建日志文件 {}", log_path.display()))?;
        // 每个任务日志都使用 UTF-8 BOM，并从统一标题开始，便于直接用记事本查看。
        log.write_all(b"\xEF\xBB\xBF")
            .context("无法初始化 UTF-8 日志文件")?;
        writeln!(log, "===== {log_title} =====").context("无法写入任务日志标题")?;
        log.flush().context("无法刷新任务日志标题")?;
        let error_log = log.try_clone().context("无法复制日志文件句柄")?;
        let mut command = Command::new(executable);
        command.args(arguments);
        for name in [
            "ZNNZ_API_KEY",
            "LITEAPI_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
        ] {
            command.env_remove(name);
        }
        if pass_api_key {
            command.env("ZNNZ_API_KEY", self.api_key.as_str());
        }
        command
            .env("ZNNZ_RUNTIME_LOG_PATH", &log_path)
            .env(i18n::LANGUAGE_ENV, i18n::language().tag())
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(error_log));
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let child = command.spawn().context("无法启动后台工作进程")?;
        let worker_pid = child.id();
        let sequence = self.allocate_log_sequence();
        Ok(RunningTask {
            sequence,
            kind,
            child: Some(child),
            worker_pid,
            log_path,
            label: label.to_owned(),
            log_title,
            log_text: String::new(),
            last_log_read: UNIX_EPOCH,
        })
    }

    fn is_running(&self, kind: TaskKind) -> bool {
        self.running.iter().any(|task| task.kind == kind)
    }

    fn client_is_running(&self, target: ClientTarget) -> bool {
        tasks_contain_client(&self.running, target)
    }

    fn client_is_installed(&self, target: ClientTarget) -> bool {
        self.installed_clients.is_installed(target)
    }

    fn poll_tasks(&mut self) {
        let selected_was_running = self.client_selected && self.client_is_running(self.selected);
        self.scan_runtime_states_if_due();
        self.scan_installations_if_due();
        if selected_was_running && !self.client_is_running(self.selected) {
            self.status = client_exited_status(self.selected);
            self.status_is_error = false;
        }

        let mut finished = Vec::new();
        let mut install_progress = None;
        for (index, task) in self.running.iter_mut().enumerate() {
            refresh_task_log(task);
            let Some(child) = task.child.as_mut() else {
                continue;
            };
            match child.try_wait() {
                Ok(Some(status)) => finished.push((index, status.success(), None)),
                Ok(None) => {
                    if let TaskKind::Install(target) = task.kind
                        && self.client_selected
                        && target == self.selected
                    {
                        install_progress = latest_install_progress(&task.log_text);
                    }
                }
                Err(error) => finished.push((index, false, Some(error.to_string()))),
            }
        }
        if let Some(progress) = install_progress {
            self.status = progress;
            self.status_is_error = false;
        }

        for (index, success, wait_error) in finished.into_iter().rev() {
            let mut task = self.running.remove(index);
            refresh_task_log(&mut task);
            let completed_log = format_log_section(&task.log_title, &task.log_text);
            self.append_completed_log(task.sequence, completed_log);

            if let TaskKind::Install(target) = task.kind {
                if let Some(error) = wait_error {
                    self.status = format!(
                        "{} {}: {error}",
                        i18n::tr("无法读取", "Failed to read"),
                        i18n::tr("客户端安装状态", "client installation status")
                    );
                    self.status_is_error = true;
                } else if success {
                    self.installed_clients = ClientInstallations::detect();
                    if self.client_is_installed(target) {
                        if self.client_selected && target == self.selected {
                            self.status = selected_client_status(
                                target,
                                self.model_list_mode(target),
                                true,
                                self.client_is_running(target),
                            );
                        } else {
                            self.status =
                                format!("{} • {}", target.title(), i18n::tr("已安装", "Installed"));
                        }
                        self.status_is_error = false;
                    } else {
                        self.status = format!(
                            "{} • {}",
                            target.title(),
                            i18n::tr(
                                "安装流程已结束，但复检仍未发现客户端，请查看运行日志",
                                "Installation finished, but the client was not detected; check the runtime log"
                            )
                        );
                        self.status_is_error = true;
                    }
                } else {
                    self.status = format!(
                        "{} • {}",
                        target.title(),
                        i18n::tr(
                            "安装失败 • 请查看运行日志",
                            "Installation failed • Check the runtime log"
                        )
                    );
                    self.status_is_error = true;
                }
                continue;
            }

            if let TaskKind::Client(target) = task.kind {
                if let Some(error) = wait_error {
                    self.status = format!(
                        "{} {}: {error}",
                        i18n::tr("无法读取后台状态", "Failed to read background status for"),
                        task.label
                    );
                    self.status_is_error = true;
                } else if success {
                    self.status = client_exited_status(target);
                    self.status_is_error = false;
                } else {
                    self.status = failed_task_status(&task);
                    self.status_is_error = true;
                }
                continue;
            }

            if let Some(error) = wait_error {
                self.status = format!(
                    "{} {}: {error}",
                    i18n::tr("无法读取后台状态", "Failed to read background status for"),
                    task.label
                );
                self.status_is_error = true;
            } else if success {
                self.status = completed_task_status(&task);
                self.status_is_error = false;
            } else {
                self.status = failed_task_status(&task);
                self.status_is_error = true;
            }
        }
        self.rebuild_log_text();
    }

    fn append_completed_log(&mut self, sequence: u64, section: String) {
        self.completed_logs.push(CompletedLogSection {
            sequence,
            text: section,
        });
        self.completed_logs.sort_by_key(|section| section.sequence);
        if let Some(path) = &self.session_log_path
            && let Err(error) = write_session_log_sections(path, &self.completed_logs)
        {
            tracing::warn!("GUI 无法重写会话汇总日志 {}：{error:#}", path.display());
        }
    }

    fn rebuild_log_text(&mut self) {
        let completed = self
            .completed_logs
            .iter()
            .map(|section| (section.sequence, section.text.clone()));
        let running = self.running.iter().map(|task| {
            (
                task.sequence,
                format_log_section(&task.log_title, &task.log_text),
            )
        });
        self.log_text = join_ordered_log_sections(completed.chain(running));
    }

    fn set_error(&mut self, error: anyhow::Error) {
        self.status = i18n::runtime_error(&error);
        self.status_is_error = true;
    }
}

impl eframe::App for LauncherApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // 主窗口改为不透明原生窗口，彻底避开 Windows 透明合成层留下的顶部黑线。
        Color32::from_rgb(43, 43, 43).to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // SVG 纹理按当前显示器的真实 DPI 生成；拖到不同缩放比例的显示器后立即重建。
        self.assets.ensure_pixels_per_point(ui.ctx());

        #[cfg(windows)]
        if let Some(native_window) = self.native_window.as_mut() {
            native_window.maintain(ui.ctx());
        }

        self.poll_tasks();
        ui.ctx().request_repaint_after(if self.running.is_empty() {
            RUNTIME_SCAN_INTERVAL
        } else {
            Duration::from_millis(350)
        });

        // 顶层区域全部使用同一个绝对坐标系计算，不再混用 egui 游标、add_space 或 Frame 自动尺寸。
        // 因此 gui_layout.rs 中的 padding / gap 会一一对应到最终可见像素。
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        let window_rect = ui.max_rect();
        paint_window_background(ui, window_rect);

        let header_rect = Rect::from_min_size(
            window_rect.min,
            Vec2::new(window_rect.width(), layout::HEADER_HEIGHT),
        );
        let divider_top_rect = Rect::from_min_size(
            egui::pos2(window_rect.left(), header_rect.bottom()),
            Vec2::new(window_rect.width(), layout::DIVIDER_HEIGHT),
        );
        let status_rect = Rect::from_min_max(
            egui::pos2(
                window_rect.left(),
                window_rect.bottom() - layout::STATUS_HEIGHT,
            ),
            window_rect.max,
        );
        let divider_bottom_rect = Rect::from_min_size(
            egui::pos2(
                window_rect.left(),
                status_rect.top() - layout::DIVIDER_HEIGHT,
            ),
            Vec2::new(window_rect.width(), layout::DIVIDER_HEIGHT),
        );
        let content_rect = Rect::from_min_max(
            egui::pos2(window_rect.left(), divider_top_rect.bottom()),
            egui::pos2(window_rect.right(), divider_bottom_rect.top()),
        );

        render_header(ui, &self.assets, header_rect);
        render_divider(ui, divider_top_rect);

        let content_inner = Rect::from_min_max(
            content_rect.min + Vec2::new(layout::CONTENT_PADDING_LEFT, layout::CONTENT_PADDING_TOP),
            content_rect.max
                - Vec2::new(
                    layout::CONTENT_PADDING_RIGHT,
                    layout::CONTENT_PADDING_BOTTOM,
                ),
        );

        // 区块 1：两行输入框都从同一个起点手工计算，横向和纵向 gap 不再经过不同的布局路径。
        let first_input_rect = Rect::from_min_size(
            content_inner.min,
            Vec2::new(content_inner.width(), layout::INPUT_ROW_HEIGHT),
        );
        let second_input_rect = Rect::from_min_size(
            egui::pos2(
                content_inner.left(),
                first_input_rect.bottom() + layout::INPUT_ROWS_VERTICAL_GAP,
            ),
            Vec2::new(content_inner.width(), layout::INPUT_ROW_HEIGHT),
        );

        // 已启动的客户端持有自己的启动参数；继续编辑地址或密钥不会影响它。
        // 这里只在连接测试本身运行期间短暂禁用输入，避免重复提交。
        let gateway_test_enabled = !self.is_running(TaskKind::GatewayTest);
        let (test_clicked, gateway_url_changed) = render_input_row_at(
            ui,
            first_input_rect,
            &self.assets.url,
            &mut self.gateway_url,
            "https://api.znnz.net",
            false,
            None,
            &self.assets.link,
            Vec2::new(
                layout::CONNECT_ACTION_ICON_WIDTH,
                layout::CONNECT_ACTION_ICON_HEIGHT,
            ),
            i18n::tr("测试连接", "Test connection"),
            gateway_test_enabled,
        );
        let (models_clicked, api_key_changed) = render_input_row_at(
            ui,
            second_input_rect,
            &self.assets.key,
            &mut self.api_key,
            "sk-xxxxxxxxxxxxxxxx...",
            !self.show_key,
            Some((
                &mut self.show_key,
                &self.assets.eye_show,
                &self.assets.eye_hide,
            )),
            &self.assets.models,
            Vec2::new(
                layout::MODEL_FETCH_ACTION_ICON_WIDTH,
                layout::MODEL_FETCH_ACTION_ICON_HEIGHT,
            ),
            i18n::tr("拉取模型", "Fetch models"),
            gateway_test_enabled,
        );
        if gateway_url_changed || api_key_changed {
            self.mark_preferences_dirty();
            ui.ctx().request_repaint_after(INPUT_AUTOSAVE_DELAY);
        }
        self.maybe_autosave_preferences();
        if test_clicked {
            self.start_gateway_test(false);
        } else if models_clicked {
            self.start_gateway_test(true);
        }

        // 区块 1 与区块 2 之间：水平居中的级联流 SVG。
        let cascade_flow_rect = Rect::from_center_size(
            egui::pos2(
                content_inner.center().x,
                second_input_rect.bottom() + layout::CASCADE_FLOW_HEIGHT / 2.0,
            ),
            Vec2::new(layout::CASCADE_FLOW_WIDTH, layout::CASCADE_FLOW_HEIGHT),
        );
        paint_cascade_flow(ui, cascade_flow_rect);

        // 区块 2-1：客户端卡片面板。
        let client_panel_rect = Rect::from_min_size(
            egui::pos2(
                content_inner.left(),
                cascade_flow_rect.bottom() + layout::BLOCKS_VERTICAL_GAP,
            ),
            Vec2::new(content_inner.width(), layout::CLIENT_CARDS_BLOCK_HEIGHT),
        );
        render_client_selector_at(ui, client_panel_rect, self);

        // 区块 2-2：模型列表来源与右对齐的启动按钮。
        let model_actions_rect = Rect::from_min_size(
            egui::pos2(
                content_inner.left(),
                client_panel_rect.bottom() + layout::CLIENT_CARDS_ACTIONS_VERTICAL_GAP,
            ),
            Vec2::new(content_inner.width(), layout::MODEL_ACTION_ROW_HEIGHT),
        );
        render_model_source_and_launch_at(ui, model_actions_rect, self);

        render_divider(ui, divider_bottom_rect);
        render_status_bar(ui, status_rect, self);

        maybe_write_layout_probe(
            ui,
            &[
                ("window", window_rect),
                ("header", header_rect),
                ("divider_top", divider_top_rect),
                ("content", content_rect),
                ("content_inner", content_inner),
                ("input_1", first_input_rect),
                ("input_2", second_input_rect),
                ("cascade_flow", cascade_flow_rect),
                ("client_panel", client_panel_rect),
                ("model_actions", model_actions_rect),
                ("divider_bottom", divider_bottom_rect),
                ("status", status_rect),
            ],
        );

        if self.show_logs {
            let mut open = true;
            // Never let the floating log window exceed the launcher window;
            // this matters especially when Windows DPI scaling makes the
            // logical fixed height larger than the current viewport.
            let log_height = layout::LOG_WINDOW_HEIGHT
                .min((window_rect.height() - layout::LOG_WINDOW_MARGIN * 2.0).max(1.0));
            let log_text = if self.log_text.trim().is_empty() {
                i18n::tr("暂无运行日志。", "No runtime logs yet.").to_owned()
            } else {
                self.log_text.clone()
            };
            let mut log_window = egui::Window::new(
                RichText::new(i18n::tr("运行日志", "Runtime log"))
                    .size(layout::LOG_WINDOW_TITLE_FONT_SIZE),
            )
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(Align2::CENTER_BOTTOM)
            .fixed_size(Vec2::new(
                (window_rect.width() - layout::LOG_WINDOW_MARGIN * 2.0).max(1.0),
                log_height,
            ))
            .frame(
                egui::Frame::window(ui.style())
                    .fill(Color32::from_rgb(24, 25, 30))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(67, 69, 78)))
                    .corner_radius(14),
            );
            if self.align_log_window_on_open {
                // 只在日志窗口刚打开时重置位置；随后仍允许用户自由拖动。
                // CENTER_BOTTOM pivot 让日志窗口相对主窗口水平居中，并保留左、右、下边距。
                log_window = log_window.current_pos(egui::pos2(
                    window_rect.center().x,
                    window_rect.bottom() - layout::LOG_WINDOW_MARGIN,
                ));
            }
            log_window.show(ui.ctx(), |ui| {
                ui.visuals_mut().override_text_color = Some(TEXT_MAIN);
                // Keep the log body as a fixed viewport. A TextEdit whose
                // desired row count equals the complete log would otherwise
                // make the egui window grow with every new line and leave no
                // room for the enclosing scrollbar.
                let body_height = (log_height - 52.0).max(1.0);
                ui.set_height(body_height);
                // TextEdit owns the scroll position and selection state. Unlike a
                // selectable Label, it asks the enclosing ScrollArea to follow
                // the selection cursor while the user drags beyond the viewport.
                let mut selectable_log = log_text;
                let viewport = Vec2::new(ui.available_width(), body_height);
                egui::ScrollArea::both()
                    .max_width(viewport.x)
                    .max_height(viewport.y)
                    .min_scrolled_height(viewport.y)
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let row_count = selectable_log.lines().count().max(1);
                        let response = ui.add(
                            egui::TextEdit::multiline(&mut selectable_log)
                                .font(FontId::monospace(11.0))
                                .desired_width(ui.available_width())
                                // The outer ScrollArea owns the fixed viewport;
                                // the full text height supplies its scrollable
                                // content and keeps text selection usable.
                                .desired_rows(row_count)
                                .frame(egui::Frame::NONE),
                        );

                        // egui's text selection requests a scroll on selection
                        // changes, but it does not continuously follow the
                        // pointer when dragging past the viewport edge. Push
                        // the enclosing ScrollArea while the primary button is
                        // held near an edge.
                        if ui.ctx().is_being_dragged(response.id)
                            && ui.input(|input| input.pointer.primary_down())
                            && let Some(pointer) = ui.input(|input| input.pointer.latest_pos())
                        {
                            let clip = ui.clip_rect();
                            let edge = 20.0;
                            let mut delta = Vec2::ZERO;
                            if pointer.y < clip.top() + edge {
                                delta.y = 12.0;
                            } else if pointer.y > clip.bottom() - edge {
                                delta.y = -12.0;
                            }
                            if pointer.x < clip.left() + edge {
                                delta.x = 12.0;
                            } else if pointer.x > clip.right() - edge {
                                delta.x = -12.0;
                            }
                            if delta != Vec2::ZERO {
                                ui.scroll_with_delta(delta);
                                ui.ctx().request_repaint();
                            }
                        }
                    });
            });
            self.align_log_window_on_open = false;
            self.show_logs = open;
        }
    }

    #[cfg(feature = "renderer-wgpu")]
    fn on_exit(&mut self) {
        if let Err(error) = self.save_preferences() {
            tracing::warn!("GUI 退出时保存输入失败: {error:#}");
        }
    }

    #[cfg(feature = "renderer-glow")]
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Err(error) = self.save_preferences() {
            tracing::warn!("GUI 退出时保存输入失败: {error:#}");
        }
    }
}

fn log_window_base_color() -> Color32 {
    // The main window uses its own egui-painted rounded background. This color is only
    // used by egui-owned secondary windows such as the detached log viewer.
    Color32::from_rgba_unmultiplied(12, 14, 18, 204)
}

fn input_surface_bg_color() -> Color32 {
    Color32::from_rgba_unmultiplied(
        layout::INPUT_SURFACE_BG_RED,
        layout::INPUT_SURFACE_BG_GREEN,
        layout::INPUT_SURFACE_BG_BLUE,
        layout::INPUT_SURFACE_BG_ALPHA,
    )
}

fn input_hint_text_color() -> Color32 {
    Color32::from_rgba_unmultiplied(
        layout::INPUT_HINT_TEXT_RED,
        layout::INPUT_HINT_TEXT_GREEN,
        layout::INPUT_HINT_TEXT_BLUE,
        layout::INPUT_HINT_TEXT_ALPHA,
    )
}

fn secondary_surface_bg_color() -> Color32 {
    Color32::from_rgba_unmultiplied(
        layout::SECONDARY_SURFACE_BG_RED,
        layout::SECONDARY_SURFACE_BG_GREEN,
        layout::SECONDARY_SURFACE_BG_BLUE,
        layout::SECONDARY_SURFACE_BG_ALPHA,
    )
}

fn secondary_surface_hover_color() -> Color32 {
    Color32::from_rgba_unmultiplied(
        layout::SECONDARY_SURFACE_BG_RED,
        layout::SECONDARY_SURFACE_BG_GREEN,
        layout::SECONDARY_SURFACE_BG_BLUE,
        layout::SECONDARY_SURFACE_HOVER_ALPHA,
    )
}

fn paint_window_background(ui: &egui::Ui, rect: Rect) {
    // 整个客户区使用纯色填满；窗口最终外形由 Win32 SetWindowRgn 的直角切口裁剪。
    // 不再叠加 egui 圆角或 DWM 非客户区边框。
    ui.painter()
        .rect_filled(rect, 0.0, Color32::from_rgb(43, 43, 43));
}

const BORDER: Color32 = Color32::from_rgb(73, 75, 84);
const TEXT_MAIN: Color32 = Color32::from_rgb(242, 243, 245);
const TEXT_MUTED: Color32 = Color32::from_rgb(160, 166, 176);
const BLUE: Color32 = Color32::from_rgb(0, 39, 255);
const BLUE_BRIGHT: Color32 = Color32::from_rgb(61, 116, 255);
const ORANGE: Color32 = Color32::from_rgb(204, 49, 0);
fn install_button_bg() -> Color32 {
    Color32::from_rgba_unmultiplied(16, 52, 255, 153)
}

fn launch_button_bg() -> Color32 {
    Color32::from_rgba_unmultiplied(0, 221, 112, 153)
}
const GREEN: Color32 = Color32::from_rgb(0, 221, 112);
const NOT_INSTALLED: Color32 = Color32::from_rgb(255, 61, 0);
const MODEL_MODE_STATUS: Color32 = Color32::from_rgb(25, 93, 255);
const SUBTITLE_LINK: Color32 = Color32::from_rgb(25, 93, 255);
const SUBTITLE_LINK_HOVER: Color32 = Color32::from_rgb(155, 255, 0);

fn default_status() -> &'static str {
    i18n::tr(
        "请输入接口地址和 API KEY、然后选择客户端。",
        "Enter gateway URL & API key, choose a client.",
    )
}

fn render_header(ui: &mut egui::Ui, assets: &GuiAssets, header_rect: Rect) {
    let content_rect = Rect::from_min_max(
        header_rect.min + Vec2::new(layout::HEADER_PADDING_LEFT, layout::HEADER_PADDING_TOP),
        header_rect.max - Vec2::new(layout::HEADER_PADDING_RIGHT, layout::HEADER_PADDING_BOTTOM),
    );
    let content_center_y = content_rect.center().y;

    let logo_rect = Rect::from_center_size(
        egui::pos2(
            content_rect.left() + layout::HEADER_LOGO_WIDTH / 2.0,
            content_center_y + layout::HEADER_LOGO_OFFSET_Y,
        ),
        Vec2::new(layout::HEADER_LOGO_WIDTH, layout::HEADER_LOGO_HEIGHT),
    );
    paint_svg_texture(
        ui,
        &assets.logo,
        logo_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    let logo_response = ui.interact(logo_rect, ui.id().with("header-logo-link"), Sense::click());
    if logo_response.hovered() {
        ui.output_mut(|output| output.cursor_icon = egui::CursorIcon::PointingHand);
    }
    if logo_response.clicked() {
        open_external_link(ui.ctx(), LOGO_URL);
    }

    let dot_size = Vec2::splat(layout::HEADER_TRAFFIC_DOT_SIZE);
    let dot_step = layout::HEADER_TRAFFIC_DOT_SIZE + layout::HEADER_TRAFFIC_DOT_GAP;
    let right_dot = content_rect.right() - layout::HEADER_TRAFFIC_DOT_SIZE / 2.0;
    // 从左到右固定为：黄（最小化）、绿（固定尺寸）、红（关闭）。
    let red_rect = Rect::from_center_size(egui::pos2(right_dot, content_center_y), dot_size);
    let green_rect =
        Rect::from_center_size(egui::pos2(right_dot - dot_step, content_center_y), dot_size);
    let yellow_rect = Rect::from_center_size(
        egui::pos2(right_dot - dot_step * 2.0, content_center_y),
        dot_size,
    );

    // 标题两行使用显式行高，整个文字块在标题栏内容区中严格垂直居中。
    let title_left = logo_rect.right() + layout::HEADER_LOGO_TITLE_GAP;
    let title_block_top = content_center_y - layout::HEADER_TITLE_BLOCK_HEIGHT / 2.0
        + layout::HEADER_TITLE_BLOCK_OFFSET_Y;
    let main_center_y = title_block_top + layout::HEADER_TITLE_MAIN_LINE_HEIGHT / 2.0;
    let subtitle_center_y = title_block_top
        + layout::HEADER_TITLE_MAIN_LINE_HEIGHT
        + layout::HEADER_TITLE_LINES_GAP
        + layout::HEADER_TITLE_SUBTITLE_LINE_HEIGHT / 2.0;
    ui.painter().text(
        egui::pos2(title_left, main_center_y),
        Align2::LEFT_CENTER,
        WINDOW_TITLE,
        FontId::proportional(15.0),
        TEXT_MAIN,
    );
    let chinese = i18n::language() == i18n::Language::ZhCn;
    let subtitle_font = FontId::proportional(11.0);
    let subtitle_link_font = FontId::proportional(if chinese { 12.0 } else { 11.0 });
    let subtitle_prefix = if chinese {
        "化繁为简 • 获取密钥 : "
    } else {
        "Simplify Everything • "
    };
    let subtitle_link = if chinese { "znnz.net" } else { "Get api key" };
    let subtitle_separator = " • ";
    let subtitle_prefix_galley = ui.painter().layout_no_wrap(
        subtitle_prefix.to_owned(),
        subtitle_font.clone(),
        TEXT_MUTED,
    );
    let subtitle_link_galley = ui.painter().layout_no_wrap(
        subtitle_link.to_owned(),
        subtitle_link_font.clone(),
        SUBTITLE_LINK,
    );
    let subtitle_prefix_size = subtitle_prefix_galley.size();
    let subtitle_link_size = subtitle_link_galley.size();
    let subtitle_separator_galley = ui.painter().layout_no_wrap(
        subtitle_separator.to_owned(),
        subtitle_font.clone(),
        TEXT_MUTED,
    );
    let subtitle_separator_size = subtitle_separator_galley.size();
    ui.painter().galley(
        egui::pos2(title_left, subtitle_center_y - subtitle_prefix_size.y / 2.0),
        subtitle_prefix_galley,
        TEXT_MUTED,
    );
    let subtitle_link_text_rect = Rect::from_min_size(
        egui::pos2(
            title_left + subtitle_prefix_size.x,
            subtitle_center_y - subtitle_link_size.y / 2.0,
        ),
        subtitle_link_size,
    );
    let subtitle_link_rect = Rect::from_min_max(
        subtitle_link_text_rect.min,
        egui::pos2(
            subtitle_link_text_rect.right(),
            subtitle_link_text_rect.bottom() + 3.0,
        ),
    );
    let subtitle_link_response = ui.interact(
        subtitle_link_rect,
        ui.id().with("header-subtitle-link"),
        Sense::click(),
    );
    let subtitle_link_color = if subtitle_link_response.hovered() {
        SUBTITLE_LINK_HOVER
    } else {
        SUBTITLE_LINK
    };
    let subtitle_link_paint_galley = ui.painter().layout_no_wrap(
        subtitle_link.to_owned(),
        subtitle_link_font,
        subtitle_link_color,
    );
    ui.painter().galley(
        subtitle_link_text_rect.min,
        subtitle_link_paint_galley,
        subtitle_link_color,
    );
    paint_wavy_underline(
        ui.painter(),
        subtitle_link_text_rect.left(),
        subtitle_link_text_rect.right(),
        subtitle_link_rect.bottom() - 0.5,
        SUBTITLE_LINK,
    );
    if subtitle_link_response.hovered() {
        ui.output_mut(|output| output.cursor_icon = egui::CursorIcon::PointingHand);
    }
    if subtitle_link_response.clicked() {
        open_external_link(ui.ctx(), GATEWAY_INFO_URL);
    }

    let first_separator_pos = egui::pos2(
        subtitle_link_rect.right(),
        subtitle_center_y - subtitle_separator_size.y / 2.0,
    );
    ui.painter().galley(
        first_separator_pos,
        subtitle_separator_galley.clone(),
        TEXT_MUTED,
    );
    let update_rect = Rect::from_center_size(
        egui::pos2(
            first_separator_pos.x
                + subtitle_separator_size.x
                + layout::HEADER_SUBTITLE_ICON_SIZE / 2.0,
            subtitle_center_y + layout::HEADER_SUBTITLE_ICON_OFFSET_Y,
        ),
        Vec2::splat(layout::HEADER_SUBTITLE_ICON_SIZE),
    );
    paint_svg_texture(
        ui,
        &assets.update,
        update_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    let update_response = ui.interact(
        update_rect,
        ui.id().with("header-update-link"),
        Sense::click(),
    );
    if update_response.hovered() {
        ui.output_mut(|output| output.cursor_icon = egui::CursorIcon::PointingHand);
    }
    if update_response.clicked() {
        open_external_link(ui.ctx(), UPDATE_URL);
    }
    show_hover_tooltip(&update_response, i18n::tr("检查更新", "Check for updates"));

    let second_separator_pos = egui::pos2(
        update_rect.right(),
        subtitle_center_y - subtitle_separator_size.y / 2.0,
    );
    ui.painter()
        .galley(second_separator_pos, subtitle_separator_galley, TEXT_MUTED);
    let github_rect = Rect::from_center_size(
        egui::pos2(
            second_separator_pos.x
                + subtitle_separator_size.x
                + layout::HEADER_SUBTITLE_ICON_SIZE / 2.0,
            subtitle_center_y + layout::HEADER_SUBTITLE_ICON_OFFSET_Y,
        ),
        Vec2::splat(layout::HEADER_SUBTITLE_ICON_SIZE),
    );
    paint_svg_texture(
        ui,
        &assets.github,
        github_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    let github_response = ui.interact(
        github_rect,
        ui.id().with("header-github-link"),
        Sense::click(),
    );
    if github_response.hovered() {
        ui.output_mut(|output| output.cursor_icon = egui::CursorIcon::PointingHand);
    }
    if github_response.clicked() {
        open_external_link(ui.ctx(), GITHUB_URL);
    }
    show_hover_tooltip(&github_response, "GitHub");

    let _ = traffic_dot_at(
        ui,
        green_rect,
        Color32::from_rgb(40, 200, 64),
        "window-size",
        &assets.window_control_glyphs,
        2,
    );
    let minimize_clicked = traffic_dot_at(
        ui,
        yellow_rect,
        Color32::from_rgb(254, 188, 46),
        "window-minimize",
        &assets.window_control_glyphs,
        1,
    );
    let close_clicked = traffic_dot_at(
        ui,
        red_rect,
        Color32::from_rgb(255, 95, 87),
        "window-close",
        &assets.window_control_glyphs,
        0,
    );

    let drag_right = yellow_rect.left() - layout::HEADER_LOGO_TITLE_GAP;
    let drag_rects = [
        // Main title and the empty band above the subtitle.
        Rect::from_min_max(
            egui::pos2(title_left, header_rect.top()),
            egui::pos2(drag_right, subtitle_link_rect.top()),
        ),
        // Subtitle text before the link.
        Rect::from_min_max(
            egui::pos2(title_left, subtitle_link_rect.top()),
            egui::pos2(subtitle_link_rect.left(), subtitle_link_rect.bottom()),
        ),
        // Empty space to the right of all subtitle links.
        Rect::from_min_max(
            egui::pos2(github_rect.right(), subtitle_link_rect.top()),
            egui::pos2(drag_right, subtitle_link_rect.bottom()),
        ),
        // Empty band below the subtitle.
        Rect::from_min_max(
            egui::pos2(title_left, subtitle_link_rect.bottom()),
            egui::pos2(drag_right, header_rect.bottom()),
        ),
    ];
    let start_drag = drag_rects.into_iter().enumerate().any(|(index, rect)| {
        rect.is_positive()
            && ui
                .interact(
                    rect,
                    ui.id().with(("window-drag", index)),
                    Sense::click_and_drag(),
                )
                .dragged()
    });
    if start_drag {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if minimize_clicked {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
    }
    if close_clicked {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

fn paint_wavy_underline(
    painter: &egui::Painter,
    left: f32,
    right: f32,
    bottom: f32,
    color: Color32,
) {
    let points = wavy_underline_points(left, right, bottom);
    if !points.is_empty() {
        painter.add(egui::Shape::line(points, Stroke::new(0.8, color)));
    }
}

fn wavy_underline_points(left: f32, right: f32, bottom: f32) -> Vec<egui::Pos2> {
    let width = right - left;
    if width <= 0.0 {
        return Vec::new();
    }

    // CSS 参考图形的半波长约为 2.375px。使用偶数段并重新均分宽度，
    // 保证左右端点精确落在下方，最后一段不会向上折。
    let mut segment_count = (width / 2.375).round().max(2.0) as usize;
    if !segment_count.is_multiple_of(2) {
        segment_count += 1;
    }
    let half_step = width / segment_count as f32;
    let top = bottom - 3.0;
    let mut points = Vec::with_capacity(segment_count + 1);
    for index in 0..=segment_count {
        let x = if index == segment_count {
            right
        } else {
            left + index as f32 * half_step
        };
        points.push(egui::pos2(x, if index % 2 == 0 { bottom } else { top }));
    }
    points
}

fn render_divider(ui: &mut egui::Ui, divider_rect: Rect) {
    if layout::DIVIDER_HEIGHT <= 0.0 {
        return;
    }

    // 分割线的布局占位仍由 gui_layout.rs 控制，但实际绘制厚度固定为 1 个物理像素。
    // 这样在 125%、150% 等 Windows DPI 下不会被放大成 1.25px、1.5px 的粗线。
    let pixels_per_point = ui.ctx().pixels_per_point().max(0.01);
    let one_physical_pixel = 1.0 / pixels_per_point;
    let center_y = divider_rect.center().y;
    let line_rect = Rect::from_min_max(
        egui::pos2(
            divider_rect.left() + layout::DIVIDER_PADDING_LEFT,
            center_y - one_physical_pixel * 0.5,
        ),
        egui::pos2(
            divider_rect.right() - layout::DIVIDER_PADDING_RIGHT,
            center_y + one_physical_pixel * 0.5,
        ),
    );
    let line_rect = align_rect_to_physical_pixels(ui.ctx(), line_rect);

    ui.painter().rect_filled(
        line_rect,
        0.0,
        Color32::from_rgba_premultiplied(0, 0, 0, 106),
    );
}

fn traffic_dot_at(
    ui: &mut egui::Ui,
    rect: Rect,
    color: Color32,
    id_salt: &str,
    glyphs: &TextureHandle,
    glyph_index: usize,
) -> bool {
    let response = ui.interact(rect, ui.id().with(id_salt), Sense::click());
    ui.painter()
        .circle_filled(rect.center(), rect.width().min(rect.height()) / 2.0, color);

    // 保持上一版扁平圆点背景；悬停时只叠加 macOS 风格符号，不显示 Tooltip。
    if response.hovered() {
        let left = glyph_index as f32 / 3.0;
        let right = (glyph_index + 1) as f32 / 3.0;
        paint_svg_texture(
            ui,
            glyphs,
            rect,
            Rect::from_min_max(egui::pos2(left, 0.0), egui::pos2(right, 1.0)),
            Color32::WHITE,
        );
    }
    response.clicked()
}

#[allow(clippy::too_many_arguments)]
fn render_input_row_at(
    ui: &mut egui::Ui,
    row_rect: Rect,
    leading_icon: &TextureHandle,
    value: &mut String,
    hint: &str,
    password: bool,
    visibility: Option<(&mut bool, &TextureHandle, &TextureHandle)>,
    action_icon: &TextureHandle,
    action_icon_size: Vec2,
    action_tip: &str,
    enabled: bool,
) -> (bool, bool) {
    let action_rect = Rect::from_min_size(
        egui::pos2(row_rect.right() - layout::INPUT_ACTION_SIZE, row_rect.top()),
        Vec2::splat(layout::INPUT_ACTION_SIZE),
    );
    let input_rect = Rect::from_min_max(
        row_rect.min,
        egui::pos2(
            action_rect.left() - layout::INPUT_ACTION_HORIZONTAL_GAP,
            row_rect.bottom(),
        ),
    );

    ui.painter()
        .rect_filled(input_rect, 10.0, input_surface_bg_color());
    paint_svg_texture(
        ui,
        leading_icon,
        Rect::from_center_size(
            egui::pos2(input_rect.left() + 14.0, input_rect.center().y),
            Vec2::splat(16.0),
        ),
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        if enabled {
            Color32::WHITE
        } else {
            Color32::from_gray(92)
        },
    );

    let has_visibility = visibility.is_some();
    let edit_rect = Rect::from_min_max(
        egui::pos2(input_rect.left() + 26.0, input_rect.top() + 2.0),
        egui::pos2(
            input_rect.right() - if has_visibility { 32.0 } else { 6.0 },
            input_rect.bottom() - 2.0,
        ),
    );
    let edit_response = ui.put(
        edit_rect,
        egui::TextEdit::singleline(value)
            .password(password)
            .hint_text(RichText::new(hint).color(input_hint_text_color()))
            .font(FontId::proportional(14.0))
            .vertical_align(egui::Align::Center)
            .margin(egui::Margin::ZERO)
            .frame(egui::Frame::NONE)
            .text_color(TEXT_MAIN)
            .interactive(enabled),
    );

    if let Some((show, show_icon, hide_icon)) = visibility {
        let visibility_rect = Rect::from_center_size(
            egui::pos2(input_rect.right() - 17.0, input_rect.center().y),
            Vec2::splat(26.0),
        );
        render_key_visibility_button_at(ui, visibility_rect, show, show_icon, hide_icon, enabled);
    }

    let action_clicked = render_icon_button_at(
        ui,
        action_rect,
        action_icon,
        action_icon_size,
        enabled,
        action_tip,
    );
    (action_clicked, edit_response.changed())
}

fn render_key_visibility_button_at(
    ui: &mut egui::Ui,
    rect: Rect,
    show: &mut bool,
    show_icon: &TextureHandle,
    hide_icon: &TextureHandle,
    enabled: bool,
) {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, ui.id().with("key-visibility"), sense);
    let icon = if *show { hide_icon } else { show_icon };
    paint_svg_texture(
        ui,
        icon,
        Rect::from_center_size(rect.center(), Vec2::splat(16.0)),
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        if enabled {
            Color32::WHITE
        } else {
            Color32::from_gray(92)
        },
    );
    show_hover_tooltip(
        &response,
        if *show {
            i18n::tr("隐藏", "Hide")
        } else {
            i18n::tr("显示", "Show")
        },
    );
    if response.clicked() {
        *show = !*show;
    }
}

fn render_icon_button_at(
    ui: &mut egui::Ui,
    rect: Rect,
    icon: &TextureHandle,
    icon_size: Vec2,
    enabled: bool,
    tooltip: &str,
) -> bool {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, ui.id().with(("input-action", tooltip)), sense);
    let fill = if response.hovered() && enabled {
        ORANGE
    } else {
        input_surface_bg_color()
    };
    ui.painter().rect_filled(rect, 10.0, fill);
    paint_svg_texture(
        ui,
        icon,
        Rect::from_center_size(rect.center(), icon_size),
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        if enabled {
            Color32::WHITE
        } else {
            Color32::from_gray(92)
        },
    );
    show_hover_tooltip(&response, tooltip);
    response.clicked()
}

#[derive(Clone, Copy)]
struct CascadeCurve {
    start: (f32, f32),
    control_1: (f32, f32),
    control_2: (f32, f32),
    end: (f32, f32),
}

const CASCADE_VIEW_WIDTH: f32 = 920.0;
const CASCADE_VIEW_HEIGHT: f32 = 127.384_61;
const CASCADE_CURVES: [CascadeCurve; 4] = [
    CascadeCurve {
        start: (280.0, 5.0),
        control_1: (280.0, 66.627),
        control_2: (22.0, 63.692),
        end: (22.0, 122.385),
    },
    CascadeCurve {
        start: (400.0, 5.0),
        control_1: (400.0, 72.496),
        control_2: (314.0, 72.496),
        end: (314.0, 122.385),
    },
    CascadeCurve {
        start: (520.0, 5.0),
        control_1: (520.0, 72.496),
        control_2: (606.0, 72.496),
        end: (606.0, 122.385),
    },
    CascadeCurve {
        start: (640.0, 5.0),
        control_1: (640.0, 66.627),
        control_2: (898.0, 63.692),
        end: (898.0, 122.385),
    },
];
const CASCADE_GROUP_TIMES: [&[f32]; 4] = [
    &[0.0, 0.22, 0.24, 0.30, 0.32, 0.58, 0.60, 0.68, 0.70, 1.0],
    &[
        0.0, 0.08, 0.10, 0.36, 0.38, 0.46, 0.48, 0.74, 0.76, 0.84, 0.86, 1.0,
    ],
    &[0.0, 0.14, 0.16, 0.28, 0.30, 0.64, 0.66, 0.78, 0.80, 1.0],
    &[
        0.0, 0.12, 0.14, 0.20, 0.22, 0.42, 0.44, 0.52, 0.54, 0.72, 0.74, 0.82, 0.84, 1.0,
    ],
];
const CASCADE_GROUP_VALUES: [&[f32]; 4] = [
    &[1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0],
    &[0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0],
    &[1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0],
    &[
        1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 1.0, 1.0,
    ],
];
const CASCADE_DOT_DURATIONS: [f32; 4] = [1.48, 1.62, 1.40, 1.56];
const CASCADE_DOT_BEGINS: [[f32; 3]; 4] = [
    [0.00, 0.48, 0.96],
    [0.22, 0.74, 1.26],
    [0.35, 0.80, 1.20],
    [0.10, 0.62, 1.14],
];

/// egui/resvg 只会将 SVG 光栅化成静态纹理，不执行 SVG SMIL 动画。
/// 这里逐帧复刻用户当前 cascade-flow.svg：四条曲线、24 秒随机交错节奏和各自的圆点速度。
fn paint_cascade_flow(ui: &egui::Ui, rect: Rect) {
    let scale = (rect.width() / CASCADE_VIEW_WIDTH).min(rect.height() / CASCADE_VIEW_HEIGHT);
    let source_size = Vec2::new(CASCADE_VIEW_WIDTH * scale, CASCADE_VIEW_HEIGHT * scale);
    let origin = rect.center() - source_size / 2.0;
    let paths = CASCADE_CURVES.map(|curve| sample_cascade_curve(curve, origin, scale));
    let painter = ui.painter_at(rect);

    let stroke = Stroke::new(
        (3.0 * scale).max(0.85),
        Color32::from_rgba_unmultiplied(0, 0, 0, 128),
    );
    for path in &paths {
        painter.add(egui::Shape::line(path.clone(), stroke));
    }

    let elapsed = ui.input(|input| input.time) as f32;
    let cycle = (elapsed % 24.0) / 24.0;
    for (index, path) in paths.iter().enumerate() {
        let group_opacity = cascade_group_opacity(index, cycle);
        if group_opacity <= 0.0 {
            continue;
        }
        let duration = CASCADE_DOT_DURATIONS[index];
        for begin in CASCADE_DOT_BEGINS[index] {
            if elapsed < begin {
                continue;
            }
            let motion = ((elapsed - begin) % duration) / duration;
            let dot_opacity = cascade_dot_opacity(motion) * group_opacity;
            if dot_opacity <= 0.0 {
                continue;
            }
            let alpha = (dot_opacity * 255.0).round().clamp(0.0, 255.0) as u8;
            painter.circle_filled(
                point_along_polyline(path, motion),
                6.0 * scale,
                Color32::from_rgba_unmultiplied(0, 221, 112, alpha),
            );
        }
    }

    // 约 60 FPS，仅重绘这个很小的原生动画，不再反复解析或光栅化 SVG。
    ui.ctx().request_repaint_after(Duration::from_millis(16));
}

fn sample_cascade_curve(curve: CascadeCurve, origin: egui::Pos2, scale: f32) -> Vec<egui::Pos2> {
    const SEGMENTS: usize = 128;
    (0..=SEGMENTS)
        .map(|index| {
            let t = index as f32 / SEGMENTS as f32;
            let one_minus_t = 1.0 - t;
            let a = one_minus_t * one_minus_t * one_minus_t;
            let b = 3.0 * one_minus_t * one_minus_t * t;
            let c = 3.0 * one_minus_t * t * t;
            let d = t * t * t;
            let x =
                a * curve.start.0 + b * curve.control_1.0 + c * curve.control_2.0 + d * curve.end.0;
            let y =
                a * curve.start.1 + b * curve.control_1.1 + c * curve.control_2.1 + d * curve.end.1;
            origin + egui::vec2(x * scale, y * scale)
        })
        .collect()
}

fn point_along_polyline(path: &[egui::Pos2], progress: f32) -> egui::Pos2 {
    let total_length: f32 = path.windows(2).map(|pair| pair[0].distance(pair[1])).sum();
    if total_length <= f32::EPSILON {
        return path.first().copied().unwrap_or_default();
    }

    let target_length = total_length * progress.clamp(0.0, 1.0);
    let mut traversed = 0.0;
    for pair in path.windows(2) {
        let segment_length = pair[0].distance(pair[1]);
        if traversed + segment_length >= target_length {
            let local = if segment_length <= f32::EPSILON {
                0.0
            } else {
                (target_length - traversed) / segment_length
            };
            return pair[0] + (pair[1] - pair[0]) * local;
        }
        traversed += segment_length;
    }
    path.last().copied().unwrap_or_default()
}

fn cascade_group_opacity(index: usize, cycle: f32) -> f32 {
    let Some(times) = CASCADE_GROUP_TIMES.get(index) else {
        return 0.0;
    };
    let values = CASCADE_GROUP_VALUES[index];
    interpolate_keyframes(cycle.rem_euclid(1.0), times, values)
}

fn cascade_dot_opacity(progress: f32) -> f32 {
    interpolate_keyframes(
        progress.rem_euclid(1.0),
        &[0.0, 0.12, 0.72, 1.0],
        &[0.0, 1.0, 1.0, 0.0],
    )
}

fn interpolate_keyframes(progress: f32, times: &[f32], values: &[f32]) -> f32 {
    debug_assert_eq!(times.len(), values.len());
    if times.is_empty() || values.is_empty() {
        return 0.0;
    }
    if progress <= times[0] {
        return values[0];
    }
    for index in 0..times.len().saturating_sub(1) {
        if progress <= times[index + 1] {
            let span = times[index + 1] - times[index];
            if span <= f32::EPSILON {
                return values[index + 1];
            }
            let amount = ((progress - times[index]) / span).clamp(0.0, 1.0);
            return values[index] + (values[index + 1] - values[index]) * amount;
        }
    }
    *values.last().unwrap_or(&0.0)
}
fn render_client_selector_at(ui: &mut egui::Ui, panel_rect: Rect, app: &mut LauncherApp) {
    let cards_rect = panel_rect.shrink(layout::CLIENT_PANEL_PADDING);
    let painter = ui.painter();

    if app.client_selected {
        // 选中卡片对应位置在区块 2-1 底部显示洋红色缺口标记。
        let selected_index = ClientTarget::ALL
            .into_iter()
            .position(|target| target == app.selected)
            .unwrap_or(0);
        let selected_center_x = cards_rect.left()
            + layout::CLIENT_CARD_WIDTH / 2.0
            + selected_index as f32
                * (layout::CLIENT_CARD_WIDTH + layout::CLIENT_CARD_HORIZONTAL_GAP);
        let notch_rect = Rect::from_min_size(
            egui::pos2(
                selected_center_x - layout::CLIENT_SELECTED_NOTCH_WIDTH / 2.0,
                panel_rect.bottom() - layout::CLIENT_SELECTED_NOTCH_HEIGHT,
            ),
            Vec2::new(
                layout::CLIENT_SELECTED_NOTCH_WIDTH,
                layout::CLIENT_SELECTED_NOTCH_HEIGHT,
            ),
        );

        // 使用三个裁剪区域绘制同一个圆角面板，中间区域由洋红色缺口标记填充。
        painter
            .with_clip_rect(Rect::from_min_max(
                panel_rect.min,
                egui::pos2(panel_rect.right(), notch_rect.top()),
            ))
            .rect_filled(
                panel_rect,
                layout::CLIENT_PANEL_CORNER_RADIUS,
                secondary_surface_bg_color(),
            );
        painter
            .with_clip_rect(Rect::from_min_max(
                egui::pos2(panel_rect.left(), notch_rect.top()),
                egui::pos2(notch_rect.left(), panel_rect.bottom()),
            ))
            .rect_filled(
                panel_rect,
                layout::CLIENT_PANEL_CORNER_RADIUS,
                secondary_surface_bg_color(),
            );
        painter
            .with_clip_rect(Rect::from_min_max(
                egui::pos2(notch_rect.right(), notch_rect.top()),
                panel_rect.max,
            ))
            .rect_filled(
                panel_rect,
                layout::CLIENT_PANEL_CORNER_RADIUS,
                secondary_surface_bg_color(),
            );
        painter.rect_filled(notch_rect, 0.0, Color32::from_rgb(255, 0, 87));
    } else {
        // 启动器刚打开时不默认选中任何客户端，也不绘制底部缺口。
        painter.rect_filled(
            panel_rect,
            layout::CLIENT_PANEL_CORNER_RADIUS,
            secondary_surface_bg_color(),
        );
    }

    let card_y = cards_rect.center().y;
    for (index, target) in ClientTarget::ALL.into_iter().enumerate() {
        let card_rect = Rect::from_center_size(
            egui::pos2(
                cards_rect.left()
                    + layout::CLIENT_CARD_WIDTH / 2.0
                    + index as f32
                        * (layout::CLIENT_CARD_WIDTH + layout::CLIENT_CARD_HORIZONTAL_GAP),
                card_y,
            ),
            Vec2::new(layout::CLIENT_CARD_WIDTH, layout::CLIENT_CARD_HEIGHT),
        );
        let clicked = render_client_card_at(
            ui,
            card_rect,
            target,
            app.client_selected && app.selected == target,
            app.client_is_running(target),
            app.client_is_installed(target),
            &app.assets,
        );
        if clicked {
            let running = app.client_is_running(target);
            let installed = app.client_is_installed(target);
            app.selected = target;
            app.client_selected = true;
            app.status =
                selected_client_status(target, app.model_list_mode(target), installed, running);
            app.status_is_error = false;
        }
    }
}
fn render_client_card_at(
    ui: &mut egui::Ui,
    rect: Rect,
    target: ClientTarget,
    selected: bool,
    running: bool,
    installed: bool,
    assets: &GuiAssets,
) -> bool {
    let size = rect.size();
    let response = ui.interact(
        rect,
        ui.id().with(("client-card", target.id())),
        Sense::click(),
    );
    let background = assets.client_card_background(selected, running, response.hovered());
    paint_svg_texture(
        ui,
        background,
        rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );

    let icon = assets.client_icon(target);
    let badge = assets.client_badge(target);
    let icon_rect = Rect::from_center_size(
        rect.center() + egui::vec2(0.0, -2.0),
        Vec2::splat(size.x.min(size.y) * 0.60),
    );
    paint_svg_texture(
        ui,
        icon,
        icon_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );

    let badge_rect = Rect::from_min_size(
        rect.left_bottom() + egui::vec2(6.0, -27.0),
        Vec2::splat(24.0),
    );
    paint_svg_texture(
        ui,
        badge,
        badge_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );

    let dot_center = rect.right_bottom() + egui::vec2(-5.0, -7.0);
    if running {
        // 复刻 CSS box-shadow 脉冲：从灯芯边缘向外扩散并逐渐淡出。
        let elapsed = ui.input(|input| input.time) as f32;
        let phase = (elapsed % layout::CLIENT_STATUS_DOT_PULSE_PERIOD)
            / layout::CLIENT_STATUS_DOT_PULSE_PERIOD;
        let (spread, alpha) = if phase <= 0.7 {
            let progress = phase / 0.7;
            (
                layout::CLIENT_STATUS_DOT_PULSE_RADIUS * progress,
                (153.0 * (1.0 - progress)).round() as u8,
            )
        } else {
            (layout::CLIENT_STATUS_DOT_PULSE_RADIUS, 0)
        };
        if alpha > 0 {
            let pulse_color = Color32::from_rgba_unmultiplied(98, 197, 85, alpha);
            // CSS 无模糊 box-shadow 的 spread 是从灯芯边缘向外填满的环带。
            // 先画扩大中的实心阴影，再在下方覆盖灯芯，即可让可见环宽随 spread 增长。
            ui.painter()
                .circle_filled(dot_center, 5.5 + spread, pulse_color);
        }
        // 动画期间持续重绘，保证脉冲不依赖运行任务的轮询间隔。
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
    ui.painter().circle_filled(
        dot_center,
        5.5,
        if running {
            GREEN
        } else {
            Color32::from_rgb(91, 97, 112)
        },
    );
    ui.painter().circle_stroke(
        dot_center,
        5.5,
        Stroke::new(2.0, Color32::from_rgba_unmultiplied(10, 12, 16, 100)),
    );

    show_client_card_tooltip(&response, target, installed, running);
    response.clicked()
}

fn render_model_source_and_launch_at(ui: &mut egui::Ui, row_rect: Rect, app: &mut LauncherApp) {
    let center_y = row_rect.center().y;

    let target = app.selected;
    let has_selected_client = app.client_selected;
    let selected_mode = app.model_list_mode(target);
    let locked = !has_selected_client || app.client_is_running(target);

    // 区块 2-2 左侧的两个模型列表按钮按内容宽度占位，类似 CSS 的
    // height + padding；不会再平均拉伸填满整行。
    let built_in_label = model_mode_label(ModelListMode::BuiltIn);
    let gateway_label = model_mode_label(ModelListMode::Gateway);
    let built_in_width = choice_chip_width(ui, built_in_label);
    let gateway_width = choice_chip_width(ui, gateway_label);

    let built_in_rect = Rect::from_min_size(
        egui::pos2(row_rect.left(), center_y - layout::MODEL_CHIP_HEIGHT / 2.0),
        Vec2::new(built_in_width, layout::MODEL_CHIP_HEIGHT),
    );
    let gateway_rect = Rect::from_min_size(
        egui::pos2(
            built_in_rect.right() + layout::MODEL_CHIP_HORIZONTAL_GAP,
            center_y - layout::MODEL_CHIP_HEIGHT / 2.0,
        ),
        Vec2::new(gateway_width, layout::MODEL_CHIP_HEIGHT),
    );

    // 启动按钮贴着区块 2-2 右边缘；安装按钮在其左侧保留可配置间距。
    let launch_rect = Rect::from_center_size(
        egui::pos2(row_rect.right() - layout::MODEL_LAUNCH_SIZE / 2.0, center_y),
        Vec2::splat(layout::MODEL_LAUNCH_SIZE),
    );
    let install_rect = Rect::from_center_size(
        egui::pos2(
            launch_rect.left()
                - layout::MODEL_INSTALL_LAUNCH_GAP
                - layout::MODEL_INSTALL_SIZE / 2.0,
            center_y,
        ),
        Vec2::splat(layout::MODEL_INSTALL_SIZE),
    );

    if choice_chip_at(
        ui,
        built_in_rect,
        built_in_label,
        has_selected_client && selected_mode == ModelListMode::BuiltIn,
        !locked,
        &app.assets.select,
    ) {
        app.set_model_list_mode(target, ModelListMode::BuiltIn);
        app.status = selected_client_status(
            target,
            ModelListMode::BuiltIn,
            app.client_is_installed(target),
            app.client_is_running(target),
        );
        app.status_is_error = false;
    }
    if choice_chip_at(
        ui,
        gateway_rect,
        gateway_label,
        has_selected_client && selected_mode == ModelListMode::Gateway,
        !locked,
        &app.assets.select,
    ) {
        app.set_model_list_mode(target, ModelListMode::Gateway);
        app.status = selected_client_status(
            target,
            ModelListMode::Gateway,
            app.client_is_installed(target),
            app.client_is_running(target),
        );
        app.status_is_error = false;
    }

    let selected_installed = has_selected_client && app.client_is_installed(target);
    let selected_installing = has_selected_client && app.is_running(TaskKind::Install(target));
    if render_install_button_at(
        ui,
        install_rect,
        &app.assets.install,
        &app.assets.installing,
        has_selected_client,
        has_selected_client && !selected_installed && !selected_installing,
        selected_installed,
        selected_installing,
    ) {
        app.start_selected_client_install();
    }

    let selected_running = has_selected_client && app.client_is_running(target);
    if render_launch_button_at(
        ui,
        launch_rect,
        &app.assets.start,
        has_selected_client && !selected_running,
    ) {
        app.start_selected_client();
    }
}

fn choice_chip_width(ui: &egui::Ui, label: &str) -> f32 {
    let text_width = ui
        .painter()
        .layout_no_wrap(label.to_owned(), FontId::proportional(11.5), Color32::WHITE)
        .size()
        .x;
    layout::MODEL_CHIP_PADDING_LEFT
        + layout::MODEL_CHECK_WIDTH
        + layout::MODEL_CHECK_TEXT_GAP
        + text_width
        + layout::MODEL_CHIP_PADDING_RIGHT
}

fn choice_chip_at(
    ui: &mut egui::Ui,
    rect: Rect,
    label: &str,
    selected: bool,
    enabled: bool,
    select_icon: &TextureHandle,
) -> bool {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, ui.id().with(("model-list-chip", label)), sense);
    let fill = if response.hovered() && enabled {
        secondary_surface_hover_color()
    } else {
        secondary_surface_bg_color()
    };
    ui.painter().rect_filled(rect, 10.0, fill);

    let box_rect = Rect::from_center_size(
        egui::pos2(
            rect.left() + layout::MODEL_CHIP_PADDING_LEFT + layout::MODEL_CHECK_WIDTH / 2.0,
            rect.center().y,
        ),
        Vec2::new(layout::MODEL_CHECK_WIDTH, layout::MODEL_CHECK_HEIGHT),
    );
    let check_background = if selected {
        Color32::from_rgb(0, 39, 255)
    } else {
        Color32::from_rgba_unmultiplied(255, 255, 255, 26)
    };
    ui.painter().rect_filled(
        box_rect,
        (layout::MODEL_CHECK_WIDTH.min(layout::MODEL_CHECK_HEIGHT) / 2.0).min(6.0),
        check_background,
    );
    if selected {
        let check_width = (layout::MODEL_CHECK_WIDTH - 4.0).max(1.0);
        let check_height = (layout::MODEL_CHECK_HEIGHT - 4.0).max(1.0);
        paint_svg_texture(
            ui,
            select_icon,
            Rect::from_center_size(box_rect.center(), Vec2::new(check_width, check_height)),
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    let text_color = if enabled {
        Color32::from_rgb(200, 206, 216)
    } else {
        Color32::from_gray(108)
    };
    let text_galley =
        ui.painter()
            .layout_no_wrap(label.to_owned(), FontId::proportional(11.5), text_color);
    let text_pos = egui::pos2(
        box_rect.right() + layout::MODEL_CHECK_TEXT_GAP,
        rect.center().y - text_galley.size().y / 2.0,
    );
    ui.painter().galley(text_pos, text_galley, text_color);
    response.clicked()
}

fn paint_rotated_texture(
    ui: &egui::Ui,
    texture: &TextureHandle,
    rect: Rect,
    angle: f32,
    tint: Color32,
) {
    let rect = align_rect_to_physical_pixels(ui.ctx(), rect);
    let center = rect.center();
    let half = rect.size() / 2.0;
    let (sin, cos) = angle.sin_cos();
    let corners = [
        (Vec2::new(-half.x, -half.y), egui::pos2(0.0, 0.0)),
        (Vec2::new(half.x, -half.y), egui::pos2(1.0, 0.0)),
        (Vec2::new(half.x, half.y), egui::pos2(1.0, 1.0)),
        (Vec2::new(-half.x, half.y), egui::pos2(0.0, 1.0)),
    ];
    let mut mesh = egui::Mesh::with_texture(texture.id());
    for (offset, uv) in corners {
        mesh.vertices.push(egui::epaint::Vertex {
            pos: egui::pos2(
                center.x + offset.x * cos - offset.y * sin,
                center.y + offset.x * sin + offset.y * cos,
            ),
            uv,
            color: tint,
        });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    ui.painter().add(egui::Shape::mesh(mesh));
}

#[allow(clippy::too_many_arguments)]
fn render_install_button_at(
    ui: &mut egui::Ui,
    rect: Rect,
    install_icon: &TextureHandle,
    installing_icon: &TextureHandle,
    has_selected_client: bool,
    enabled: bool,
    installed: bool,
    installing: bool,
) -> bool {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, ui.id().with("install-client"), sense);
    let fill = if response.hovered() && enabled {
        ORANGE
    } else if enabled {
        install_button_bg()
    } else {
        Color32::from_rgba_unmultiplied(255, 255, 255, 13)
    };
    ui.painter()
        .rect_filled(rect, layout::MODEL_ACTION_BUTTON_CORNER_RADIUS, fill);
    let icon_rect = Rect::from_center_size(
        rect.center(),
        Vec2::new(
            layout::MODEL_INSTALL_ICON_WIDTH,
            layout::MODEL_INSTALL_ICON_HEIGHT,
        ),
    );
    if installing {
        let angle = ui.input(|input| input.time as f32) * std::f32::consts::TAU;
        paint_rotated_texture(ui, installing_icon, icon_rect, angle, Color32::WHITE);
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    } else {
        paint_svg_texture(
            ui,
            install_icon,
            icon_rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            if enabled {
                Color32::WHITE
            } else {
                Color32::from_gray(128)
            },
        );
    }
    if !has_selected_client || (!installed && !installing) {
        show_hover_tooltip(&response, i18n::tr("安装客户端", "Install client"));
    } else if installed {
        show_colored_status_tooltip(&response, i18n::tr("已安装", "Installed"), GREEN);
    } else if installing {
        show_hover_tooltip(&response, i18n::tr("正在安装", "Installing"));
    } else {
        show_colored_status_tooltip(
            &response,
            i18n::tr("未安装", "Not installed"),
            NOT_INSTALLED,
        );
    }
    response.clicked()
}

fn render_launch_button_at(
    ui: &mut egui::Ui,
    rect: Rect,
    icon: &TextureHandle,
    enabled: bool,
) -> bool {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, ui.id().with("launch-client"), sense);
    let fill = if response.hovered() && enabled {
        ORANGE
    } else if enabled {
        launch_button_bg()
    } else {
        Color32::from_rgba_unmultiplied(255, 255, 255, 13)
    };
    ui.painter()
        .rect_filled(rect, layout::MODEL_ACTION_BUTTON_CORNER_RADIUS, fill);
    paint_svg_texture(
        ui,
        icon,
        Rect::from_center_size(
            rect.center(),
            Vec2::new(
                layout::MODEL_LAUNCH_ICON_WIDTH,
                layout::MODEL_LAUNCH_ICON_HEIGHT,
            ),
        ),
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        if enabled {
            Color32::WHITE
        } else {
            Color32::from_gray(128)
        },
    );
    show_hover_tooltip(&response, i18n::tr("配置 • 启动", "Configure • Launch"));
    response.clicked()
}

fn render_status_bar(ui: &mut egui::Ui, status_rect: Rect, app: &mut LauncherApp) {
    if app.status_is_error {
        // 状态栏警告背景贴合窗口底边，窗口外形统一由原生 Region 裁剪。
        ui.painter().rect_filled(
            status_rect,
            0.0,
            Color32::from_rgba_unmultiplied(104, 48, 29, 82),
        );
    }

    let icon = if app.status_is_error {
        &app.assets.prompt_warn
    } else {
        &app.assets.prompt
    };
    let color = if app.status_is_error {
        Color32::from_rgb(255, 179, 107)
    } else {
        Color32::from_rgb(200, 206, 216)
    };

    let inner_rect = Rect::from_min_max(
        status_rect.min + Vec2::new(layout::STATUS_PADDING_LEFT, layout::STATUS_PADDING_TOP),
        status_rect.max - Vec2::new(layout::STATUS_PADDING_RIGHT, layout::STATUS_PADDING_BOTTOM),
    );
    let content_center_y = inner_rect.center().y;
    let icon_rect = Rect::from_center_size(
        egui::pos2(
            inner_rect.left() + layout::STATUS_ICON_SIZE / 2.0,
            content_center_y,
        ),
        Vec2::splat(layout::STATUS_ICON_SIZE),
    );
    paint_svg_texture(
        ui,
        icon,
        icon_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );

    let log_rect = Rect::from_center_size(
        egui::pos2(
            inner_rect.right() - layout::STATUS_LOG_BUTTON_SIZE / 2.0,
            content_center_y,
        ),
        Vec2::splat(layout::STATUS_LOG_BUTTON_SIZE),
    );
    let text_rect = Rect::from_min_max(
        egui::pos2(
            icon_rect.right() + layout::STATUS_ICON_TEXT_HORIZONTAL_GAP,
            inner_rect.top(),
        ),
        egui::pos2(log_rect.left(), inner_rect.bottom()),
    );
    // 直接按矩形绘制，避免嵌套 Label 自带布局占位改变图标与文字之间的可见距离。
    // “已安装”和“运行中”显示绿色，“未安装”显示橙红色。
    paint_status_text(ui, text_rect, &app.status, color);

    if render_log_button_at(ui, log_rect, &app.assets.log) {
        let opening = !app.show_logs;
        app.show_logs = opening;
        if opening {
            app.align_log_window_on_open = true;
        }
    }
}

fn paint_status_text(ui: &egui::Ui, text_rect: Rect, text: &str, default_color: Color32) {
    let painter = ui.painter().with_clip_rect(text_rect);
    let regular_font = FontId::proportional(11.5);
    let bold_font = FontId::new(11.5, FontFamily::Name("status-bold".into()));
    let highlights = status_highlight_ranges(text);

    let mut cursor = 0;
    let mut x = text_rect.left();
    for (start, end, color) in highlights {
        paint_status_segment(
            &painter,
            &regular_font,
            &text[cursor..start],
            default_color,
            text_rect,
            &mut x,
        );
        let font = if color == MODEL_MODE_STATUS {
            &regular_font
        } else {
            &bold_font
        };
        paint_status_segment(&painter, font, &text[start..end], color, text_rect, &mut x);
        cursor = end;
    }
    paint_status_segment(
        &painter,
        &regular_font,
        &text[cursor..],
        default_color,
        text_rect,
        &mut x,
    );
}

fn status_highlight_ranges(text: &str) -> Vec<(usize, usize, Color32)> {
    let mut ranges = Vec::new();
    for (needle, color) in [
        ("已安装", GREEN),
        ("运行中", GREEN),
        ("未安装", NOT_INSTALLED),
        ("成功连接", GREEN),
        ("成功拉取", GREEN),
        ("Installed", GREEN),
        ("Running", GREEN),
        ("Not installed", NOT_INSTALLED),
        ("Connected", GREEN),
        ("Fetched", GREEN),
        ("默认模型列表", MODEL_MODE_STATUS),
        ("网关模型列表", MODEL_MODE_STATUS),
        ("Default models", MODEL_MODE_STATUS),
        ("Gateway models", MODEL_MODE_STATUS),
        // Also color logs produced by older builds while they are still open.
        ("连接成功", GREEN),
        ("拉取成功", GREEN),
    ] {
        let mut offset = 0;
        while let Some(found) = text[offset..].find(needle) {
            let start = offset + found;
            ranges.push((start, start + needle.len(), color));
            offset = start + needle.len();
        }
    }

    if let Some(marker) = text.find("个可见模型") {
        let bytes = text.as_bytes();
        let mut end = marker;
        while end > 0 && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && bytes[start - 1].is_ascii_digit() {
            start -= 1;
        }
        if start < end {
            ranges.push((start, end, GREEN));
        }
    }
    if let Some(marker) = text.find("visible models") {
        let bytes = text.as_bytes();
        let mut end = marker;
        while end > 0 && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && bytes[start - 1].is_ascii_digit() {
            start -= 1;
        }
        if start < end {
            ranges.push((start, end, GREEN));
        }
    }

    ranges.sort_by_key(|(start, end, _)| (*start, *end));
    let mut non_overlapping = Vec::with_capacity(ranges.len());
    for range in ranges {
        if non_overlapping
            .last()
            .is_none_or(|(_, previous_end, _)| *previous_end <= range.0)
        {
            non_overlapping.push(range);
        }
    }
    non_overlapping
}

fn paint_status_segment(
    painter: &egui::Painter,
    font: &FontId,
    text: &str,
    color: Color32,
    text_rect: Rect,
    x: &mut f32,
) {
    if text.is_empty() {
        return;
    }
    let galley = painter.layout_no_wrap(text.to_owned(), font.clone(), color);
    let position = egui::pos2(*x, text_rect.center().y - galley.size().y / 2.0);
    *x += galley.size().x;
    painter.galley(position, galley, color);
}

fn render_log_button_at(ui: &mut egui::Ui, rect: Rect, icon: &TextureHandle) -> bool {
    let response = ui.interact(rect, ui.id().with("status-log"), Sense::click());
    ui.painter().rect_filled(
        rect,
        12.0,
        if response.hovered() {
            ORANGE
        } else {
            secondary_surface_bg_color()
        },
    );
    paint_svg_texture(
        ui,
        icon,
        Rect::from_center_size(rect.center(), Vec2::splat(18.0)),
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    show_hover_tooltip(&response, i18n::tr("运行日志", "Runtime log"));
    response.clicked()
}

fn show_hover_tooltip(response: &egui::Response, text: impl Into<String>) {
    show_hover_tooltip_at(response, text, egui::RectAlign::TOP);
}

fn show_client_card_tooltip(
    response: &egui::Response,
    target: ClientTarget,
    installed: bool,
    running: bool,
) {
    let state = client_card_state_label(installed, running);
    let state_color = client_card_state_color(installed, running);
    show_hover_tooltip_contents(response, egui::RectAlign::TOP, |ui| {
        // Match the previous plain multiline tooltip: no extra vertical
        // line gap and no layout gap around the bullet separator.
        ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
        ui.label(
            RichText::new(target.title())
                .size(12.0)
                .color(Color32::WHITE),
        );
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(client_kind_label(target))
                    .size(12.0)
                    .color(Color32::WHITE),
            );
            ui.label(RichText::new(" • ").size(12.0).color(Color32::WHITE));
            ui.label(RichText::new(state).size(12.0).color(state_color));
        });
    });
}

fn open_external_link(ctx: &egui::Context, url: &str) {
    // eframe's OpenUrl output is not handled consistently by every native
    // renderer/window setup. Use the Windows shell directly, retaining the
    // egui path as a fallback for other desktop backends.
    if crate::platform::open_external_url(url).is_err() {
        ctx.open_url(egui::OpenUrl::new_tab(url));
    }
}

fn show_colored_status_tooltip(response: &egui::Response, text: &'static str, color: Color32) {
    show_hover_tooltip_contents(response, egui::RectAlign::TOP, |ui| {
        ui.add(egui::Label::new(RichText::new(text).size(12.0).color(color)).extend());
    });
}

fn show_hover_tooltip_at(
    response: &egui::Response,
    text: impl Into<String>,
    align: egui::RectAlign,
) {
    let text = text.into();
    show_hover_tooltip_contents(response, align, |ui| {
        ui.add(
            egui::Label::new(RichText::new(text).size(12.0).color(Color32::WHITE))
                .extend()
                .halign(egui::Align::Center),
        );
    });
}

fn show_hover_tooltip_contents(
    response: &egui::Response,
    align: egui::RectAlign,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    let mut tooltip = egui::Tooltip::for_enabled(response);
    tooltip.popup = tooltip
        .popup
        .align(align)
        .align_alternatives(&[])
        .gap(layout::TOOLTIP_GAP)
        .frame(
            egui::Frame::popup(response.ctx.global_style().as_ref())
                .fill(tooltip_background_color())
                .stroke(Stroke::NONE)
                .inner_margin(egui::Margin::symmetric(
                    layout::TOOLTIP_PADDING_HORIZONTAL,
                    layout::TOOLTIP_PADDING_VERTICAL,
                ))
                .corner_radius(layout::TOOLTIP_CORNER_RADIUS)
                .shadow(egui::epaint::Shadow {
                    offset: [0, 1],
                    blur: 3,
                    spread: 0,
                    color: Color32::from_rgba_unmultiplied(0, 0, 0, 28),
                }),
        );
    let shown = tooltip.show(|ui| {
        ui.set_min_height(layout::TOOLTIP_CONTENT_MIN_HEIGHT);
        ui.vertical_centered(|ui| {
            add_contents(ui);
        });
    });
    if let Some(shown) = shown {
        let rect = shown.response.rect;
        // Align the arrow with the hovered control rather than the popup's
        // center. Clamp it so a wide popup still keeps the arrow on its edge.
        let center_x = response.rect.center().x.clamp(
            rect.left() + layout::TOOLTIP_ARROW_WIDTH / 2.0,
            rect.right() - layout::TOOLTIP_ARROW_WIDTH / 2.0,
        );
        let fill = tooltip_background_color();
        response
            .ctx
            .layer_painter(shown.response.layer_id)
            .add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(center_x - layout::TOOLTIP_ARROW_WIDTH / 2.0, rect.bottom()),
                    egui::pos2(center_x + layout::TOOLTIP_ARROW_WIDTH / 2.0, rect.bottom()),
                    egui::pos2(center_x, rect.bottom() + layout::TOOLTIP_ARROW_HEIGHT),
                ],
                fill,
                Stroke::NONE,
            ));
    }
}

fn tooltip_background_color() -> Color32 {
    Color32::from_rgba_unmultiplied(54, 0, 148, layout::TOOLTIP_BACKGROUND_ALPHA)
}

fn selected_client_status(
    target: ClientTarget,
    mode: ModelListMode,
    installed: bool,
    running: bool,
) -> String {
    selected_client_status_for_language(target, mode, installed, running, i18n::language())
}

fn selected_client_status_for_language(
    target: ClientTarget,
    mode: ModelListMode,
    installed: bool,
    running: bool,
    language: i18n::Language,
) -> String {
    if !installed {
        return format!(
            "{} • {} • {}",
            target.title(),
            language.text("未安装", "Not installed"),
            model_mode_label_for_language(mode, language)
        );
    }
    if running {
        if language == i18n::Language::En {
            format!(
                "{} • {} • Running ...",
                target.title(),
                model_mode_label_for_language(mode, language)
            )
        } else {
            format!(
                "{} • 已安装 • {} • 运行中 ...",
                target.title(),
                model_mode_label_for_language(mode, language)
            )
        }
    } else {
        format!(
            "{} • {} • {}",
            target.title(),
            language.text("已安装", "Installed"),
            model_mode_label_for_language(mode, language)
        )
    }
}

fn client_exited_status(target: ClientTarget) -> String {
    format!("{} • {}", target.title(), i18n::tr("已退出", "Exited"))
}

fn client_launch_requires_install_status(target: ClientTarget) -> String {
    format!(
        "{} • {} • {}",
        target.title(),
        i18n::tr("未安装", "Not installed"),
        i18n::tr("请先安装客户端", "Install the client first")
    )
}

fn client_kind_label(target: ClientTarget) -> &'static str {
    match target {
        ClientTarget::CodexCli | ClientTarget::ClaudeCode => {
            i18n::tr("终端 Agent", "Terminal Agent")
        }
        ClientTarget::CodexDesktop | ClientTarget::ClaudeDesktop => {
            i18n::tr("桌面 Agent", "Desktop Agent")
        }
    }
}

fn client_card_state_label(installed: bool, running: bool) -> &'static str {
    if running {
        i18n::tr("已启动", "Running")
    } else if installed {
        i18n::tr("已安装", "Installed")
    } else {
        i18n::tr("未安装", "Not installed")
    }
}

fn client_card_state_color(installed: bool, running: bool) -> Color32 {
    if running || installed {
        GREEN
    } else {
        NOT_INSTALLED
    }
}
fn model_mode_label(mode: ModelListMode) -> &'static str {
    model_mode_label_for_language(mode, i18n::language())
}

fn model_mode_label_for_language(mode: ModelListMode, language: i18n::Language) -> &'static str {
    match mode {
        ModelListMode::BuiltIn => language.text("默认模型列表", "Default models"),
        ModelListMode::Gateway => language.text("网关模型列表", "Gateway models"),
    }
}
fn tasks_contain_client(tasks: &[RunningTask], target: ClientTarget) -> bool {
    tasks
        .iter()
        .any(|task| task.kind == TaskKind::Client(target))
}

fn completed_task_status(task: &RunningTask) -> String {
    if task.kind == TaskKind::GatewayTest
        && let Some(line) = task.log_text.lines().map(str::trim).find(|line| {
            line.starts_with("成功连接 • ")
                || line.starts_with("成功拉取 • ")
                || line.starts_with("Connected • ")
                || line.starts_with("Fetched • ")
        })
    {
        return line.to_owned();
    }
    format!("{} • {}", task.label, i18n::tr("操作已完成", "Completed"))
}

fn failed_task_status(task: &RunningTask) -> String {
    if task
        .log_text
        .contains(crate::gateway::MAINLAND_ACCESS_ERROR)
    {
        i18n::tr(
            crate::gateway::MAINLAND_ACCESS_ERROR,
            crate::gateway::MAINLAND_ACCESS_ERROR_EN,
        )
        .to_owned()
    } else {
        format!(
            "{} • {}",
            task.label,
            i18n::tr(
                "操作失败 • 请查看运行日志 ...",
                "Failed • Check the runtime log ..."
            )
        )
    }
}

fn refresh_task_log(task: &mut RunningTask) {
    let Ok(metadata) = fs::metadata(&task.log_path) else {
        return;
    };
    let Ok(modified) = metadata.modified() else {
        return;
    };
    if modified <= task.last_log_read {
        return;
    }
    let Ok(mut file) = File::open(&task.log_path) else {
        return;
    };
    let mut text = String::new();
    if file.read_to_string(&mut text).is_ok() {
        task.log_text = text.trim_start_matches('\u{feff}').to_owned();
        task.last_log_read = modified;
    }
}

fn task_log_title(kind: TaskKind, label: &str) -> String {
    match kind {
        TaskKind::GatewayTest if is_fetch_models_label(label) => {
            i18n::tr("网关 • 拉取模型", "Gateway • Fetch models").to_owned()
        }
        TaskKind::GatewayTest => {
            i18n::tr("网关 • 连接测试", "Gateway • Connection test").to_owned()
        }
        TaskKind::Client(target) => format!(
            "{} • {}",
            target.title(),
            i18n::tr("配置 • 启动", "Configure • Launch")
        ),
        TaskKind::Install(target) => {
            format!("{} • {}", target.title(), i18n::tr("安装", "Install"))
        }
    }
}

fn task_log_slug(kind: TaskKind, label: &str) -> String {
    match kind {
        TaskKind::GatewayTest if is_fetch_models_label(label) => "gateway-models".to_owned(),
        TaskKind::GatewayTest => "gateway-test".to_owned(),
        TaskKind::Client(target) => format!("{}-launch", target.id()),
        TaskKind::Install(target) => format!("{}-install", target.id()),
    }
}

fn is_fetch_models_label(label: &str) -> bool {
    matches!(label, "拉取模型" | "Fetch models")
}

fn format_log_section(title: &str, text: &str) -> String {
    let normalized = text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut lines = normalized.lines().collect::<Vec<_>>();
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    if lines
        .first()
        .is_some_and(|line| is_structured_log_title(line.trim()))
    {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines
        .last()
        .is_some_and(|line| line.trim() == LOG_SECTION_FOOTER)
    {
        lines.pop();
    }

    let body = normalize_log_body(&lines.join("\n"));
    let body = if body.is_empty() {
        i18n::tr("（等待输出）", "(Waiting for output)").to_owned()
    } else {
        body
    };
    format!("===== {title} =====\n\n{body}\n\n{LOG_SECTION_FOOTER}")
}

fn is_structured_log_title(line: &str) -> bool {
    line.starts_with("=====") && line.ends_with("=====")
}

fn normalize_log_body(text: &str) -> String {
    let mut output = Vec::new();
    let mut previous_was_blank = false;
    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.trim().is_empty() {
            if !output.is_empty() && !previous_was_blank {
                output.push(String::new());
            }
            previous_was_blank = true;
            continue;
        }

        previous_was_blank = false;
        if let Some(detail) = line
            .strip_prefix("错误:")
            .or_else(|| line.strip_prefix("错误："))
            .or_else(|| line.strip_prefix("Error:"))
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
        {
            output.push(i18n::tr("错误：", "Error:").to_owned());
            output.push(format!("  {detail}"));
        } else {
            output.push(line.to_owned());
        }
    }
    while output.last().is_some_and(String::is_empty) {
        output.pop();
    }
    output.join("\n")
}

fn join_ordered_log_sections(sections: impl IntoIterator<Item = (u64, String)>) -> String {
    let mut sections = sections
        .into_iter()
        .filter_map(|(sequence, text)| {
            let text = text.trim_end();
            (!text.is_empty()).then(|| (sequence, text.to_owned()))
        })
        .collect::<Vec<_>>();
    sections.sort_by_key(|(sequence, _)| *sequence);
    sections
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join(LOG_SECTION_SEPARATOR)
}

fn latest_install_progress(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty() && !is_structured_log_title(line))
        .map(str::to_owned)
}

fn client_worker_arguments(
    target: ClientTarget,
    gateway_url: &str,
    model_list_mode: ModelListMode,
) -> Vec<String> {
    vec![
        "internal-run".to_owned(),
        target.id().to_owned(),
        "--model-list-mode".to_owned(),
        model_list_mode.id().to_owned(),
        "--gateway-url".to_owned(),
        normalized_gateway_url(gateway_url),
    ]
}

fn validate_gateway_inputs(gateway_url: &str, api_key: &str) -> Result<()> {
    let parsed = match url::Url::parse(gateway_url.trim()) {
        Ok(parsed) => parsed,
        Err(_) => bail!(
            "{}",
            i18n::tr("接口地址格式无效 !", "Invalid gateway URL format !")
        ),
    };
    if parsed.host_str().is_none() {
        bail!(
            "{}",
            i18n::tr("接口地址缺少主机名 !", "Gateway URL is missing a host !")
        );
    }
    if parsed.scheme() != "https"
        && !matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
    {
        bail!(
            "{}",
            i18n::tr("接口地址必须使用 HTTPS !", "Gateway URLs must use HTTPS !")
        );
    }
    if api_key.trim().is_empty() {
        bail!(
            "{}",
            i18n::tr("请输入 API Key !", "Please enter the API key !")
        );
    }
    Ok(())
}

fn normalized_gateway_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_owned()
}

fn render_svg_rgba(bytes: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let tree = resvg::usvg::Tree::from_data(bytes, &resvg::usvg::Options::default())
        .map_err(|error| anyhow::anyhow!("SVG 解析失败: {error}"))?;
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(width, height).context("无法创建 SVG 光栅化画布")?;

    let source_size = tree.size();
    let source_width = source_size.width();
    let source_height = source_size.height();
    let scale = ((width as f32) / source_width).min((height as f32) / source_height);
    let translate_x = (width as f32 - source_width * scale) / 2.0;
    let translate_y = (height as f32 - source_height * scale) / 2.0;
    let transform =
        resvg::tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, translate_x, translate_y);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok(pixmap.data().to_vec())
}

fn svg_pixels_per_point(context: &egui::Context) -> f32 {
    // 量化到 1/64，过滤浮点抖动，避免同一显示器上反复重建纹理。
    let pixels_per_point = context.pixels_per_point().max(0.5);
    (pixels_per_point * 64.0).round() / 64.0
}

fn load_embedded_svg_texture(
    context: &egui::Context,
    name: &str,
    bytes: &[u8],
    logical_size: Vec2,
) -> TextureHandle {
    // 直接生成与目标控件物理像素一一对应的纹理。旧实现固定放大四倍后由 GPU
    // 二次缩小，LINEAR 在小图标上容易产生锯齿和发虚；精确尺寸更接近浏览器渲染。
    let pixels_per_point = svg_pixels_per_point(context);
    let width = (logical_size.x.max(1.0) * pixels_per_point)
        .round()
        .max(1.0) as u32;
    let height = (logical_size.y.max(1.0) * pixels_per_point)
        .round()
        .max(1.0) as u32;
    let rgba = render_svg_rgba(bytes, width, height)
        .unwrap_or_else(|error| panic!("内置 SVG 图标 {name} 光栅化失败：{error:#}"));
    let color_image =
        egui::ColorImage::from_rgba_premultiplied([width as usize, height as usize], &rgba);
    context.load_texture(name, color_image, TextureOptions::LINEAR)
}

fn align_rect_to_physical_pixels(context: &egui::Context, rect: Rect) -> Rect {
    let pixels_per_point = context.pixels_per_point().max(0.5);
    let snap = |value: f32| (value * pixels_per_point).round() / pixels_per_point;
    Rect::from_min_max(
        egui::pos2(snap(rect.min.x), snap(rect.min.y)),
        egui::pos2(snap(rect.max.x), snap(rect.max.y)),
    )
}

fn paint_svg_texture(ui: &egui::Ui, texture: &TextureHandle, rect: Rect, uv: Rect, tint: Color32) {
    ui.painter().image(
        texture.id(),
        align_rect_to_physical_pixels(ui.ctx(), rect),
        uv,
        tint,
    );
}

fn unpremultiply_rgba(premultiplied: &[u8]) -> Vec<u8> {
    let mut rgba = premultiplied.to_vec();
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = pixel[3] as u16;
        for channel in &mut pixel[..3] {
            let numerator = *channel as u16 * 255 + alpha / 2;
            let value = numerator.checked_div(alpha).unwrap_or(0);
            *channel = value.min(255) as u8;
        }
    }
    rgba
}

fn native_icon_data() -> Result<egui::IconData> {
    const ICON_SIZE: u32 = 64;
    let premultiplied = render_svg_rgba(
        include_bytes!("../assets/gui/svg/logo.svg"),
        ICON_SIZE,
        ICON_SIZE,
    )
    .context("无法读取内置程序图标")?;
    Ok(egui::IconData {
        rgba: unpremultiply_rgba(&premultiplied),
        width: ICON_SIZE,
        height: ICON_SIZE,
    })
}
fn maybe_write_layout_probe(ui: &egui::Ui, rects: &[(&str, Rect)]) {
    let Some(path) = std::env::var_os("ZNNZ_GUI_LAYOUT_DEBUG") else {
        return;
    };
    static WRITE_ONCE: std::sync::Once = std::sync::Once::new();
    WRITE_ONCE.call_once(|| {
        let mut output = format!(
            "pixels_per_point={:.3} expected_window={:.1}x{:.1} actual_ui={:?}\n",
            ui.ctx().pixels_per_point(),
            layout::WINDOW_WIDTH,
            layout::WINDOW_HEIGHT,
            ui.max_rect(),
        );
        for (name, rect) in rects {
            output.push_str(&format!(
                "{name}: left={:.1} top={:.1} right={:.1} bottom={:.1} width={:.1} height={:.1}\n",
                rect.left(),
                rect.top(),
                rect.right(),
                rect.bottom(),
                rect.width(),
                rect.height(),
            ));
        }
        let _ = fs::write(PathBuf::from(path), output);
    });
}

#[cfg(windows)]
const NATIVE_WINDOW_SUBCLASS_ID: usize = 0x5A4E_4E5A;
// Windows 主题管理器会用这两个未公开消息单独重画标题文字和标题框。
// 即使 WS_CAPTION 已被移除，也必须与 WM_NCPAINT 一起拦截，才能杜绝激活时闪现。
#[cfg(windows)]
const WM_NCUAHDRAWCAPTION: u32 = 0x00AE;
#[cfg(windows)]
const WM_NCUAHDRAWFRAME: u32 = 0x00AF;

#[cfg(windows)]
thread_local! {
    // 两个钩子都只作用于当前 GUI 线程，不会影响其他线程或进程。
    // WH_CBT 在 CreateWindowExW 真正创建窗口前清理样式；WH_CALLWNDPROC 则保证
    // 第一次 WM_SHOWWINDOW 进入窗口过程前，最终尺寸对应的 Region 已经生效。
    static NATIVE_CREATE_HOOK: Cell<*mut std::ffi::c_void> = const { Cell::new(std::ptr::null_mut()) };
    static NATIVE_PRE_SHOW_HOOK: Cell<*mut std::ffi::c_void> = const { Cell::new(std::ptr::null_mut()) };
    static NATIVE_MAIN_WINDOW: Cell<*mut std::ffi::c_void> = const { Cell::new(std::ptr::null_mut()) };
    static NATIVE_PRE_SHOW_APPLIED: Cell<bool> = const { Cell::new(false) };
}

#[cfg(windows)]
struct NativeCreateHook;

#[cfg(windows)]
impl NativeCreateHook {
    fn install() -> windows::core::Result<Self> {
        let thread_id = unsafe { GetCurrentThreadId() };

        // 先安装“首次显示前”钩子，确保即使窗口创建流程很快，也不会漏掉第一次显示消息。
        let pre_show_hook = unsafe {
            SetWindowsHookExW(
                WH_CALLWNDPROC,
                Some(native_pre_show_hook_proc),
                None,
                thread_id,
            )?
        };
        NATIVE_PRE_SHOW_HOOK.with(|slot| slot.set(pre_show_hook.0));
        NATIVE_MAIN_WINDOW.with(|slot| slot.set(std::ptr::null_mut()));
        NATIVE_PRE_SHOW_APPLIED.with(|slot| slot.set(false));

        let create_hook = match unsafe {
            SetWindowsHookExW(WH_CBT, Some(native_create_hook_proc), None, thread_id)
        } {
            Ok(hook) => hook,
            Err(error) => {
                unsafe { remove_native_pre_show_hook() };
                return Err(error);
            }
        };
        NATIVE_CREATE_HOOK.with(|slot| slot.set(create_hook.0));
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for NativeCreateHook {
    fn drop(&mut self) {
        unsafe {
            remove_native_create_hook();
            remove_native_pre_show_hook();
        }
    }
}

#[cfg(windows)]
unsafe fn remove_native_create_hook() {
    NATIVE_CREATE_HOOK.with(|slot| {
        let raw_hook = slot.replace(std::ptr::null_mut());
        if !raw_hook.is_null() {
            let _ = unsafe { UnhookWindowsHookEx(HHOOK(raw_hook)) };
        }
    });
}

#[cfg(windows)]
unsafe fn remove_native_pre_show_hook() {
    NATIVE_PRE_SHOW_HOOK.with(|slot| {
        let raw_hook = slot.replace(std::ptr::null_mut());
        if !raw_hook.is_null() {
            let _ = unsafe { UnhookWindowsHookEx(HHOOK(raw_hook)) };
        }
    });
}

#[cfg(windows)]
unsafe fn wide_window_title_equals(title: windows::core::PCWSTR, expected: &str) -> bool {
    let pointer = title.0;
    // Win32 字符串参数也允许使用低位整数资源 ID；这种值不能按指针解引用。
    if pointer.is_null() || pointer as usize <= 0xFFFF {
        return false;
    }

    let mut index = 0usize;
    for expected_unit in expected.encode_utf16() {
        if unsafe { *pointer.add(index) } != expected_unit {
            return false;
        }
        index += 1;
    }
    unsafe { *pointer.add(index) == 0 }
}

#[cfg(windows)]
unsafe extern "system" fn native_create_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let mut matched_main_window = false;

    if code == HCBT_CREATEWND as i32 && lparam.0 != 0 {
        let create_window = unsafe { &mut *(lparam.0 as *mut CBT_CREATEWNDW) };
        if !create_window.lpcs.is_null() {
            let create = unsafe { &mut *create_window.lpcs };
            if unsafe { wide_window_title_equals(create.lpszName, WINDOW_TITLE) } {
                // 直接修改即将传给 CreateWindowExW 的 CREATESTRUCTW。窗口从诞生起就是
                // Popup 客户区，不再先创建系统标题栏后移除，行为与 InstallPE.c 一致。
                create.style = clean_native_window_style(create.style as u32) as i32;
                create.dwExStyle.0 = clean_native_extended_style(create.dwExStyle.0);
                let hwnd = HWND(wparam.0 as *mut std::ffi::c_void);
                NATIVE_MAIN_WINDOW.with(|slot| slot.set(hwnd.0));
                NATIVE_PRE_SHOW_APPLIED.with(|slot| slot.set(false));

                // HCBT_CREATEWND 发生在 WM_NCCREATE / WM_CREATE 之前，此时 HWND 已可用。
                // 在这里安装子类过程，确保窗口从第一条非客户区消息开始就由我们接管，
                // 而不是等 LauncherApp::new() 执行后才清除已经出现过的系统标题栏。
                let _ = unsafe {
                    SetWindowSubclass(
                        hwnd,
                        Some(native_window_subclass_proc),
                        NATIVE_WINDOW_SUBCLASS_ID,
                        0,
                    )
                };
                matched_main_window = true;
            }
        }
    }

    // 先让同线程上的其他 CBT 钩子看到已经清理过的创建参数，再卸载本钩子。
    let result = unsafe { CallNextHookEx(None, code, wparam, lparam) };
    if matched_main_window {
        unsafe { remove_native_create_hook() };
    }
    result
}

#[cfg(windows)]
unsafe extern "system" fn native_pre_show_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let mut completed = false;

    if code >= 0 && lparam.0 != 0 {
        let call = unsafe { &*(lparam.0 as *const CWPSTRUCT) };
        let target = NATIVE_MAIN_WINDOW.with(|slot| slot.get());
        let is_main_window = !target.is_null() && call.hwnd.0 == target;
        if is_main_window && call.message == WM_SHOWWINDOW && call.wParam.0 != 0 {
            let should_apply = NATIVE_PRE_SHOW_APPLIED.with(|slot| !slot.replace(true));
            if should_apply {
                // WH_CALLWNDPROC 在目标窗口过程收到 WM_SHOWWINDOW 之前运行。此时 winit 已经
                // 写入最终 DPI/尺寸，但窗口仍未显示，是设置 Region 的最后且最可靠时机。
                unsafe { prepare_native_window_for_first_show(call.hwnd) };
                completed = true;
            }
        }
    }

    let result = unsafe { CallNextHookEx(None, code, wparam, lparam) };
    if completed {
        unsafe { remove_native_pre_show_hook() };
        NATIVE_MAIN_WINDOW.with(|slot| slot.set(std::ptr::null_mut()));
    }
    result
}

#[cfg(windows)]
unsafe fn prepare_native_window_for_first_show(hwnd: HWND) {
    unsafe {
        enforce_native_popup_style(hwnd);
        apply_native_frame_policy(hwnd);

        let dpi = native_window_dpi(hwnd);
        let expected_width = scale_dip(layout::WINDOW_WIDTH, dpi);
        let expected_height = scale_dip(layout::WINDOW_HEIGHT, dpi);
        let (x, y) =
            bottom_right_position(hwnd, expected_width, expected_height, dpi).unwrap_or((0, 0));
        let _ = SetWindowPos(
            hwnd,
            None,
            x,
            y,
            expected_width,
            expected_height,
            SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE,
        );

        // SetWindowPos 后重新读取真实外窗尺寸，避免 DPI 舍入导致 Region 少一行或一列。
        let mut rect = RECT::default();
        let (width, height) = if GetWindowRect(hwnd, &mut rect).is_ok() {
            (rect.right - rect.left, rect.bottom - rect.top)
        } else {
            (expected_width, expected_height)
        };
        apply_cutout_region(hwnd, width, height, dpi);
    }
}

#[cfg(windows)]
struct NativeWindowState {
    hwnd: HWND,
    last_dpi: u32,
    anchored_to_bottom_right: bool,
    first_maintenance: bool,
}

#[cfg(windows)]
impl NativeWindowState {
    fn configure(creation: &eframe::CreationContext<'_>) -> Option<Self> {
        let window_handle = creation.window_handle().ok()?;
        let RawWindowHandle::Win32(win32_handle) = window_handle.as_raw() else {
            return None;
        };
        let hwnd = HWND(win32_handle.hwnd.get() as *mut std::ffi::c_void);

        unsafe {
            // 先安装 Win32 子类过程，再应用切口；winit 在显示窗口或 DPI 变化时会重算窗口样式。
            // 子类过程会拦截 WS_CAPTION 等非客户区样式，避免系统标题栏短暂闪现。
            let _ = SetWindowSubclass(
                hwnd,
                Some(native_window_subclass_proc),
                NATIVE_WINDOW_SUBCLASS_ID,
                0,
            );

            enforce_native_popup_style(hwnd);
            apply_native_frame_policy(hwnd);

            let dpi = native_window_dpi(hwnd);
            let width = scale_dip(layout::WINDOW_WIDTH, dpi);
            let height = scale_dip(layout::WINDOW_HEIGHT, dpi);
            let (x, y) = bottom_right_position(hwnd, width, height, dpi).unwrap_or((0, 0));

            let _ = SetWindowPos(
                hwnd,
                None,
                x,
                y,
                width,
                height,
                SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            apply_cutout_region(hwnd, width, height, dpi);

            Some(Self {
                hwnd,
                last_dpi: dpi,
                anchored_to_bottom_right: true,
                // App 创建后 winit 还会写入一次窗口样式，首帧 UI 无条件再校正一次。
                first_maintenance: true,
            })
        }
    }

    fn maintain(&mut self, context: &egui::Context) {
        unsafe {
            // Windows 最小化时的窗口矩形不可用，此时不重算尺寸和位置。
            // eframe 首帧显示前必须再应用一次 HWND Region，否则首次打开可能看不到切口。
            if IsIconic(self.hwnd).as_bool() {
                return;
            }

            let dpi = native_window_dpi(self.hwnd);
            let expected_width = scale_dip(layout::WINDOW_WIDTH, dpi);
            let expected_height = scale_dip(layout::WINDOW_HEIGHT, dpi);
            let style_changed = enforce_native_popup_style(self.hwnd);

            let mut client_rect = RECT::default();
            let client_size = GetClientRect(self.hwnd, &mut client_rect).ok().map(|_| {
                (
                    client_rect.right - client_rect.left,
                    client_rect.bottom - client_rect.top,
                )
            });
            let size_is_wrong = client_size.is_none_or(|(width, height)| {
                (width - expected_width).abs() > 2 || (height - expected_height).abs() > 2
            });
            let dpi_changed = dpi != self.last_dpi;
            let refresh_native_shape =
                self.first_maintenance || style_changed || size_is_wrong || dpi_changed;

            if refresh_native_shape {
                let mut window_rect = RECT::default();
                let current_position = GetWindowRect(self.hwnd, &mut window_rect)
                    .ok()
                    .map(|_| (window_rect.left, window_rect.top));

                let position =
                    if self.first_maintenance || (dpi_changed && self.anchored_to_bottom_right) {
                        bottom_right_position(self.hwnd, expected_width, expected_height, dpi)
                    } else {
                        current_position.map(|(x, y)| {
                            clamp_position_to_work_area(
                                self.hwnd,
                                x,
                                y,
                                expected_width,
                                expected_height,
                            )
                        })
                    };

                let (x, y, mut position_flags) = match position {
                    Some((x, y)) => (x, y, SWP_NOZORDER | SWP_NOACTIVATE),
                    None => (0, 0, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE),
                };
                if self.first_maintenance || style_changed || dpi_changed {
                    position_flags |= SWP_FRAMECHANGED;
                }

                apply_native_frame_policy(self.hwnd);
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    x,
                    y,
                    expected_width,
                    expected_height,
                    position_flags,
                );
                apply_cutout_region(self.hwnd, expected_width, expected_height, dpi);
                context.request_repaint();
            }

            self.first_maintenance = false;
            self.last_dpi = dpi;
            self.anchored_to_bottom_right = is_at_bottom_right(self.hwnd, dpi);
        }
    }
}

#[cfg(windows)]
fn clean_native_window_style(style: u32) -> u32 {
    // Keep an InstallPE-style popup client area, but retain WS_SYSMENU + WS_MINIMIZEBOX.
    // These flags preserve standard Windows taskbar minimize behavior without restoring a caption.
    (style | WS_POPUP.0 | WS_CLIPCHILDREN.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0)
        & !(WS_CAPTION.0 | WS_BORDER.0 | WS_THICKFRAME.0 | WS_MAXIMIZEBOX.0)
}

#[cfg(windows)]
fn clean_native_extended_style(style: u32) -> u32 {
    style & !(WS_EX_WINDOWEDGE.0 | WS_EX_CLIENTEDGE.0 | WS_EX_DLGMODALFRAME.0 | WS_EX_STATICEDGE.0)
}

#[cfg(windows)]
unsafe fn enforce_native_popup_style(hwnd: HWND) -> bool {
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 };
    let clean_style = clean_native_window_style(style);
    let extended_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 };
    let clean_extended_style = clean_native_extended_style(extended_style);
    let mut changed = false;

    if clean_style != style {
        unsafe { SetWindowLongPtrW(hwnd, GWL_STYLE, clean_style as isize) };
        changed = true;
    }
    if clean_extended_style != extended_style {
        unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, clean_extended_style as isize) };
        changed = true;
    }
    changed
}

#[cfg(windows)]
unsafe fn apply_native_frame_policy(hwnd: HWND) {
    // 关闭 DWM 非客户区绘制，并禁止 Windows 11 自动圆角和边框；外形完全交给 SetWindowRgn 控制。
    let non_client_policy = DWMNCRP_DISABLED;
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY,
            std::ptr::from_ref(&non_client_policy).cast(),
            std::mem::size_of_val(&non_client_policy) as u32,
        )
    };

    let corner_preference = DWMWCP_DONOTROUND;
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            std::ptr::from_ref(&corner_preference).cast(),
            std::mem::size_of_val(&corner_preference) as u32,
        )
    };

    const DWMWA_COLOR_NONE: u32 = 0xFFFF_FFFE;
    let border_color = DWMWA_COLOR_NONE;
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            (&border_color as *const u32).cast(),
            std::mem::size_of_val(&border_color) as u32,
        )
    };
}

#[cfg(windows)]
unsafe extern "system" fn native_window_subclass_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _reference_data: usize,
) -> LRESULT {
    match message {
        WM_NCCALCSIZE => {
            // 整个 HWND 都是客户区，不给系统标题栏和边框预留任何非客户区像素。
            return LRESULT(0);
        }
        WM_NCPAINT | WM_NCUAHDRAWCAPTION | WM_NCUAHDRAWFRAME => {
            // 禁止系统和主题管理器绘制标题栏、顶部横线及边框。
            return LRESULT(0);
        }
        WM_NCACTIVATE => {
            // 仍把消息交给 winit，让它维护 Focused/active 状态；但把 lParam 设为 -1，
            // 按 Win32 约定要求 DefWindowProc 不重画非客户区，避免点击窗口、桌面或
            // 任务栏图标时瞬间叠加原生标题栏。
            return unsafe { DefSubclassProc(hwnd, message, wparam, LPARAM(-1)) };
        }
        _ => {}
    }

    if message == WM_STYLECHANGING && lparam.0 != 0 {
        let style = unsafe { &mut *(lparam.0 as *mut STYLESTRUCT) };
        match wparam.0 as i32 {
            value if value == GWL_STYLE.0 => {
                style.styleNew = clean_native_window_style(style.styleNew)
            }
            value if value == GWL_EXSTYLE.0 => {
                style.styleNew = clean_native_extended_style(style.styleNew)
            }
            _ => {}
        }
    }

    if message == WM_DPICHANGED {
        // 先让 winit/Windows 完成 DPI 切换，随后立即恢复 DWM 策略、无边框样式和 Region。
        // 这样 DPI 变化后不必等待下一帧才恢复切口。
        let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
        unsafe {
            apply_native_frame_policy(hwnd);
            enforce_native_popup_style(hwnd);
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_ok() {
                apply_cutout_region(
                    hwnd,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    native_window_dpi(hwnd),
                );
            }
        }
        return result;
    }

    if message == WM_NCDESTROY {
        let _ = unsafe {
            RemoveWindowSubclass(
                hwnd,
                Some(native_window_subclass_proc),
                NATIVE_WINDOW_SUBCLASS_ID,
            )
        };
    }

    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

#[cfg(windows)]
fn native_window_dpi(hwnd: HWND) -> u32 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 96 } else { dpi }
}

#[cfg(windows)]
fn scale_dip(value: f32, dpi: u32) -> i32 {
    (value * dpi as f32 / 96.0).round() as i32
}

#[cfg(windows)]
unsafe fn monitor_work_area(hwnd: HWND) -> Option<RECT> {
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
    if monitor.0.is_null() {
        return None;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        Some(info.rcWork)
    } else {
        None
    }
}

#[cfg(windows)]
unsafe fn bottom_right_position(
    hwnd: HWND,
    width: i32,
    height: i32,
    dpi: u32,
) -> Option<(i32, i32)> {
    let work = unsafe { monitor_work_area(hwnd) }?;
    let margin_right = scale_dip(layout::WINDOW_START_MARGIN_RIGHT, dpi);
    let margin_bottom = scale_dip(layout::WINDOW_START_MARGIN_BOTTOM, dpi);
    Some((
        work.right - width - margin_right,
        work.bottom - height - margin_bottom,
    ))
}

#[cfg(windows)]
unsafe fn clamp_position_to_work_area(
    hwnd: HWND,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> (i32, i32) {
    let Some(work) = (unsafe { monitor_work_area(hwnd) }) else {
        return (x, y);
    };
    let max_x = (work.right - width).max(work.left);
    let max_y = (work.bottom - height).max(work.top);
    (x.clamp(work.left, max_x), y.clamp(work.top, max_y))
}

#[cfg(windows)]
unsafe fn is_at_bottom_right(hwnd: HWND, dpi: u32) -> bool {
    let Some(work) = (unsafe { monitor_work_area(hwnd) }) else {
        return false;
    };
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return false;
    }
    let expected_right = work.right - scale_dip(layout::WINDOW_START_MARGIN_RIGHT, dpi);
    let expected_bottom = work.bottom - scale_dip(layout::WINDOW_START_MARGIN_BOTTOM, dpi);
    (rect.right - expected_right).abs() <= 3 && (rect.bottom - expected_bottom).abs() <= 3
}

#[cfg(windows)]
unsafe fn subtract_region_rect(
    full: windows::Win32::Graphics::Gdi::HRGN,
    cut: windows::Win32::Graphics::Gdi::HRGN,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
) {
    if right <= left || bottom <= top {
        return;
    }
    let _ = unsafe { SetRectRgn(cut, left, top, right, bottom) };
    let _ = unsafe { CombineRgn(Some(full), Some(full), Some(cut), RGN_DIFF) };
}

#[cfg(windows)]
unsafe fn apply_cutout_region(hwnd: HWND, width: i32, height: i32, dpi: u32) {
    if width <= 0 || height <= 0 {
        return;
    }

    let full = unsafe { CreateRectRgn(0, 0, width, height) };
    if full.0.is_null() {
        return;
    }
    let cut = unsafe { CreateRectRgn(0, 0, 0, 0) };
    if cut.0.is_null() {
        let _ = unsafe { DeleteObject(HGDIOBJ(full.0)) };
        return;
    }

    let corner = scale_dip(layout::WINDOW_CORNER_CUT_SIZE, dpi).clamp(0, width.min(height));
    if corner > 0 {
        unsafe { subtract_region_rect(full, cut, 0, 0, corner, corner) };
        unsafe { subtract_region_rect(full, cut, width - corner, 0, width, corner) };
        unsafe { subtract_region_rect(full, cut, 0, height - corner, corner, height) };
        unsafe { subtract_region_rect(full, cut, width - corner, height - corner, width, height) };
    }

    let side_width = scale_dip(layout::WINDOW_SIDE_CUT_WIDTH, dpi).clamp(0, width);
    let side_height = scale_dip(layout::WINDOW_SIDE_CUT_HEIGHT, dpi).clamp(0, height);
    if side_width > 0 && side_height > 0 {
        let side_y = scale_dip(layout::WINDOW_SIDE_CUT_OFFSET_Y, dpi)
            .clamp(0, height.saturating_sub(side_height));
        unsafe { subtract_region_rect(full, cut, 0, side_y, side_width, side_y + side_height) };
        unsafe {
            subtract_region_rect(
                full,
                cut,
                width - side_width,
                side_y,
                width,
                side_y + side_height,
            )
        };
    }

    // SetWindowRgn 成功后 full 的所有权转交给 Windows，只有失败时才能在本处释放。
    if unsafe { SetWindowRgn(hwnd, Some(full), true) } == 0 {
        let _ = unsafe { DeleteObject(HGDIOBJ(full.0)) };
    }
    let _ = unsafe { DeleteObject(HGDIOBJ(cut.0)) };
}

fn configure_style(context: &egui::Context) {
    context.global_style_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(8.0, 5.0);
        style.interaction.tooltip_delay = layout::TOOLTIP_DELAY_SECONDS;
        style.interaction.tooltip_grace_time = 0.15;
        style.interaction.show_tooltips_only_when_still = false;
        style.visuals.dark_mode = true;
        style.visuals.panel_fill = Color32::TRANSPARENT;
        style.visuals.window_fill = log_window_base_color();
        style.visuals.faint_bg_color = secondary_surface_bg_color();
        style.visuals.extreme_bg_color = secondary_surface_bg_color();
        style.visuals.override_text_color = Some(TEXT_MAIN);
        // TextEdit 会将 HintText 强制映射为 weak_text_color；在这里设置才会真正生效。
        style.visuals.weak_text_color = Some(input_hint_text_color());
        style.visuals.widgets.noninteractive.bg_fill = secondary_surface_bg_color();
        style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
        style.visuals.widgets.inactive.bg_fill = secondary_surface_bg_color();
        style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(42, 43, 51);
        style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, BLUE_BRIGHT);
        style.visuals.widgets.active.bg_fill = Color32::from_rgb(35, 45, 73);
        style.visuals.widgets.active.bg_stroke = Stroke::new(1.0, BLUE_BRIGHT);
        style.visuals.widgets.inactive.corner_radius = 8.into();
        style.visuals.widgets.hovered.corner_radius = 8.into();
        style.visuals.widgets.active.corner_radius = 8.into();
        style.visuals.selection.bg_fill = BLUE;
        style.visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    });
}
fn install_fonts(context: &egui::Context) {
    let regular_candidates = [
        PathBuf::from(r"C:\Windows\Fonts\msyh.ttc"),
        PathBuf::from(r"C:\Windows\Fonts\msyh.ttf"),
        PathBuf::from(r"C:\Windows\Fonts\simhei.ttf"),
    ];
    let Some(regular_bytes) = regular_candidates
        .iter()
        .find_map(|path| fs::read(path).ok())
    else {
        return;
    };
    let bold_bytes = [
        PathBuf::from(r"C:\Windows\Fonts\msyhbd.ttc"),
        PathBuf::from(r"C:\Windows\Fonts\msyhbd.ttf"),
        PathBuf::from(r"C:\Windows\Fonts\simhei.ttf"),
    ]
    .iter()
    .find_map(|path| fs::read(path).ok())
    .unwrap_or_else(|| regular_bytes.clone());
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "system-cjk".to_owned(),
        FontData::from_owned(regular_bytes).into(),
    );
    fonts.font_data.insert(
        "system-cjk-bold".to_owned(),
        FontData::from_owned(bold_bytes).into(),
    );
    for family in [FontFamily::Proportional, FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, "system-cjk".to_owned());
    }
    fonts.families.insert(
        FontFamily::Name("status-bold".into()),
        vec!["system-cjk-bold".to_owned(), "system-cjk".to_owned()],
    );
    context.set_fonts(fonts);
}

fn logs_directory() -> Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("无法确定 LOCALAPPDATA")?;
    let directory = local.join("znnz-client").join("logs");
    fs::create_dir_all(&directory).context("无法创建日志目录")?;
    Ok(directory)
}

fn current_log_timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn next_task_log_path(slug: &str) -> Result<PathBuf> {
    Ok(logs_directory()?.join(format!(
        "task-{}-{}-{slug}.log",
        current_log_timestamp(),
        std::process::id()
    )))
}

fn create_session_log_file() -> Result<PathBuf> {
    let path = logs_directory()?.join(format!(
        "gui-session-{}-{}.log",
        current_log_timestamp(),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("无法创建会话汇总日志 {}", path.display()))?;
    file.write_all(b"\xEF\xBB\xBF")
        .context("无法初始化 UTF-8 会话汇总日志")?;
    Ok(path)
}

fn write_session_log_sections(
    path: &std::path::Path,
    sections: &[CompletedLogSection],
) -> Result<()> {
    let content = join_ordered_log_sections(
        sections
            .iter()
            .map(|section| (section.sequence, section.text.clone())),
    );
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .with_context(|| format!("无法重写会话汇总日志 {}", path.display()))?;
    file.write_all(b"\xEF\xBB\xBF")
        .context("无法初始化 UTF-8 会话汇总日志")?;
    file.write_all(content.as_bytes())?;
    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn native_popup_style_keeps_shell_minimize_support() {
        let style = clean_native_window_style(
            WS_CAPTION.0 | WS_BORDER.0 | WS_THICKFRAME.0 | WS_MAXIMIZEBOX.0,
        );

        assert_ne!(style & WS_POPUP.0, 0);
        assert_ne!(style & WS_SYSMENU.0, 0);
        assert_ne!(style & WS_MINIMIZEBOX.0, 0);
        assert_eq!(style & WS_CAPTION.0, 0);
        assert_eq!(style & WS_BORDER.0, 0);
        assert_eq!(style & WS_THICKFRAME.0, 0);
        assert_eq!(style & WS_MAXIMIZEBOX.0, 0);
    }

    #[test]
    fn task_log_metadata_uses_stable_titles_and_ascii_slugs() {
        assert_eq!(
            task_log_title(
                TaskKind::Client(ClientTarget::CodexDesktop),
                "Codex Desktop"
            ),
            "Codex Desktop • 配置 • 启动"
        );
        assert_eq!(
            task_log_title(TaskKind::Install(ClientTarget::ClaudeDesktop), "ignored"),
            "Claude Desktop • 安装"
        );
        assert_eq!(
            task_log_title(TaskKind::GatewayTest, "拉取模型"),
            "网关 • 拉取模型"
        );
        assert_eq!(
            task_log_slug(TaskKind::Install(ClientTarget::ClaudeDesktop), "ignored"),
            "claude-desktop-install"
        );
        assert_eq!(
            task_log_slug(TaskKind::GatewayTest, "连接测试"),
            "gateway-test"
        );
    }

    #[test]
    fn log_formatter_does_not_duplicate_existing_title() {
        let formatted = format_log_section(
            "Claude Desktop • 安装",
            "\u{feff}===== Claude Desktop • 安装 =====\r\n第一段\r\n\r\n\r\n第二段\r\n\r\n------------------------------\r\n",
        );
        assert_eq!(
            formatted,
            "===== Claude Desktop • 安装 =====\n\n第一段\n\n第二段\n\n------------------------------"
        );
        assert_eq!(formatted.matches("=====").count(), 2);
        assert_eq!(formatted.matches(LOG_SECTION_FOOTER).count(), 1);
    }

    #[test]
    fn log_formatter_indents_inline_error_details() {
        assert_eq!(
            format_log_section("Claude Desktop • 安装", "错误: 下载超时"),
            "===== Claude Desktop • 安装 =====\n\n错误：\n  下载超时\n\n------------------------------"
        );
    }

    #[test]
    fn log_sections_keep_creation_order_and_requested_spacing() {
        let first = format_log_section("Codex Desktop • 配置 • 启动", "日志段落1\n\n日志段落2");
        let second = format_log_section("Codex Desktop • 安装", "日志段落1\n\n日志段落2");
        let history = join_ordered_log_sections(vec![(2, second), (1, first)]);
        assert_eq!(
            history,
            "===== Codex Desktop • 配置 • 启动 =====\n\n日志段落1\n\n日志段落2\n\n------------------------------\n\n\n===== Codex Desktop • 安装 =====\n\n日志段落1\n\n日志段落2\n\n------------------------------"
        );
    }

    #[test]
    fn task_keeps_the_same_position_after_it_finishes() {
        let first_running = format_log_section("第一个任务", "（等待输出）");
        let second_running = format_log_section("第二个任务", "正在运行");
        let before =
            join_ordered_log_sections(vec![(1, first_running), (2, second_running.clone())]);
        let first_completed = format_log_section("第一个任务", "已经完成");
        let after = join_ordered_log_sections(vec![(2, second_running), (1, first_completed)]);

        assert!(before.find("第一个任务").unwrap() < before.find("第二个任务").unwrap());
        assert!(after.find("第一个任务").unwrap() < after.find("第二个任务").unwrap());
    }

    #[test]
    fn installer_progress_uses_last_non_empty_log_line() {
        assert_eq!(
            latest_install_progress("第一行\n\n正在切换国内镜像……\n").as_deref(),
            Some("正在切换国内镜像……")
        );
    }

    #[test]
    fn cascade_flow_svg_rasterizes() {
        let rgba = render_svg_rgba(
            include_bytes!("../assets/gui/svg/cascade-flow.svg"),
            layout::CASCADE_FLOW_WIDTH as u32 * 4,
            layout::CASCADE_FLOW_HEIGHT as u32 * 4,
        )
        .expect("cascade-flow.svg should rasterize");
        assert!(rgba.chunks_exact(4).any(|pixel| pixel[3] != 0));
    }

    #[test]
    fn input_hint_text_uses_requested_rgb() {
        assert_eq!(
            input_hint_text_color(),
            Color32::from_rgba_unmultiplied(109, 115, 128, 179)
        );
    }

    #[test]
    fn cascade_animation_matches_updated_staggered_svg() {
        let mut branch_was_visible = [false; 4];
        for step in 0..2400 {
            let cycle = step as f32 / 2400.0;
            let opacities = [
                cascade_group_opacity(0, cycle),
                cascade_group_opacity(1, cycle),
                cascade_group_opacity(2, cycle),
                cascade_group_opacity(3, cycle),
            ];
            assert!(
                opacities.iter().copied().fold(0.0_f32, f32::max) > 0.001,
                "updated SVG should not have a global blank window at cycle={cycle}"
            );
            for (index, opacity) in opacities.into_iter().enumerate() {
                branch_was_visible[index] |= opacity > 0.99;
            }
        }
        assert!(branch_was_visible.into_iter().all(|visible| visible));

        assert!(cascade_dot_opacity(0.0) < 0.001);
        assert!((cascade_dot_opacity(0.06) - 0.5).abs() < 0.001);
        assert!((cascade_dot_opacity(0.5) - 1.0).abs() < 0.001);
        assert!((cascade_dot_opacity(0.86) - 0.5).abs() < 0.001);
    }

    #[test]
    fn inherited_runtime_task_keeps_client_running_indicator_on() {
        let tasks = vec![RunningTask {
            sequence: 1,
            kind: TaskKind::Client(ClientTarget::CodexDesktop),
            child: None,
            worker_pid: 4321,
            log_path: PathBuf::from(r"C:\Temp\codex-desktop.log"),
            label: "Codex Desktop".to_owned(),
            log_title: "Codex Desktop • 配置 • 启动".to_owned(),
            log_text: String::new(),
            last_log_read: UNIX_EPOCH,
        }];

        assert!(tasks_contain_client(&tasks, ClientTarget::CodexDesktop));
        assert!(!tasks_contain_client(&tasks, ClientTarget::ClaudeDesktop));
    }

    #[test]
    fn client_card_state_prefers_running_over_installation() {
        assert_eq!(client_card_state_label(false, false), "未安装");
        assert_eq!(client_card_state_label(true, false), "已安装");
        assert_eq!(client_card_state_label(true, true), "已启动");
        assert_eq!(client_card_state_label(false, true), "已启动");
    }

    #[test]
    fn selected_client_status_reports_installation_and_runtime() {
        assert_eq!(
            selected_client_status(ClientTarget::ClaudeCode, ModelListMode::BuiltIn, true, true,),
            "Claude Code • 已安装 • 默认模型列表 • 运行中 ..."
        );
        assert_eq!(
            selected_client_status(
                ClientTarget::ClaudeCode,
                ModelListMode::Gateway,
                true,
                false,
            ),
            "Claude Code • 已安装 • 网关模型列表"
        );
        assert_eq!(
            selected_client_status(
                ClientTarget::ClaudeCode,
                ModelListMode::Gateway,
                false,
                false,
            ),
            "Claude Code • 未安装 • 网关模型列表"
        );
    }

    #[test]
    fn english_running_status_omits_redundant_installed_text() {
        assert_eq!(
            selected_client_status_for_language(
                ClientTarget::CodexDesktop,
                ModelListMode::Gateway,
                true,
                true,
                i18n::Language::En,
            ),
            "Codex Desktop • Gateway models • Running ..."
        );
        assert_eq!(
            selected_client_status_for_language(
                ClientTarget::CodexDesktop,
                ModelListMode::Gateway,
                true,
                false,
                i18n::Language::En,
            ),
            "Codex Desktop • Installed • Gateway models"
        );
    }

    #[test]
    fn client_exit_status_uses_the_real_client_name() {
        assert_eq!(
            client_exited_status(ClientTarget::ClaudeCode),
            "Claude Code • 已退出"
        );
        assert_eq!(
            client_exited_status(ClientTarget::CodexCli),
            "Codex CLI • 已退出"
        );
    }

    #[test]
    fn gateway_success_status_uses_dot_separators() {
        let task = RunningTask {
            sequence: 1,
            kind: TaskKind::GatewayTest,
            child: None,
            worker_pid: 0,
            log_path: PathBuf::new(),
            label: "测试连接".to_owned(),
            log_title: "网关 • 连接测试".to_owned(),
            log_text: "成功连接 • https://api.example.com".to_owned(),
            last_log_read: UNIX_EPOCH,
        };
        assert_eq!(
            completed_task_status(&task),
            "成功连接 • https://api.example.com"
        );
    }

    #[test]
    fn gateway_success_highlights_action_and_visible_model_count() {
        let text = "成功拉取 • 31 个可见模型 • https://api.example.com";
        let ranges = status_highlight_ranges(text);
        let highlighted = ranges
            .iter()
            .map(|(start, end, color)| (&text[*start..*end], *color))
            .collect::<Vec<_>>();
        assert!(highlighted.contains(&("成功拉取", GREEN)));
        assert!(highlighted.contains(&("31", GREEN)));
    }

    #[test]
    fn model_mode_status_uses_blue_regular_highlight_range() {
        let text = "Codex Desktop • Gateway models • Running ...";
        let ranges = status_highlight_ranges(text);
        let highlighted = ranges
            .iter()
            .map(|(start, end, color)| (&text[*start..*end], *color))
            .collect::<Vec<_>>();
        assert!(highlighted.contains(&("Gateway models", MODEL_MODE_STATUS)));
        assert!(highlighted.contains(&("Running", GREEN)));
    }

    #[test]
    fn blocked_custom_gateway_uses_concise_status_message() {
        let task = RunningTask {
            sequence: 1,
            kind: TaskKind::GatewayTest,
            child: None,
            worker_pid: 0,
            log_path: PathBuf::new(),
            label: "连接测试".to_owned(),
            log_title: "网关 • 连接测试".to_owned(),
            log_text: format!(
                "错误: 无法连接模型接口。{}，请配置代理后重试",
                crate::gateway::MAINLAND_ACCESS_ERROR
            ),
            last_log_read: UNIX_EPOCH,
        };
        assert_eq!(
            failed_task_status(&task),
            crate::gateway::MAINLAND_ACCESS_ERROR
        );
    }

    #[test]
    fn client_tooltip_labels_match_terminal_and_desktop_clients() {
        assert_eq!(client_kind_label(ClientTarget::CodexCli), "终端 Agent");
        assert_eq!(client_kind_label(ClientTarget::CodexDesktop), "桌面 Agent");
        assert_eq!(
            client_launch_requires_install_status(ClientTarget::ClaudeCode),
            "Claude Code • 未安装 • 请先安装客户端"
        );
    }

    #[test]
    fn gateway_input_validation_rejects_insecure_remote_http() {
        assert!(validate_gateway_inputs("http://api.example.com", "sk-test").is_err());
        assert!(validate_gateway_inputs("http://127.0.0.1:8080", "sk-test").is_ok());
        assert!(validate_gateway_inputs("https://api.example.com/", "sk-test").is_ok());
        assert!(validate_gateway_inputs("https://api.example.com", "").is_err());
    }

    #[test]
    fn empty_gateway_input_hides_the_url_parser_detail() {
        let error = validate_gateway_inputs("", "sk-test").unwrap_err();
        assert_eq!(format!("{error:#}"), "接口地址格式无效 !");
        assert!(!format!("{error:#}").contains("relative URL"));
    }

    #[test]
    fn worker_arguments_never_contain_api_key() {
        let key = "sk-secret-that-must-not-appear";
        let arguments = client_worker_arguments(
            ClientTarget::CodexDesktop,
            "https://api.example.com/",
            ModelListMode::Gateway,
        );
        assert_eq!(
            arguments.last().map(String::as_str),
            Some("https://api.example.com")
        );
        assert!(!arguments.iter().any(|argument| argument.contains(key)));
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--claude-desktop-model-mode")
        );
    }

    #[test]
    fn worker_arguments_include_selected_model_list_mode() {
        let arguments = client_worker_arguments(
            ClientTarget::ClaudeDesktop,
            "https://api.example.com/",
            ModelListMode::BuiltIn,
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--model-list-mode", "built-in"])
        );
    }
    #[test]
    fn all_embedded_svg_assets_render() {
        let assets: &[(&str, &[u8], u32, u32)] = &[
            (
                "logo",
                include_bytes!("../assets/gui/svg/logo.svg"),
                120,
                120,
            ),
            (
                "update",
                include_bytes!("../assets/gui/svg/Update.svg"),
                48,
                48,
            ),
            (
                "github",
                include_bytes!("../assets/gui/svg/GitHub.svg"),
                48,
                48,
            ),
            ("url", include_bytes!("../assets/gui/svg/url.svg"), 64, 64),
            ("key", include_bytes!("../assets/gui/svg/key.svg"), 64, 64),
            ("link", include_bytes!("../assets/gui/svg/link.svg"), 64, 64),
            (
                "models",
                include_bytes!("../assets/gui/svg/models.svg"),
                64,
                64,
            ),
            (
                "install",
                include_bytes!("../assets/gui/svg/install.svg"),
                64,
                64,
            ),
            (
                "installing",
                include_bytes!("../assets/gui/svg/Installing.svg"),
                64,
                64,
            ),
            (
                "start",
                include_bytes!("../assets/gui/svg/start.svg"),
                64,
                64,
            ),
            (
                "prompt",
                include_bytes!("../assets/gui/svg/prompt.svg"),
                64,
                64,
            ),
            (
                "promptwarn",
                include_bytes!("../assets/gui/svg/promptwarn.svg"),
                64,
                64,
            ),
            (
                "codex-cli",
                include_bytes!("../assets/gui/svg/codex-cli.svg"),
                160,
                160,
            ),
            (
                "codex-desktop",
                include_bytes!("../assets/gui/svg/codex-desktop.svg"),
                160,
                160,
            ),
            (
                "claude-code",
                include_bytes!("../assets/gui/svg/claude-code.svg"),
                160,
                160,
            ),
            (
                "claude-desktop",
                include_bytes!("../assets/gui/svg/claude-desktop.svg"),
                160,
                160,
            ),
            (
                "badge-cli",
                include_bytes!("../assets/gui/svg/badge-cli.svg"),
                96,
                96,
            ),
            (
                "badge-desktop",
                include_bytes!("../assets/gui/svg/badge-desktop.svg"),
                96,
                96,
            ),
            (
                "eye-show",
                include_bytes!("../assets/gui/svg/eye-show.svg"),
                64,
                64,
            ),
            (
                "eye-hide",
                include_bytes!("../assets/gui/svg/eye-hide.svg"),
                64,
                64,
            ),
            (
                "select",
                include_bytes!("../assets/gui/svg/select.svg"),
                64,
                64,
            ),
            ("log", include_bytes!("../assets/gui/svg/log.svg"), 72, 72),
            (
                "window-control-glyphs",
                include_bytes!("../assets/gui/svg/window-control-glyphs.svg"),
                192,
                64,
            ),
            (
                "client-card-normal",
                include_bytes!("../assets/gui/svg/client-card-normal.svg"),
                100,
                100,
            ),
            (
                "client-card-hover",
                include_bytes!("../assets/gui/svg/client-card-hover.svg"),
                100,
                100,
            ),
            (
                "client-card-selected",
                include_bytes!("../assets/gui/svg/client-card-selected.svg"),
                100,
                100,
            ),
            (
                "client-card-running",
                include_bytes!("../assets/gui/svg/client-card-running.svg"),
                100,
                100,
            ),
        ];

        for (name, bytes, width, height) in assets {
            let rgba = render_svg_rgba(bytes, *width, *height)
                .unwrap_or_else(|error| panic!("{name}.svg 渲染失败: {error:#}"));
            assert_eq!(rgba.len(), (*width * *height * 4) as usize);
            assert!(
                rgba.chunks_exact(4).any(|pixel| pixel[3] != 0),
                "{name}.svg 渲染结果完全透明"
            );
        }
    }
}
