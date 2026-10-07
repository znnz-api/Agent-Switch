use crate::gui_layout as layout;
use crate::gui_settings::{self, GuiPreferences};
use crate::gui_worker::{ClientTarget, ModelListMode};
use crate::i18n;
use crate::local_gateway;
use crate::runtime_state::{self, ClientRuntimeState};
use crate::usage;
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
    Foundation::{FILETIME, HWND, LPARAM, LRESULT, RECT, SYSTEMTIME, WPARAM},
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
    System::{
        Threading::GetCurrentThreadId,
        Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
    },
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
const LOG_SECTION_FOOTER: &str = "=========================================";
// Keep one empty line between completed runtime-log sections.
const LOG_SECTION_SEPARATOR: &str = "\n\n";
pub const WINDOW_TITLE: &str = if crate::gateway::CUSTOM_EDITION {
    "znnz • Agent-Switch • v1.1"
} else {
    "Agent • Switch • v1.1"
};
const LOGO_URL: &str = "https://r.networkpe.top/Agent-Switch-Logo";
const UPDATE_URL: &str = "https://r.networkpe.top/Agent-Switch-Update";
const GITHUB_URL: &str = "https://r.networkpe.top/Agent-Switch-GitHub";
const STAR_URL: &str = "https://r.networkpe.top/Agent-Switch-star";
const HELP_URL: &str = "https://r.networkpe.top/Agent-Switch-help";
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
    let _background = crate::background::Host::start()?;
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
        Box::new(|creation| Ok(Box::new(LauncherApp::new(creation)?))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
    .with_context(|| format!("无法初始化 {GUI_RENDERER_NAME} 图形渲染器"))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    GatewayTest,
    GatewayUpdate(ClientTarget),
    Client(ClientTarget),
    Install(ClientTarget),
    AccountMode(ClientTarget),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogView {
    Runtime,
    Model,
}

struct RunningTask {
    configuration_fingerprint: Option<String>,
    sequence: u64,
    kind: TaskKind,
    child: Option<Child>,
    job: Option<crate::background::Job>,
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
    client_card_hover_selected: TextureHandle,
    client_card_running: TextureHandle,
    client_card_running_connected: TextureHandle,
    logo: TextureHandle,
    update: TextureHandle,
    github: TextureHandle,
    star: TextureHandle,
    help: TextureHandle,
    url: TextureHandle,
    server: TextureHandle,
    key: TextureHandle,
    statistics: TextureHandle,
    link: TextureHandle,
    convert: TextureHandle,
    converting: Vec<TextureHandle>,
    models: TextureHandle,
    install: TextureHandle,
    login_mode: TextureHandle,
    spinner: Vec<TextureHandle>,
    providers: Vec<TextureHandle>,
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
    history_expand: TextureHandle,
    history_delete: TextureHandle,
    history_edit: TextureHandle,
    history_confirm: TextureHandle,
    select: TextureHandle,
    select_green: TextureHandle,
    copy: TextureHandle,
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
            providers: Vec::new(),
            client_card_normal: load_embedded_svg_texture(
                context,
                "gui-client-card-normal-svg",
                include_bytes!("../assets/gui/svg/client-card-normal.svg"),
                Vec2::new(layout::CLIENT_CARD_WIDTH, layout::CLIENT_CARD_HEIGHT),
            ),
            client_card_hover_selected: load_embedded_svg_texture(
                context,
                "gui-client-card-hover-selected-svg",
                include_bytes!("../assets/gui/svg/client-card-hover-selected.svg"),
                Vec2::new(layout::CLIENT_CARD_WIDTH, layout::CLIENT_CARD_HEIGHT),
            ),
            client_card_running: load_embedded_svg_texture(
                context,
                "gui-client-card-running-svg",
                include_bytes!("../assets/gui/svg/client-card-running.svg"),
                Vec2::new(layout::CLIENT_CARD_WIDTH, layout::CLIENT_CARD_HEIGHT),
            ),
            client_card_running_connected: load_embedded_svg_texture(
                context,
                "gui-client-card-running-connected-svg",
                include_bytes!("../assets/gui/svg/client-card-running-connected.svg"),
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
            star: load_embedded_svg_texture(
                context,
                "gui-star-svg",
                include_bytes!("../assets/gui/svg/star.svg"),
                Vec2::splat(layout::HEADER_SUBTITLE_ICON_SIZE),
            ),
            help: load_embedded_svg_texture(
                context,
                "gui-help-svg",
                include_bytes!("../assets/gui/svg/help.svg"),
                Vec2::splat(layout::HEADER_SUBTITLE_ICON_SIZE),
            ),
            url: load_embedded_svg_texture(
                context,
                "gui-url-svg",
                include_bytes!("../assets/gui/svg/url.svg"),
                Vec2::splat(16.0),
            ),
            server: load_embedded_svg_texture(
                context,
                "gui-server-svg",
                include_bytes!("../assets/gui/svg/server.svg"),
                Vec2::splat(16.0),
            ),
            key: load_embedded_svg_texture(
                context,
                "gui-key-svg",
                include_bytes!("../assets/gui/svg/key.svg"),
                Vec2::splat(16.0),
            ),
            statistics: load_embedded_svg_texture(
                context,
                "gui-statistics-svg",
                include_bytes!("../assets/gui/svg/statistics.svg"),
                Vec2::splat(layout::HISTORY_USAGE_ICON_SIZE),
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
            convert: load_embedded_svg_texture(
                context,
                "gui-convert-svg",
                include_bytes!("../assets/gui/svg/Convert.svg"),
                Vec2::new(
                    layout::CONNECT_ACTION_ICON_WIDTH,
                    layout::CONNECT_ACTION_ICON_HEIGHT,
                ),
            ),
            converting: crate::gui_convert::frames()
                .into_iter()
                .enumerate()
                .map(|(index, svg)| {
                    load_embedded_svg_texture(
                        context,
                        &format!("gui-converting-{index}"),
                        svg.as_bytes(),
                        Vec2::new(
                            layout::CONNECT_ACTION_ICON_WIDTH,
                            layout::CONNECT_ACTION_ICON_HEIGHT,
                        ),
                    )
                })
                .collect(),
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
            login_mode: load_embedded_svg_texture(
                context,
                "gui-login-mode-svg",
                include_bytes!("../assets/gui/svg/Login-mode.svg"),
                Vec2::new(
                    layout::MODEL_INSTALL_ICON_WIDTH,
                    layout::MODEL_INSTALL_ICON_HEIGHT,
                ),
            ),
            spinner: crate::gui_spinner::frames()
                .into_iter()
                .enumerate()
                .map(|(index, svg)| {
                    load_embedded_svg_texture(
                        context,
                        &format!("gui-spinner-{index}"),
                        svg.as_bytes(),
                        Vec2::splat(layout::ACTION_SPINNER_SIZE),
                    )
                })
                .collect(),
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
            history_expand: load_embedded_svg_texture(
                context,
                "gui-history-expand",
                include_bytes!("../assets/gui/svg/history-expand.svg"),
                Vec2::splat(16.0),
            ),
            history_edit: load_embedded_svg_texture(
                context,
                "gui-history-edit",
                include_bytes!("../assets/gui/svg/history-edit.svg"),
                Vec2::splat(16.0),
            ),
            history_confirm: load_embedded_svg_texture(
                context,
                "gui-history-confirm",
                include_bytes!("../assets/gui/svg/history-confirm.svg"),
                Vec2::splat(16.0),
            ),
            history_delete: load_embedded_svg_texture(
                context,
                "gui-history-delete",
                include_bytes!("../assets/gui/svg/history-delete.svg"),
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
            select_green: load_embedded_svg_texture(
                context,
                "gui-select-green-svg",
                include_bytes!("../assets/gui/svg/select-green.svg"),
                Vec2::splat(layout::MODEL_MENU_CHECK_ICON_SIZE),
            ),
            copy: load_embedded_svg_texture(
                context,
                "gui-copy-svg",
                include_bytes!("../assets/gui/svg/copy.svg"),
                Vec2::splat(layout::MODEL_MENU_ACTION_ICON_SIZE),
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
            // Provider textures are loaded asynchronously and are not part of
            // the embedded asset bundle. Keep them across a DPI-triggered
            // reload; otherwise the next frame can still contain providers
            // while the texture vector is empty.
            let provider_textures = std::mem::take(&mut self.providers);
            let mut reloaded = Self::load(context);
            reloaded.providers = provider_textures;
            *self = reloaded;
        }
    }

    fn client_card_background(
        &self,
        selected: bool,
        running: bool,
        attached: bool,
        hovered: bool,
    ) -> &TextureHandle {
        if running && attached {
            &self.client_card_running_connected
        } else if running {
            &self.client_card_running
        } else if selected || hovered {
            &self.client_card_hover_selected
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
    /// The normalized URL used by workers and saved preferences.
    gateway_url: String,
    /// Text shown in the URL input. A named history entry uses its friendly
    /// name here while `gateway_url` continues to hold the real endpoint.
    gateway_url_display: String,
    conversion_enabled: bool,
    preset_provider: Option<String>,
    history_delete_guard: Option<(String, Instant)>,
    providers: Vec<crate::gui_providers::Provider>,
    provider_job: Option<crate::background::ProviderJob>,
    api_key: Zeroizing<String>,
    show_key: bool,
    show_history: bool,
    editing_history: Option<String>,
    history_name: String,
    history_name_focus: bool,
    show_models: bool,
    fetched_models: Vec<String>,
    fetched_models_configuration: Option<String>,
    history: Vec<gui_settings::GatewayHistoryEntry>,
    client_configurations: gui_settings::ClientConfigurations,
    usage_summaries: std::collections::HashMap<String, usage::Summary>,
    usage_reader: usage::SummaryReader,
    model_log_reader: usage::ModelLogReader,
    model_log_text: String,
    usage_available: bool,
    remember_key: bool,
    selected: ClientTarget,
    client_selected: bool,
    codex_cli_model_mode: ModelListMode,
    codex_desktop_model_mode: ModelListMode,
    claude_code_model_mode: ModelListMode,
    claude_desktop_model_mode: ModelListMode,
    running: Vec<RunningTask>,
    latest_client_jobs: std::collections::HashMap<ClientTarget, u64>,
    client_processes: Vec<(ClientTarget, u32)>,
    attached_clients: Vec<ClientTarget>,
    attached_urls: Vec<(ClientTarget, String)>,
    attached_configurations: Vec<(ClientTarget, String)>,
    installed_clients: ClientInstallations,
    gateway_managed_clients: Vec<ClientTarget>,
    status: String,
    /// Raw result status and the configuration captured when its worker started.
    gateway_result_configuration: Option<(String, String)>,
    status_is_warning: bool,
    log_text: String,
    completed_logs: Vec<CompletedLogSection>,
    next_log_sequence: u64,
    session_log_path: Option<PathBuf>,
    assets: GuiAssets,
    show_logs: bool,
    log_view: LogView,
    #[cfg(windows)]
    native_window: Option<NativeWindowState>,
    #[cfg(windows)]
    tray: crate::tray::Tray,
    allow_exit: bool,
    last_runtime_scan: SystemTime,
    last_installation_scan: SystemTime,
    last_usage_scan: SystemTime,
    last_model_log_scan: SystemTime,
    preferences_dirty: bool,
    preferences_changed_at: Option<Instant>,
    /// The most recent statistics popup rectangle. It is retained for one
    /// frame so clicking its scrollbar/text does not close the history menu.
    history_tooltip_rect: Option<Rect>,
    history_tooltip_entry: Option<String>,
    history_hover_started_at: f64,
}

impl LauncherApp {
    fn new(creation: &eframe::CreationContext<'_>) -> Result<Self> {
        #[cfg(windows)]
        let native_window = NativeWindowState::configure(creation);
        #[cfg(windows)]
        let tray = crate::tray::Tray::new(
            native_window.as_ref().context("无法取得托盘所属窗口")?.hwnd,
            creation.egui_ctx.clone(),
        )?;
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
                        "读取本地设置失败、将使用默认值",
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
        let usage_summaries = std::collections::HashMap::new();
        let (history, warning) = match gui_settings::load_history() {
            Ok(history) => (history, warning),
            Err(error) => (
                Vec::new(),
                Some(format!(
                    "{}: {error:#}",
                    i18n::tr("读取配置历史失败", "Failed to load configuration history")
                )),
            ),
        };
        let (client_configurations, warning) = match gui_settings::load_client_configurations() {
            Ok(configurations) => (configurations, warning),
            Err(error) => (
                gui_settings::ClientConfigurations::new(),
                Some(format!(
                    "{}: {error:#}",
                    i18n::tr(
                        "读取客户端配置记忆失败",
                        "Failed to load remembered client configurations"
                    )
                )),
            ),
        };
        let had_settings_warning = warning.is_some();
        let conversion_enabled = gui_settings::conversion_for_configuration(
            &preferences.gateway_url,
            key.as_ref().map(|value| value.as_str()).unwrap_or(""),
        )
        .unwrap_or(false);
        let mut app = Self {
            gateway_url: preferences.gateway_url.clone(),
            gateway_url_display: preferences.gateway_url,
            conversion_enabled,
            preset_provider: preferences.preset_provider,
            history_delete_guard: None,
            providers: Vec::new(),
            provider_job: None,
            api_key: key.unwrap_or_else(|| Zeroizing::new(String::new())),
            show_key: false,
            show_history: false,
            editing_history: None,
            history_name: String::new(),
            history_name_focus: false,
            show_models: false,
            fetched_models: Vec::new(),
            fetched_models_configuration: None,
            history,
            client_configurations,
            usage_summaries,
            usage_reader: usage::SummaryReader::new(),
            model_log_reader: usage::ModelLogReader::new(),
            model_log_text: String::new(),
            usage_available: false,
            remember_key: true,
            selected,
            client_selected: false,
            codex_cli_model_mode,
            codex_desktop_model_mode,
            claude_code_model_mode,
            claude_desktop_model_mode,
            running: Vec::new(),
            latest_client_jobs: std::collections::HashMap::new(),
            client_processes: Vec::new(),
            attached_clients: Vec::new(),
            attached_urls: Vec::new(),
            attached_configurations: Vec::new(),
            installed_clients,
            gateway_managed_clients: Vec::new(),
            status: warning.unwrap_or_else(|| default_status().to_owned()),
            gateway_result_configuration: None,
            status_is_warning: had_settings_warning,
            log_text: String::new(),
            completed_logs: Vec::new(),
            next_log_sequence: 1,
            session_log_path,
            assets,
            show_logs: false,
            log_view: LogView::Runtime,
            #[cfg(windows)]
            native_window,
            #[cfg(windows)]
            tray,
            allow_exit: false,
            last_runtime_scan: UNIX_EPOCH,
            last_installation_scan: SystemTime::now(),
            last_usage_scan: UNIX_EPOCH,
            last_model_log_scan: UNIX_EPOCH,
            preferences_dirty: false,
            preferences_changed_at: None,
            history_tooltip_rect: None,
            history_tooltip_entry: None,
            history_hover_started_at: 0.0,
        };
        app.refresh_gateway_display();
        app.provider_job = crate::background::start_provider_job().ok();
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
            app.status_is_warning = true;
        }
        Ok(app)
    }

    fn validate_inputs(&self) -> Result<()> {
        validate_gateway_inputs(&self.gateway_url, &self.api_key)
    }

    fn refresh_gateway_display(&mut self) {
        self.gateway_url_display = self
            .history
            .iter()
            .find(|entry| {
                entry.gateway_url == normalized_gateway_url(&self.gateway_url)
                    && entry.api_key == self.api_key.trim()
            })
            .map(|entry| entry.display_name().to_owned())
            .unwrap_or_else(|| {
                self.providers
                    .iter()
                    .find(|provider| {
                        Some(provider.name.as_str()) == self.preset_provider.as_deref()
                            && provider.endpoint == self.gateway_url.trim_end_matches('/')
                    })
                    .map_or_else(
                        || {
                            self.preset_provider
                                .clone()
                                .unwrap_or_else(|| self.gateway_url.clone())
                        },
                        |provider| provider.name.clone(),
                    )
            });
    }

    fn select_history_configuration(&mut self, index: usize) {
        let entry = &self.history[index];
        let changed = normalized_gateway_url(&self.gateway_url)
            != normalized_gateway_url(&entry.gateway_url)
            || self.api_key.trim() != entry.api_key.trim();
        self.preset_provider = None;
        self.gateway_url.clone_from(&entry.gateway_url);
        *self.api_key = entry.api_key.clone();
        self.conversion_enabled = entry.conversion_enabled;
        self.refresh_gateway_display();
        self.show_key = false;
        if changed {
            self.show_models = false;
            self.fetched_models.clear();
            self.fetched_models_configuration = None;
        }
        self.mark_preferences_dirty();
    }

    fn select_client_configuration(&mut self, target: ClientTarget) {
        let fingerprint = self
            .attached_configurations
            .iter()
            .find(|(client, _)| *client == target)
            .map(|(_, fingerprint)| fingerprint.as_str());
        if self.client_is_attached(target) && fingerprint.is_none() {
            return;
        }
        if let Some(configuration) = configuration_for_client(
            &self.history,
            &self.client_configurations,
            target,
            if self.client_is_attached(target) {
                fingerprint
            } else {
                None
            },
        ) {
            let changed = normalized_gateway_url(&self.gateway_url)
                != normalized_gateway_url(&configuration.gateway_url)
                || self.api_key.trim() != configuration.api_key.trim();
            self.gateway_url.clone_from(&configuration.gateway_url);
            *self.api_key = configuration.api_key.clone();
            self.preset_provider = configuration.preset_provider.clone();
            self.refresh_gateway_display();
            self.restore_conversion_setting();
            self.show_key = false;
            if changed {
                self.show_models = false;
                self.fetched_models.clear();
                self.fetched_models_configuration = None;
            }
        }
    }

    fn remember_client_inputs(&mut self) {
        if self.client_selected {
            self.client_configurations.insert(
                self.selected.id().to_owned(),
                gui_settings::ClientConfiguration {
                    gateway_url: self.gateway_url.clone(),
                    api_key: self.api_key.to_string(),
                    preset_provider: self.preset_provider.clone(),
                },
            );
        }
    }

    fn restore_conversion_setting(&mut self) {
        self.conversion_enabled = self.history.iter().any(|entry| {
            normalized_gateway_url(&entry.gateway_url) == normalized_gateway_url(&self.gateway_url)
                && entry.api_key.trim() == self.api_key.trim()
                && entry.conversion_enabled
        }) && conversion_inputs_present(
            &self.gateway_url_display,
            &self.gateway_url,
            &self.api_key,
        );
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
        self.latest_client_jobs.insert(state.client, sequence);
        self.running.push(RunningTask {
            configuration_fingerprint: local_gateway::attached_configuration_fingerprint(
                state.client,
            ),
            sequence,
            kind,
            child: None,
            job: None,
            worker_pid: state.worker_pid,
            log_path: state.log_path,
            label: state.client.title().to_owned(),
            log_title: task_log_title(kind, state.client.title()),
            log_text: String::new(),
            last_log_read: UNIX_EPOCH,
        });
    }

    fn sync_runtime_states(&mut self) -> Result<()> {
        self.gateway_managed_clients = ClientTarget::ALL
            .into_iter()
            .filter(|target| {
                match crate::account_mode::is_managed(*target) {
                    Ok(managed) => managed,
                    Err(error) => {
                        tracing::warn!(
                            "Unable to check {} account mode: {error:#}",
                            target.title()
                        );
                        // An unreadable configuration must not be reported as account mode.
                        true
                    }
                }
            })
            .collect();
        self.client_processes = crate::platform::running_client_processes()?;
        local_gateway::sync_client_processes(&self.client_processes);
        self.attached_clients = ClientTarget::ALL
            .into_iter()
            .filter(|target| local_gateway::is_attached(*target))
            .collect();
        self.attached_urls = self
            .attached_clients
            .iter()
            .filter_map(|target| {
                local_gateway::attached_gateway_url(*target).map(|url| (*target, url))
            })
            .collect();
        self.attached_configurations = self
            .attached_clients
            .iter()
            .filter_map(|target| {
                local_gateway::attached_configuration_fingerprint(*target)
                    .map(|fingerprint| (*target, fingerprint))
            })
            .collect();
        let mut seeded_memory = false;
        for (target, fingerprint) in &self.attached_configurations {
            if !self.client_configurations.contains_key(target.id())
                && let Some(configuration) = configuration_for_client(
                    &self.history,
                    &self.client_configurations,
                    *target,
                    Some(fingerprint),
                )
            {
                self.client_configurations
                    .insert(target.id().to_owned(), configuration);
                seeded_memory = true;
            }
        }
        if seeded_memory {
            self.preferences_dirty = true;
            self.preferences_changed_at = Some(Instant::now());
        }
        let mut states = runtime_state::load_active_states()?;
        states.sort_by_key(|state| state.started_at_ms);
        self.last_runtime_scan = SystemTime::now();

        let mut completed_logs = Vec::new();
        self.running.retain_mut(|task| {
            if task.child.is_some()
                || task.job.is_some()
                || !matches!(task.kind, TaskKind::Client(_))
            {
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
        if !self.client_selected
            || self.status_is_warning
            || is_model_list_change_status(&self.status)
            || is_model_name_copied_status(&self.status)
        {
            return;
        }

        // 安装任务和网关检查运行时，状态栏用于展示实时进度，不在这里覆盖。
        if self.is_running(TaskKind::Install(self.selected))
            || self.is_running(TaskKind::AccountMode(self.selected))
            || self.is_running(TaskKind::GatewayTest)
        {
            return;
        }
        self.status = selected_client_status(
            self.selected,
            self.client_is_installed(self.selected),
            self.client_is_running(self.selected),
            self.client_is_attached(self.selected),
        );
        self.status_is_warning = false;
    }

    fn save_preferences(&self) -> Result<()> {
        let preferences = GuiPreferences {
            gateway_url: normalized_gateway_url(&self.gateway_url),
            preset_provider: self.preset_provider.clone(),
            selected_client: self.selected.id().to_owned(),
            remember_key: self.remember_key,
            codex_cli_model_mode: self.codex_cli_model_mode.id().to_owned(),
            codex_desktop_model_mode: self.codex_desktop_model_mode.id().to_owned(),
            claude_code_model_mode: self.claude_code_model_mode.id().to_owned(),
            claude_desktop_model_mode: self.claude_desktop_model_mode.id().to_owned(),
        };
        gui_settings::save(&preferences, &self.api_key)?;
        gui_settings::save_client_configurations(&self.client_configurations)
    }

    fn mark_preferences_dirty(&mut self) {
        self.remember_client_inputs();
        self.preferences_dirty = true;
        self.preferences_changed_at = Some(Instant::now());
    }

    fn remember_configuration(&mut self) -> Result<()> {
        self.validate_inputs()?;
        self.save_preferences()?;
        let mut history = self.history.clone();
        gui_settings::record_history(
            &mut history,
            &normalized_gateway_url(&self.gateway_url),
            &self.api_key,
        );
        if let Some(entry) = history.iter_mut().find(|entry| {
            entry.gateway_url == normalized_gateway_url(&self.gateway_url)
                && entry.api_key == self.api_key.trim()
        }) {
            entry.conversion_enabled = self.conversion_enabled;
        }
        if let Some(provider) = self.providers.iter().find(|provider| {
            Some(provider.name.as_str()) == self.preset_provider.as_deref()
                && provider.endpoint == self.gateway_url.trim_end_matches('/')
        }) && let Some(entry) = history.iter_mut().find(|entry| {
            entry.gateway_url == provider.endpoint && entry.api_key == self.api_key.trim()
        }) && entry.name.is_empty()
        {
            entry.name = provider.name.clone();
        }
        gui_settings::save_history(&history)?;
        self.history = history;
        self.preferences_dirty = false;
        self.preferences_changed_at = None;
        Ok(())
    }

    fn toggle_protocol_conversion(&mut self) {
        if let Err(error) = self.validate_inputs() {
            self.set_error(error);
            return;
        }
        if self.is_running(TaskKind::AccountMode(self.selected)) {
            return;
        }
        if self.is_running(TaskKind::GatewayUpdate(self.selected)) {
            self.status = i18n::tr(
                "协议转换正在更新，请稍后重试",
                "Protocol conversion is updating; try again shortly",
            )
            .to_owned();
            self.status_is_warning = true;
            return;
        }
        let previous = self.conversion_enabled;
        self.conversion_enabled = !self.conversion_enabled;
        if let Err(error) = self.remember_configuration() {
            self.conversion_enabled = previous;
            self.set_error(error);
            return;
        }
        let conversion_status = format!(
            "{} • {}",
            i18n::tr("协议转换", "Protocol conversion"),
            if self.conversion_enabled {
                i18n::tr("已开启", "Enabled")
            } else {
                i18n::tr("已关闭", "Disabled")
            }
        );
        if self.configuration_is_current(self.selected) {
            let arguments = gateway_update_arguments(self.selected, &self.gateway_url);
            match self.spawn_worker(
                TaskKind::GatewayUpdate(self.selected),
                self.selected.title(),
                &arguments,
                true,
            ) {
                Ok(task) => self.running.push(task),
                Err(error) => {
                    self.set_error(error);
                    return;
                }
            }
        }
        self.status = conversion_status;
        self.status_is_warning = false;
    }

    fn render_history(&mut self, ctx: &egui::Context, anchor: Rect) {
        if !self.show_history {
            self.editing_history = None;
            self.history_tooltip_entry = None;
            self.history_tooltip_rect = None;
            return;
        }
        let position = egui::pos2(
            anchor.left(),
            anchor.bottom() + layout::HISTORY_VERTICAL_GAP,
        );
        let size = egui::vec2(anchor.width(), layout::HISTORY_HEIGHT);
        let popup_rect = Rect::from_min_size(position, size);
        if ctx.input(|input| {
            input.key_pressed(egui::Key::Escape)
                || !input.focused
                || (input.pointer.any_pressed()
                    && input.pointer.interact_pos().is_some_and(|pos| {
                        !popup_rect.contains(pos)
                            && !anchor.contains(pos)
                            && !self
                                .history_tooltip_rect
                                .is_some_and(|tooltip| tooltip.contains(pos))
                    }))
        }) {
            self.show_history = false;
            self.editing_history = None;
            self.history_tooltip_entry = None;
            self.history_tooltip_rect = None;
            return;
        }
        let pointer_pos = ctx.input(|input| input.pointer.interact_pos());
        let tooltip_is_hovered = self
            .history_tooltip_rect
            .is_some_and(|tooltip| pointer_pos.is_some_and(|pos| tooltip.contains(pos)));
        let mut tooltip_candidate = None;
        let mut selected = None;
        let mut selected_provider = None;
        let mut removed = None;
        let mut close = false;
        let mut renamed = None;
        let mut reordered: Option<(String, String)> = None;
        egui::Area::new(egui::Id::new("gateway-history"))
            .order(egui::Order::Foreground)
            .fixed_pos(position)
            .default_size(size)
            .constrain(false)
            .fade_in(true)
            .show(ctx, |ui| {
                // Keep menu edges clear instead of dimming rows while scrolling.
                ui.spacing_mut().scroll.fade.strength = 0.0;
                let frame = egui::Frame::new()
                    .fill(history_background_color())
                    .shadow(egui::epaint::Shadow::NONE)
                    .corner_radius(layout::INPUT_CORNER_RADIUS)
                    .inner_margin(menu_frame_margin())
                    .show(ui, |ui| {
                        ui.set_width(
                            size.x - 2.0 * f32::from(layout::HISTORY_PADDING)
                                + f32::from(layout::SCROLLBAR_GUTTER),
                        );
                        ui.set_height(size.y - 2.0 * f32::from(layout::HISTORY_PADDING));
                        ui.spacing_mut().item_spacing.y = layout::HISTORY_ITEM_VERTICAL_GAP;
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .max_height(size.y - 2.0 * f32::from(layout::HISTORY_PADDING))
                            .show(ui, |ui| {
                                if !self.history.is_empty() {
                                    paint_provider_section_title(
                                        ui,
                                        i18n::tr("提供商记录", "Provider Records"),
                                        layout::PROVIDER_RECORDS_TITLE_LINE_LENGTH,
                                        layout::PROVIDER_RECORDS_TITLE_BOTTOM_GAP,
                                    );
                                }
                                for (index, entry) in self.history.iter().enumerate() {
                                    let (rect, _) = ui.allocate_exact_size(
                                        egui::vec2(
                                            ui.available_width(),
                                            layout::HISTORY_ITEM_HEIGHT,
                                        ),
                                        Sense::hover(),
                                    );
                                    let entry_id =
                                        usage::configuration_id(&entry.gateway_url, &entry.api_key);
                                    let editing =
                                        self.editing_history.as_deref() == Some(entry_id.as_str());
                                    let hovered = ui.rect_contains_pointer(rect);

                                    let active = entry.gateway_url
                                        == normalized_gateway_url(&self.gateway_url)
                                        && entry.api_key == self.api_key.trim();
                                    ui.painter().rect_filled(
                                        rect,
                                        layout::HISTORY_ITEM_CORNER_RADIUS,
                                        if editing {
                                            Color32::from_gray(layout::HISTORY_EDIT_BG)
                                        } else if hovered || active {
                                            history_item_active_background()
                                        } else {
                                            history_item_background_color()
                                        },
                                    );
                                    if editing {
                                        paint_surface_outline(
                                            ui.painter(),
                                            rect,
                                            layout::HISTORY_ITEM_CORNER_RADIUS,
                                        );
                                    }
                                    let dot_x = rect.right() - layout::HISTORY_ITEM_DOT_RIGHT;
                                    let delete_right =
                                        dot_x - 5.5 - layout::HISTORY_ITEM_DOT_ACTION_GAP
                                            + (layout::HISTORY_ITEM_DELETE_WIDTH - 16.0) / 2.0;
                                    let delete_rect = Rect::from_min_max(
                                        egui::pos2(
                                            delete_right - layout::HISTORY_ITEM_DELETE_WIDTH,
                                            rect.top(),
                                        ),
                                        egui::pos2(delete_right, rect.bottom()),
                                    );
                                    let edit_right = delete_rect.center().x
                                        - 8.0
                                        - layout::HISTORY_ITEM_ACTION_GAP
                                        + (layout::HISTORY_ITEM_EDIT_WIDTH - 16.0) / 2.0;
                                    let edit_rect = Rect::from_min_max(
                                        egui::pos2(
                                            edit_right - layout::HISTORY_ITEM_EDIT_WIDTH,
                                            rect.top(),
                                        ),
                                        egui::pos2(edit_right, rect.bottom()),
                                    );
                                    let in_use = configuration_has_running_client(
                                        &self.attached_configurations,
                                        &self.attached_clients,
                                        &self.client_processes,
                                        &entry.gateway_url,
                                        &entry.api_key,
                                    );
                                    let dot_rect = Rect::from_center_size(
                                        egui::pos2(dot_x, rect.center().y),
                                        Vec2::splat(18.0),
                                    );
                                    paint_connection_dot_scaled_with_radius(
                                        ui,
                                        dot_rect.center(),
                                        in_use,
                                        layout::INPUT_CONNECTION_DOT_SIZE / 11.0,
                                        layout::INPUT_CONNECTION_DOT_PULSE_RADIUS,
                                    );
                                    let text_rect = Rect::from_min_max(
                                        egui::pos2(
                                            rect.left()
                                                + layout::HISTORY_ITEM_ICON_LEFT
                                                + layout::HISTORY_ITEM_ICON_SIZE
                                                + layout::HISTORY_ITEM_ICON_TEXT_GAP,
                                            rect.top(),
                                        ),
                                        egui::pos2(
                                            if in_use || editing {
                                                delete_rect.left()
                                            } else {
                                                edit_rect.left()
                                            },
                                            rect.bottom(),
                                        ),
                                    );
                                    paint_svg_texture(
                                        ui,
                                        &self.assets.url,
                                        Rect::from_min_size(
                                            egui::pos2(
                                                rect.left() + layout::HISTORY_ITEM_ICON_LEFT,
                                                rect.center().y
                                                    - layout::HISTORY_ITEM_ICON_SIZE / 2.0,
                                            ),
                                            Vec2::splat(layout::HISTORY_ITEM_ICON_SIZE),
                                        ),
                                        Rect::from_min_max(
                                            egui::pos2(0.0, 0.0),
                                            egui::pos2(1.0, 1.0),
                                        ),
                                        Color32::WHITE,
                                    );
                                    if editing {
                                        let response = ui.place(
                                            text_rect.shrink2(egui::vec2(0.0, 2.0)),
                                            egui::TextEdit::singleline(&mut self.history_name)
                                                .id(egui::Id::new(("history-name", &entry_id)))
                                                .font(FontId::proportional(14.0))
                                                .text_color(TEXT_MAIN)
                                                .vertical_align(egui::Align::Center)
                                                .margin(egui::Margin::ZERO)
                                                .frame(egui::Frame::NONE),
                                        );
                                        if self.history_name_focus {
                                            response.request_focus();
                                            self.history_name_focus = false;
                                        }
                                        if (response.has_focus() || response.lost_focus())
                                            && ui.input(|input| input.key_pressed(egui::Key::Enter))
                                        {
                                            renamed =
                                                Some((index, self.history_name.trim().to_owned()));
                                        }
                                    } else {
                                        let response = ui
                                            .interact(
                                                rect,
                                                ui.id().with(("history-select", &entry_id)),
                                                Sense::click_and_drag(),
                                            )
                                            .on_hover_cursor(
                                                if ui.ctx().is_being_dragged(
                                                    ui.id().with(("history-select", &entry_id)),
                                                ) {
                                                    egui::CursorIcon::Grabbing
                                                } else {
                                                    egui::CursorIcon::PointingHand
                                                },
                                            );
                                        response.dnd_set_drag_payload(entry_id.clone());
                                        if response.dragged() {
                                            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                                        }
                                        let over_control = pointer_pos.is_some_and(|pos| {
                                            dot_rect.contains(pos)
                                                || delete_rect.contains(pos)
                                                || (!in_use && edit_rect.contains(pos))
                                        });
                                        if let Some(source) = response.dnd_hover_payload::<String>()
                                            && *source != entry_id
                                            && !over_control
                                        {
                                            ui.painter().line_segment(
                                                [rect.left_top(), rect.right_top()],
                                                Stroke::new(2.0, history_item_active_text()),
                                            );
                                        }
                                        if !over_control
                                            && let Some(source) =
                                                response.dnd_release_payload::<String>()
                                        {
                                            reordered = Some(((*source).clone(), entry_id.clone()));
                                        }
                                        let text_color = if hovered || active {
                                            history_item_active_text()
                                        } else {
                                            TEXT_MUTED
                                        };
                                        let galley = ui.painter().layout_no_wrap(
                                            if entry.name.is_empty() {
                                                entry.gateway_url.clone()
                                            } else {
                                                entry.name.clone()
                                            },
                                            FontId::proportional(14.0),
                                            text_color,
                                        );
                                        ui.painter()
                                            .with_clip_rect(text_rect.intersect(ui.clip_rect()))
                                            .galley(
                                                egui::pos2(
                                                    text_rect.left(),
                                                    text_rect.center().y - galley.size().y / 2.0,
                                                ),
                                                galley,
                                                text_color,
                                            );
                                        if response.clicked() {
                                            selected = Some(index);
                                        }
                                        if !egui::DragAndDrop::has_any_payload(ctx) {
                                            if response.hovered() {
                                                if self.history_tooltip_entry.as_deref()
                                                    != Some(entry_id.as_str())
                                                {
                                                    self.history_hover_started_at =
                                                        ctx.input(|input| input.time);
                                                    self.history_tooltip_rect = None;
                                                    self.history_tooltip_entry =
                                                        Some(entry_id.clone());
                                                }
                                                tooltip_candidate =
                                                    Some((index, response.clone(), rect));
                                            } else if self.history_tooltip_entry.as_deref()
                                                == Some(entry_id.as_str())
                                                && tooltip_is_hovered
                                                && !pointer_pos
                                                    .is_some_and(|pos| rect.contains(pos))
                                            {
                                                tooltip_candidate =
                                                    Some((index, response.clone(), rect));
                                            }
                                        }
                                    }
                                    if hovered || editing {
                                        let edit_rect = if in_use || editing {
                                            delete_rect
                                        } else {
                                            edit_rect
                                        };
                                        let response = ui.interact(
                                            edit_rect,
                                            ui.id().with(("history-edit", &entry_id)),
                                            Sense::click_and_drag(),
                                        );
                                        paint_svg_texture(
                                            ui,
                                            if editing {
                                                &self.assets.history_confirm
                                            } else {
                                                &self.assets.history_edit
                                            },
                                            Rect::from_center_size(
                                                edit_rect.center(),
                                                Vec2::splat(16.0),
                                            ),
                                            Rect::from_min_max(
                                                egui::pos2(0.0, 0.0),
                                                egui::pos2(1.0, 1.0),
                                            ),
                                            if response.hovered() {
                                                Color32::WHITE
                                            } else {
                                                Color32::from_rgb(91, 97, 112)
                                            },
                                        );
                                        show_hover_tooltip(
                                            &response,
                                            if editing {
                                                i18n::tr("确认名称", "Confirm name")
                                            } else {
                                                i18n::tr("重命名", "Rename")
                                            },
                                        );
                                        if response.clicked() {
                                            if editing {
                                                renamed = Some((
                                                    index,
                                                    self.history_name.trim().to_owned(),
                                                ));
                                            } else {
                                                self.editing_history = Some(entry_id.clone());
                                                self.history_name = if entry.name.is_empty() {
                                                    entry.gateway_url.clone()
                                                } else {
                                                    entry.name.clone()
                                                };
                                                self.history_name_focus = true;
                                            }
                                        }
                                    }
                                    let delete_delay = self
                                        .history_delete_guard
                                        .as_ref()
                                        .filter(|(id, _)| id == &entry_id)
                                        .and_then(|(_, confirmed)| {
                                            Duration::from_millis(layout::HISTORY_DELETE_DELAY_MS)
                                                .checked_sub(confirmed.elapsed())
                                        });
                                    if let Some(remaining) = delete_delay {
                                        ctx.request_repaint_after(remaining);
                                    }
                                    if hovered && !editing && !in_use && delete_delay.is_none() {
                                        let response = ui.interact(
                                            delete_rect,
                                            ui.id().with(("history-delete", &entry_id)),
                                            Sense::click_and_drag(),
                                        );
                                        paint_svg_texture(
                                            ui,
                                            &self.assets.history_delete,
                                            Rect::from_center_size(
                                                delete_rect.center(),
                                                Vec2::splat(16.0),
                                            ),
                                            Rect::from_min_max(
                                                egui::pos2(0.0, 0.0),
                                                egui::pos2(1.0, 1.0),
                                            ),
                                            if response.hovered() {
                                                Color32::from_rgb(
                                                    layout::HISTORY_DELETE_HOVER_COLOR[0],
                                                    layout::HISTORY_DELETE_HOVER_COLOR[1],
                                                    layout::HISTORY_DELETE_HOVER_COLOR[2],
                                                )
                                            } else {
                                                Color32::from_rgb(91, 97, 112)
                                            },
                                        );
                                        show_hover_tooltip(&response, i18n::tr("删除", "Delete"));
                                        if response.clicked() {
                                            removed = Some(index);
                                        }
                                    }
                                    // Register controls above the row so they consume their own
                                    // clicks/drags and never start row selection or reordering.
                                    let dot_response = ui.interact(
                                        dot_rect,
                                        ui.id().with(("history-connection", &entry_id)),
                                        Sense::click_and_drag(),
                                    );
                                    let mut dot_tip = dot_response.clone();
                                    dot_tip.rect = Rect::from_center_size(
                                        dot_rect.center(),
                                        egui::vec2(
                                            layout::HISTORY_ITEM_DELETE_WIDTH,
                                            rect.height(),
                                        ),
                                    );
                                    dot_tip.interact_rect = dot_tip.rect.intersect(ui.clip_rect());
                                    show_hover_tooltip(
                                        &dot_tip,
                                        if in_use {
                                            i18n::tr("连接中", "Connected")
                                        } else {
                                            i18n::tr("未连接", "Not connected")
                                        },
                                    );
                                }
                                if !self.providers.is_empty() {
                                    ui.add_space(layout::PROVIDER_TOP_GAP);
                                    paint_provider_section_title(
                                        ui,
                                        i18n::tr("提供商预设", "Provider Preset"),
                                        layout::PROVIDER_PRESET_TITLE_LINE_LENGTH,
                                        layout::PROVIDER_PRESET_TITLE_BOTTOM_GAP,
                                    );
                                }
                                let providers = &self.providers;
                                let width = ui.available_width();
                                let columns = provider_column_count(width, providers.len());
                                ui.spacing_mut().item_spacing.y = layout::PROVIDER_ROW_GAP;
                                for (row, entries) in providers.chunks(columns).enumerate() {
                                    let (row_rect, _) = ui.allocate_exact_size(
                                        Vec2::new(width, layout::PROVIDER_BUTTON_SIZE),
                                        Sense::hover(),
                                    );
                                    for (column, provider) in entries.iter().enumerate() {
                                        let index = row * columns + column;
                                        let rect = Rect::from_min_size(
                                            row_rect.min
                                                + Vec2::new(
                                                    column as f32
                                                        * (layout::PROVIDER_BUTTON_SIZE
                                                            + layout::PROVIDER_BUTTON_GAP),
                                                    0.0,
                                                ),
                                            Vec2::splat(layout::PROVIDER_BUTTON_SIZE),
                                        );
                                        let response = ui
                                            .interact(
                                                rect,
                                                ui.id().with(("provider", &provider.name)),
                                                Sense::click(),
                                            )
                                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                                        let [r, g, b] = layout::PROVIDER_BACKGROUND;
                                        ui.painter().rect_filled(
                                            rect,
                                            layout::PROVIDER_CORNER_RADIUS,
                                            Color32::from_rgba_unmultiplied(
                                                r,
                                                g,
                                                b,
                                                layout::PROVIDER_BACKGROUND_ALPHA,
                                            ),
                                        );
                                        if let Some(texture) = self.assets.providers.get(index) {
                                            paint_svg_texture(
                                                ui,
                                                texture,
                                                Rect::from_center_size(
                                                    rect.center(),
                                                    Vec2::splat(layout::PROVIDER_ICON_SIZE),
                                                ),
                                                Rect::from_min_max(
                                                    egui::pos2(0.0, 0.0),
                                                    egui::pos2(1.0, 1.0),
                                                ),
                                                Color32::WHITE,
                                            );
                                        }
                                        let outline = if response.hovered() {
                                            Stroke::new(
                                                1.0,
                                                Color32::from_rgba_unmultiplied(
                                                    layout::PROVIDER_HOVER_OUTLINE[0],
                                                    layout::PROVIDER_HOVER_OUTLINE[1],
                                                    layout::PROVIDER_HOVER_OUTLINE[2],
                                                    layout::PROVIDER_HOVER_OUTLINE[3],
                                                ),
                                            )
                                        } else {
                                            surface_outline_stroke()
                                        };
                                        ui.painter().rect_stroke(
                                            rect,
                                            layout::PROVIDER_CORNER_RADIUS,
                                            outline,
                                            egui::StrokeKind::Inside,
                                        );
                                        show_hover_tooltip(&response, provider.name.clone());
                                        if response.clicked() {
                                            selected_provider = Some(index);
                                        }
                                    }
                                }
                            });
                    });
                paint_surface_outline(
                    ui.painter(),
                    frame.response.rect,
                    layout::INPUT_CORNER_RADIUS,
                );
            });
        if let Some((index, response, rect)) = tooltip_candidate {
            let remaining = f64::from(layout::HISTORY_USAGE_DELAY_SECONDS)
                - (ctx.input(|input| input.time) - self.history_hover_started_at);
            if remaining <= 0.0 {
                let entry = &self.history[index];
                self.history_tooltip_rect = show_history_key_tooltip(
                    &response,
                    entry,
                    rect,
                    self.usage_summaries
                        .get(&usage::configuration_id(&entry.gateway_url, &entry.api_key)),
                    &self.assets,
                    self.usage_available,
                )
                .map(|popup| popup.union(rect));
            } else {
                ctx.request_repaint_after(Duration::from_secs_f64(remaining));
            }
        } else {
            self.history_tooltip_rect = None;
            self.history_tooltip_entry = None;
        }
        if let Some((index, name)) = renamed {
            let mut history = self.history.clone();
            history[index].name = name;
            match gui_settings::save_history(&history) {
                Ok(()) => {
                    self.history_delete_guard = Some((
                        usage::configuration_id(
                            &history[index].gateway_url,
                            &history[index].api_key,
                        ),
                        Instant::now(),
                    ));
                    self.history = history;
                    self.refresh_gateway_display();
                    self.editing_history = None;
                }
                Err(error) => self.set_error(error),
            }
        } else if let Some(index) = removed {
            let mut history = self.history.clone();
            history.remove(index);
            self.editing_history = None;
            match gui_settings::save_history(&history) {
                Ok(()) => {
                    self.history = history;
                    self.refresh_gateway_display();
                }
                Err(error) => self.set_error(error),
            }
        } else if let Some((source, target)) = reordered {
            if source != target
                && let (Some(from), Some(to)) = (
                    self.history.iter().position(|entry| {
                        usage::configuration_id(&entry.gateway_url, &entry.api_key) == source
                    }),
                    self.history.iter().position(|entry| {
                        usage::configuration_id(&entry.gateway_url, &entry.api_key) == target
                    }),
                )
            {
                let mut history = self.history.clone();
                let entry = history.remove(from);
                history.insert(to.min(history.len()), entry);
                match gui_settings::save_history(&history) {
                    Ok(()) => self.history = history,
                    Err(error) => self.set_error(error),
                }
            }
        } else if let Some(index) = selected_provider {
            let provider = &self.providers[index];
            if normalized_gateway_url(&self.gateway_url) != provider.endpoint {
                *self.api_key = String::new();
            }
            self.gateway_url = provider.endpoint.clone();
            self.gateway_url_display = provider.name.clone();
            self.preset_provider = Some(provider.name.clone());
            self.conversion_enabled = false;
            if let Some(website) = provider.website.as_deref() {
                open_external_link(ctx, website);
            }
            self.show_key = false;
            self.show_models = false;
            self.fetched_models.clear();
            self.fetched_models_configuration = None;
            self.mark_preferences_dirty();
            ctx.request_repaint_after(INPUT_AUTOSAVE_DELAY);
            close = true;
        } else if let Some(index) = selected {
            self.select_history_configuration(index);
            ctx.request_repaint_after(INPUT_AUTOSAVE_DELAY);
            close = true;
        }
        if close {
            self.show_history = false;
            self.editing_history = None;
            self.history_tooltip_entry = None;
            self.history_tooltip_rect = None;
        }
    }

    fn render_models_menu(&mut self, ctx: &egui::Context, anchor: Rect, button: Rect) {
        if !self.show_models {
            return;
        }
        if self.fetched_models_configuration
            != local_gateway::configuration_fingerprint(&self.gateway_url, &self.api_key).ok()
        {
            self.show_models = false;
            return;
        }
        let hidden_models = self
            .history
            .iter()
            .find(|entry| {
                entry.gateway_url == normalized_gateway_url(&self.gateway_url)
                    && entry.api_key == self.api_key.trim()
            })
            .map(|entry| entry.hidden_models.clone())
            .unwrap_or_default();
        let mut toggled = None;
        let mut copy_requested: Option<String> = None;
        let size = egui::vec2(anchor.width(), layout::HISTORY_HEIGHT);
        let position = egui::pos2(
            anchor.left(),
            anchor.bottom() + layout::HISTORY_VERTICAL_GAP,
        );
        let popup_rect = Rect::from_min_size(position, size);
        if ctx.input(|input| {
            input.key_pressed(egui::Key::Escape)
                || !input.focused
                || (input.pointer.any_pressed()
                    && input
                        .pointer
                        .interact_pos()
                        .is_some_and(|pos| !popup_rect.contains(pos) && !button.contains(pos)))
        }) {
            self.show_models = false;
            return;
        }
        egui::Area::new(egui::Id::new("gateway-models-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(position)
            .default_size(size)
            .constrain(false)
            .fade_in(true)
            .show(ctx, |ui| {
                // Keep menu edges clear instead of dimming rows while scrolling.
                ui.spacing_mut().scroll.fade.strength = 0.0;
                let frame = egui::Frame::new()
                    .fill(history_background_color())
                    .shadow(egui::epaint::Shadow::NONE)
                    .corner_radius(layout::INPUT_CORNER_RADIUS)
                    .inner_margin(menu_frame_margin())
                    .show(ui, |ui| {
                        let inner = size
                            - egui::vec2(
                                2.0 * f32::from(layout::HISTORY_PADDING)
                                    - f32::from(layout::SCROLLBAR_GUTTER),
                                2.0 * f32::from(layout::HISTORY_PADDING),
                            );
                        ui.set_width(inner.x);
                        ui.set_height(inner.y);
                        ui.spacing_mut().item_spacing.y = layout::HISTORY_ITEM_VERTICAL_GAP;
                        egui::ScrollArea::vertical()
                            .id_salt("gateway-model-names")
                            .auto_shrink([false, false])
                            .max_height(inner.y)
                            .show(ui, |ui| {
                                for (index, model) in self.fetched_models.iter().enumerate() {
                                    let (rect, response) = ui.allocate_exact_size(
                                        egui::vec2(
                                            ui.available_width(),
                                            layout::HISTORY_ITEM_HEIGHT,
                                        ),
                                        Sense::click(),
                                    );
                                    let response =
                                        response.on_hover_cursor(egui::CursorIcon::PointingHand);
                                    let active = response.hovered();
                                    ui.painter().rect_filled(
                                        rect,
                                        layout::HISTORY_ITEM_CORNER_RADIUS,
                                        if active {
                                            history_item_active_background()
                                        } else {
                                            history_item_background_color()
                                        },
                                    );
                                    let color = if active {
                                        history_item_active_text()
                                    } else {
                                        TEXT_MUTED
                                    };
                                    let icon_rect = Rect::from_center_size(
                                        egui::pos2(
                                            rect.left()
                                                + layout::HISTORY_ITEM_ICON_LEFT
                                                + layout::HISTORY_ITEM_ICON_SIZE / 2.0,
                                            rect.center().y,
                                        ),
                                        Vec2::splat(layout::HISTORY_ITEM_ICON_SIZE),
                                    );
                                    paint_svg_texture(
                                        ui,
                                        &self.assets.models,
                                        icon_rect,
                                        Rect::from_min_max(
                                            egui::pos2(0.0, 0.0),
                                            egui::pos2(1.0, 1.0),
                                        ),
                                        Color32::WHITE,
                                    );
                                    let check_rect = Rect::from_center_size(
                                        egui::pos2(
                                            rect.right()
                                                - layout::HISTORY_ITEM_ICON_LEFT
                                                - layout::MODEL_MENU_ACTION_SIZE / 2.0,
                                            rect.center().y,
                                        ),
                                        Vec2::splat(layout::MODEL_MENU_ACTION_SIZE),
                                    );
                                    let copy_rect = Rect::from_center_size(
                                        egui::pos2(
                                            check_rect.left()
                                                - layout::MODEL_MENU_ACTION_GAP
                                                - layout::MODEL_MENU_ACTION_SIZE / 2.0
                                                + layout::MODEL_MENU_COPY_OFFSET_X,
                                            rect.center().y,
                                        ),
                                        Vec2::splat(layout::MODEL_MENU_ACTION_SIZE),
                                    );
                                    let text_rect = Rect::from_min_max(
                                        egui::pos2(
                                            icon_rect.right() + layout::HISTORY_ITEM_ICON_TEXT_GAP,
                                            rect.top(),
                                        ),
                                        egui::pos2(
                                            copy_rect.left() - layout::HISTORY_ITEM_ICON_TEXT_GAP,
                                            rect.bottom(),
                                        ),
                                    );
                                    let copy_response = ui.interact(
                                        copy_rect,
                                        ui.id().with(("model-copy", index)),
                                        Sense::click(),
                                    );
                                    if response.hovered() || copy_response.hovered() {
                                        paint_svg_texture(
                                            ui,
                                            &self.assets.copy,
                                            Rect::from_center_size(
                                                copy_rect.center(),
                                                Vec2::splat(layout::MODEL_MENU_ACTION_ICON_SIZE),
                                            ),
                                            Rect::from_min_max(
                                                egui::pos2(0.0, 0.0),
                                                egui::pos2(1.0, 1.0),
                                            ),
                                            if copy_response.hovered() {
                                                Color32::WHITE
                                            } else {
                                                Color32::from_rgb(130, 136, 150)
                                            },
                                        );
                                    }
                                    show_hover_tooltip(
                                        &copy_response,
                                        i18n::tr("复制模型名称", "Copy model name"),
                                    );
                                    if copy_response.clicked() {
                                        copy_requested = Some(model.clone());
                                    }
                                    if !hidden_models.contains(model) {
                                        paint_svg_texture(
                                            ui,
                                            &self.assets.select_green,
                                            Rect::from_center_size(
                                                check_rect.center(),
                                                Vec2::splat(layout::MODEL_MENU_CHECK_ICON_SIZE),
                                            ),
                                            Rect::from_min_max(
                                                egui::pos2(0.0, 0.0),
                                                egui::pos2(1.0, 1.0),
                                            ),
                                            Color32::WHITE,
                                        );
                                    }
                                    let galley = ui.painter().layout_no_wrap(
                                        model.clone(),
                                        FontId::proportional(13.0),
                                        color,
                                    );
                                    ui.painter()
                                        .with_clip_rect(text_rect.intersect(ui.clip_rect()))
                                        .galley(
                                            egui::pos2(
                                                text_rect.left(),
                                                text_rect.center().y - galley.size().y / 2.0,
                                            ),
                                            galley,
                                            color,
                                        );
                                    let action = if hidden_models.contains(model) {
                                        i18n::tr(
                                            "点击 • 开启此模型映射",
                                            "Click • Enable This Model Mapping",
                                        )
                                    } else {
                                        i18n::tr(
                                            "点击 • 关闭此模型映射",
                                            "Click • Disable this model mapping",
                                        )
                                    };
                                    if !copy_response.hovered() {
                                        show_hover_tooltip(&response, action);
                                    }
                                    if response.clicked() && !copy_response.clicked() {
                                        toggled = Some(index);
                                    }
                                }
                            });
                    });
                paint_surface_outline(
                    ui.painter(),
                    frame.response.rect,
                    layout::INPUT_CORNER_RADIUS,
                );
            });
        if let Some(model) = copy_requested {
            ctx.output_mut(|output| {
                output
                    .commands
                    .push(egui::OutputCommand::CopyText(model.clone()));
            });
            self.status = format!(
                "{} • {}",
                i18n::tr("已复制模型名称", "Copied model name"),
                model
            );
            self.status_is_warning = false;
        }
        if let Some(index) = toggled {
            let mut history = self.history.clone();
            if let Some(entry) = history.iter_mut().find(|entry| {
                entry.gateway_url == normalized_gateway_url(&self.gateway_url)
                    && entry.api_key == self.api_key.trim()
            }) {
                let model = &self.fetched_models[index];
                if entry.hidden_models.contains(model) {
                    entry.hidden_models.retain(|id| id != model);
                } else {
                    entry.hidden_models.push(model.clone());
                }
                match gui_settings::save_history(&history) {
                    Ok(()) => {
                        self.history = history;
                        self.status = model_list_change_status().to_owned();
                        self.status_is_warning = false;
                    }
                    Err(error) => self.set_error(error),
                }
            }
        }
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
        if self.gateway_action_running(fetch_models) {
            self.set_error(anyhow::anyhow!(i18n::tr(
                "网关检查已经在运行",
                "A gateway check is already running"
            )));
            return;
        }
        if fetch_models {
            self.show_history = false;
            self.show_models = false;
            self.fetched_models.clear();
            self.fetched_models_configuration = None;
        }
        if let Err(error) = self.remember_configuration() {
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
                i18n::tr("正在连接 • 拉取模型 …", "Connecting • fetching models …"),
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
                self.status_is_warning = false;
            }
            Err(error) => self.set_error(error),
        }
    }

    fn start_selected_client(&mut self) {
        if !self.client_selected {
            self.status = default_status().to_owned();
            self.status_is_warning = false;
            return;
        }
        let target = self.selected;
        if self.configuration_is_current(target) {
            return;
        }
        if !self.client_is_installed(target) {
            self.status = client_launch_requires_install_status(target);
            self.status_is_warning = true;
            return;
        }
        if let Err(error) = self.remember_configuration() {
            self.set_error(error);
            return;
        }
        let kind = TaskKind::Client(target);
        if self.client_connect_pending(target)
            || self.is_running(TaskKind::GatewayUpdate(target))
            || self.is_running(TaskKind::AccountMode(target))
        {
            self.status = format!(
                "{} • {}",
                target.title(),
                i18n::tr("自动重启 • 失败!", "Auto-restart • failed!")
            );
            self.status_is_warning = true;
            return;
        }
        let arguments =
            client_worker_arguments(target, &self.gateway_url, self.model_list_mode(target));
        match self.spawn_worker(kind, target.title(), &arguments, true) {
            Ok(task) => {
                self.latest_client_jobs.insert(target, task.sequence);
                self.running.push(task);
                self.status = format!(
                    "{} • {}",
                    target.title(),
                    if self.client_is_running(target) {
                        i18n::tr(
                            "正在重启 & 刷新提供商 & 模型列表",
                            "Restarting & Refreshing provider & models",
                        )
                    } else {
                        i18n::tr("正在启动 • 连接", "Starting • Connecting")
                    }
                );
                self.status_is_warning = false;
            }
            Err(error) => self.set_error(error),
        }
    }

    fn start_selected_client_install(&mut self) {
        if !self.client_selected {
            self.status = default_status().to_owned();
            self.status_is_warning = false;
            return;
        }
        let target = self.selected;
        if self.client_is_installed(target) {
            if !self.gateway_managed_clients.contains(&target)
                || self.client_connect_pending(target)
                || self.is_running(TaskKind::GatewayUpdate(target))
                || self.is_running(TaskKind::AccountMode(target))
            {
                return;
            }
            match self.spawn_worker(TaskKind::AccountMode(target), target.title(), &[], false) {
                Ok(task) => {
                    // Suppress the old monitor's completion while the account job
                    // closes it, just as provider replacement already does.
                    self.latest_client_jobs.insert(target, task.sequence);
                    self.running.push(task);
                    self.status = format!(
                        "{} • {}",
                        target.title(),
                        i18n::tr("正在切换账号模式", "Switching account mode")
                    );
                    self.status_is_warning = false;
                }
                Err(error) => self.set_error(error),
            }
            return;
        }

        let kind = TaskKind::Install(target);
        if self.is_running(kind) {
            self.status = format!(
                "{} {} ...",
                i18n::tr("正在安装", "Installing"),
                target.title()
            );
            self.status_is_warning = false;
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
                self.status_is_warning = false;
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
        if let TaskKind::AccountMode(target) = kind {
            let job = crate::background::start_account_mode_job(target, log_path.clone())?;
            return Ok(RunningTask {
                configuration_fingerprint: None,
                sequence: self.allocate_log_sequence(),
                kind,
                child: None,
                job: Some(job),
                worker_pid: std::process::id(),
                log_path,
                label: label.to_owned(),
                log_title,
                log_text: String::new(),
                last_log_read: UNIX_EPOCH,
            });
        }
        if let TaskKind::Client(target) | TaskKind::GatewayUpdate(target) = kind {
            let job = crate::background::start_client_job(
                target,
                self.model_list_mode(target),
                matches!(kind, TaskKind::GatewayUpdate(_)),
                self.gateway_url.clone(),
                self.api_key.to_string(),
                self.conversion_enabled,
                log_path.clone(),
            )?;
            return Ok(RunningTask {
                configuration_fingerprint: local_gateway::configuration_fingerprint(
                    &self.gateway_url,
                    &self.api_key,
                )
                .ok(),
                sequence: self.allocate_log_sequence(),
                kind,
                child: None,
                job: Some(job),
                worker_pid: std::process::id(),
                log_path,
                label: label.to_owned(),
                log_title,
                log_text: String::new(),
                last_log_read: UNIX_EPOCH,
            });
        }
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
            configuration_fingerprint: local_gateway::configuration_fingerprint(
                &self.gateway_url,
                &self.api_key,
            )
            .ok(),
            sequence,
            kind,
            child: Some(child),
            job: None,
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

    fn client_connect_pending(&self, target: ClientTarget) -> bool {
        let latest = self
            .running
            .iter()
            .filter(|task| task.kind == TaskKind::Client(target))
            .max_by_key(|task| task.sequence);
        latest.is_some_and(|task| {
            task.job.is_some()
                && (!self.client_is_attached(target)
                    || !self
                        .attached_configurations
                        .iter()
                        .any(|(client, fingerprint)| {
                            *client == target
                                && task.configuration_fingerprint.as_deref()
                                    == Some(fingerprint.as_str())
                        }))
        })
    }

    fn gateway_action_running(&self, fetch_models: bool) -> bool {
        self.running.iter().any(|task| {
            task.kind == TaskKind::GatewayTest && is_fetch_models_label(&task.label) == fetch_models
        })
    }

    fn client_is_running(&self, target: ClientTarget) -> bool {
        self.client_processes
            .iter()
            .any(|(client, _)| *client == target)
    }

    fn client_is_attached(&self, target: ClientTarget) -> bool {
        self.attached_clients.contains(&target)
    }

    fn client_is_installed(&self, target: ClientTarget) -> bool {
        self.client_is_running(target) || self.installed_clients.is_installed(target)
    }

    fn configuration_is_current(&self, target: ClientTarget) -> bool {
        self.client_is_running(target)
            && self.client_is_attached(target)
            && configuration_matches(
                &self.attached_configurations,
                target,
                &self.gateway_url,
                &self.api_key,
            )
    }

    fn poll_tasks(&mut self, ctx: &egui::Context) {
        if let Some(job) = &self.provider_job
            && let Some(result) = job.poll()
        {
            self.provider_job = None;
            if let Ok(providers) = result {
                self.assets.providers.clear();
                for provider in &providers {
                    let bytes = if render_svg_rgba(&provider.icon_bytes, 1, 1).is_ok() {
                        provider.icon_bytes.as_slice()
                    } else {
                        include_bytes!("../assets/gui/svg/Default-Provider.svg")
                    };
                    self.assets.providers.push(load_embedded_svg_texture(
                        ctx,
                        &format!("provider-{}", provider.id),
                        bytes,
                        Vec2::splat(layout::PROVIDER_ICON_SIZE),
                    ));
                }
                self.providers = providers;
            } else {
                self.providers.clear();
                self.assets.providers.clear();
            }
        }
        if let Some(result) = self.usage_reader.poll() {
            self.usage_available = result.is_ok();
            if let Ok(summaries) = result {
                self.usage_summaries = summaries;
            }
        }
        if let Some(result) = self.model_log_reader.poll()
            && let Ok(entries) = result
        {
            self.model_log_text = format_model_log_entries(&entries, &self.history);
        }
        if self.show_history
            && self.last_usage_scan.elapsed().unwrap_or_default() >= Duration::from_secs(1)
        {
            self.usage_reader.refresh();
            self.last_usage_scan = SystemTime::now();
        }
        if self.show_logs
            && matches!(self.log_view, LogView::Model)
            && self.last_model_log_scan.elapsed().unwrap_or_default() >= Duration::from_secs(1)
        {
            self.model_log_reader.refresh(300);
            self.last_model_log_scan = SystemTime::now();
        }
        self.scan_runtime_states_if_due();
        self.scan_installations_if_due();
        if self.client_selected
            && !self.status_is_warning
            && !self.is_running(TaskKind::GatewayTest)
            && !self.is_running(TaskKind::GatewayUpdate(self.selected))
            && !self.client_connect_pending(self.selected)
            && !self.is_running(TaskKind::Install(self.selected))
            && !self.is_running(TaskKind::AccountMode(self.selected))
            && !is_gateway_result_status(&self.status)
            && !is_model_list_change_status(&self.status)
            && !is_model_name_copied_status(&self.status)
            && !is_protocol_conversion_status(&self.status)
            && !is_account_mode_status(&self.status)
        {
            self.status = selected_client_status(
                self.selected,
                self.client_is_installed(self.selected),
                self.client_is_running(self.selected),
                self.client_is_attached(self.selected),
            );
        }

        let mut finished = Vec::new();
        let mut install_progress = None;
        for (index, task) in self.running.iter_mut().enumerate() {
            refresh_task_log(task);
            if let Some(job) = &task.job {
                if let Some(result) = job.poll() {
                    // Job errors are client/configuration errors already written to the
                    // task log, not failures to query a child process's exit status.
                    finished.push((index, result.is_ok(), None));
                }
                continue;
            }
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
            self.status_is_warning = false;
        }

        for (index, success, wait_error) in finished.into_iter().rev() {
            let mut task = self.running.remove(index);
            refresh_task_log(&mut task);
            if task.kind == TaskKind::GatewayTest && is_fetch_models_label(&task.label) {
                let result_path = task.log_path.with_extension("models.json");
                let result = fs::read(&result_path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok());
                self.fetched_models = if success {
                    result.unwrap_or_default()
                } else {
                    Vec::new()
                };
                self.fetched_models_configuration = task.configuration_fingerprint.clone();
                self.show_models = !self.fetched_models.is_empty()
                    && self.fetched_models_configuration
                        == local_gateway::configuration_fingerprint(
                            &self.gateway_url,
                            &self.api_key,
                        )
                        .ok();
                if self.show_models {
                    self.show_history = false;
                }
                let _ = fs::remove_file(result_path);
            }
            let completed_log = format_log_section(&task.log_title, &task.log_text);
            self.append_completed_log(task.sequence, completed_log);

            if let TaskKind::AccountMode(target) = task.kind {
                self.last_runtime_scan = UNIX_EPOCH;
                let _ = self.sync_runtime_states();
                self.status = if success {
                    format!(
                        "{} • {}",
                        target.title(),
                        i18n::tr("已切换账号模式", "Switched to account mode")
                    )
                } else {
                    failed_task_status(&task)
                };
                self.status_is_warning = !success;
                continue;
            }

            if let TaskKind::Install(target) = task.kind {
                if let Some(error) = wait_error {
                    self.status = format!(
                        "{} {}: {error}",
                        i18n::tr("无法读取", "Failed to read"),
                        i18n::tr("客户端安装状态", "client installation status")
                    );
                    self.status_is_warning = true;
                } else if success {
                    self.installed_clients = ClientInstallations::detect();
                    if self.client_is_installed(target) {
                        if self.client_selected && target == self.selected {
                            self.status = selected_client_status(
                                target,
                                true,
                                self.client_is_running(target),
                                self.client_is_attached(target),
                            );
                        } else {
                            self.status =
                                format!("{} • {}", target.title(), i18n::tr("已安装", "Installed"));
                        }
                        self.status_is_warning = false;
                    } else {
                        self.status = format!(
                            "{} • {}",
                            target.title(),
                            i18n::tr(
                                "安装流程已结束、但复检仍未发现客户端、请查看运行日志",
                                "Installation finished, but the client was not detected; check the runtime log"
                            )
                        );
                        self.status_is_warning = true;
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
                    self.status_is_warning = true;
                }
                continue;
            }

            if let TaskKind::Client(target) = task.kind {
                // The old monitor ending during a restart must not overwrite
                // the replacement monitor's progress or failure status.
                if self
                    .latest_client_jobs
                    .get(&target)
                    .is_some_and(|sequence| *sequence > task.sequence)
                {
                    continue;
                }
                if let Some(error) = wait_error {
                    self.status = format!(
                        "{} {}: {error}",
                        i18n::tr("无法读取后台状态", "Failed to read background status for"),
                        task.label
                    );
                    self.status_is_warning = true;
                } else if success {
                    self.last_runtime_scan = UNIX_EPOCH;
                    self.status = selected_client_status(
                        target,
                        self.client_is_installed(target),
                        self.client_is_running(target),
                        self.client_is_attached(target),
                    );
                    self.status_is_warning = false;
                } else {
                    self.status = failed_task_status(&task);
                    self.status_is_warning = true;
                }
                continue;
            }

            if let TaskKind::GatewayUpdate(target) = task.kind {
                if let Some(error) = wait_error {
                    self.status = format!(
                        "{} • {}: {error}",
                        target.title(),
                        i18n::tr("协议转换更新失败", "Protocol conversion update failed")
                    );
                    self.status_is_warning = true;
                } else if success {
                    self.last_runtime_scan = UNIX_EPOCH;
                    if !is_protocol_conversion_status(&self.status) {
                        self.status = selected_client_status(
                            target,
                            self.client_is_installed(target),
                            self.client_is_running(target),
                            self.client_is_attached(target),
                        );
                    }
                    self.status_is_warning = false;
                } else {
                    self.status = failed_task_status(&task);
                    self.status_is_warning = true;
                }
                continue;
            }

            if let Some(error) = wait_error {
                self.status = format!(
                    "{} {}: {error}",
                    i18n::tr("无法读取后台状态", "Failed to read background status for"),
                    task.label
                );
                self.status_is_warning = true;
            } else if success {
                self.status = completed_task_status(&task);
                self.gateway_result_configuration = task
                    .configuration_fingerprint
                    .as_ref()
                    .filter(|_| task.kind == TaskKind::GatewayTest)
                    .map(|fingerprint| (self.status.clone(), fingerprint.clone()));
                self.status_is_warning = false;
            } else {
                self.status = failed_task_status(&task);
                self.status_is_warning = true;
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
        self.status_is_warning = true;
    }
}

impl LauncherApp {
    fn request_full_exit(&mut self, ctx: &egui::Context) {
        // Exiting is explicit and immediate. AI client processes remain untouched;
        // their requests fail when the shared local gateway is stopped.
        self.finish_exit(ctx);
    }

    fn finish_exit(&mut self, ctx: &egui::Context) {
        self.allow_exit = true;
        // Only our temporary test/install workers, never an AI client process.
        for task in &mut self.running {
            if let Some(child) = task.child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for LauncherApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // 主窗口改为不透明原生窗口，彻底避开 Windows 透明合成层留下的顶部黑线。
        Color32::from_rgba_unmultiplied(
            layout::WINDOW_BACKGROUND[0],
            layout::WINDOW_BACKGROUND[1],
            layout::WINDOW_BACKGROUND[2],
            layout::WINDOW_BACKGROUND[3],
        )
        .to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        #[cfg(windows)]
        if self.tray.take_exit_request() {
            self.request_full_exit(ui.ctx());
        }
        if ui.input(|input| input.viewport().close_requested()) && !self.allow_exit {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            if let Err(error) = self.save_preferences() {
                self.set_error(error);
            }
        }
        // SVG 纹理按当前显示器的真实 DPI 生成；拖到不同缩放比例的显示器后立即重建。
        self.assets.ensure_pixels_per_point(ui.ctx());

        #[cfg(windows)]
        if let Some(native_window) = self.native_window.as_mut() {
            native_window.maintain(ui.ctx());
        }

        self.poll_tasks(ui.ctx());
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

        // 编辑只保存输入；点击“启动 • 连接”或“重启 • 连接”才更新客户端使用的网关。
        // 执行期间只禁用对应操作按钮；输入和其他操作保持可用。
        let testing = self.gateway_action_running(false);
        let fetching = self.gateway_action_running(true);
        let input_connected = configuration_has_running_client(
            &self.attached_configurations,
            &self.attached_clients,
            &self.client_processes,
            &self.gateway_url,
            &self.api_key,
        );
        let leading_tooltip = gateway_input_tooltip(&self.gateway_url_display, &self.gateway_url);
        let conversion_action_enabled = !testing;
        let conversion_tip = {
            let client_protocol = match self.selected {
                ClientTarget::CodexCli | ClientTarget::CodexDesktop => "openai-responses",
                ClientTarget::ClaudeCode | ClientTarget::ClaudeDesktop => "anthropic-messages",
            };
            let recommendation = self
                .providers
                .iter()
                .find(|provider| {
                    self.preset_provider.as_deref() == Some(provider.name.as_str())
                        || normalized_gateway_url(&self.gateway_url) == provider.endpoint
                })
                .and_then(|provider| {
                    provider.protocols.as_ref().map(|protocols| {
                        !protocols
                            .iter()
                            .any(|protocol| protocol.eq_ignore_ascii_case(client_protocol))
                    })
                });
            let status = if self.conversion_enabled {
                i18n::tr("协议转换 • 开启", "Protocol conversion • Enabled")
            } else {
                i18n::tr("协议转换 • 关闭", "Protocol conversion • Disabled")
            };
            match recommendation {
                Some(true) => format!(
                    "{status}\n{}",
                    i18n::tr("建议 • 开启", "Recommended • Enable")
                ),
                Some(false) => format!(
                    "{status}\n{}",
                    i18n::tr("建议 • 关闭", "Recommended • Disable")
                ),
                None => status.to_owned(),
            }
        };
        let (conversion_clicked, gateway_url_changed) = render_input_row_at(
            ui,
            first_input_rect,
            &self.assets.url,
            &mut self.gateway_url_display,
            if crate::gateway::DEFAULT_GATEWAY_URL.is_empty() {
                "https://api-endpoint"
            } else {
                crate::gateway::DEFAULT_GATEWAY_URL
            },
            false,
            None,
            Some((&mut self.show_history, &self.assets.history_expand)),
            input_connected,
            leading_tooltip,
            &self.assets.convert,
            self.conversion_enabled
                .then_some(self.assets.converting.as_slice()),
            Vec2::new(
                layout::CONNECT_ACTION_ICON_WIDTH,
                layout::CONNECT_ACTION_ICON_HEIGHT,
            ),
            &conversion_tip,
            conversion_action_enabled,
            testing,
            &self.assets.spinner,
        );
        let (models_clicked, api_key_changed) = render_input_row_at(
            ui,
            second_input_rect,
            &self.assets.key,
            &mut self.api_key,
            "sk-xxxxxxxxxxxxxxxxx...",
            !self.show_key,
            Some((
                &mut self.show_key,
                &self.assets.eye_show,
                &self.assets.eye_hide,
            )),
            None,
            false,
            i18n::tr("API Key", "API Key").to_owned(),
            &self.assets.models,
            None,
            Vec2::new(
                layout::MODEL_FETCH_ACTION_ICON_WIDTH,
                layout::MODEL_FETCH_ACTION_ICON_HEIGHT,
            ),
            i18n::tr("连接 • 拉取模型", "Connect • Fetch models"),
            !fetching,
            fetching,
            &self.assets.spinner,
        );
        if gateway_url_changed {
            // Keep the real endpoint separate from the friendly name shown by
            // a selected history entry. Typing in the field always switches
            // back to using the typed value as the endpoint.
            self.preset_provider = None;
            self.gateway_url.clone_from(&self.gateway_url_display);
        } else if api_key_changed {
            // A changed key is a different configuration even when the URL is
            // the same, so do not leave the previous friendly name in the URL
            // field and make the pair look like the old history entry.
            self.refresh_gateway_display();
        }
        if gateway_url_changed || api_key_changed {
            self.restore_conversion_setting();
            self.mark_preferences_dirty();
            ui.ctx().request_repaint_after(INPUT_AUTOSAVE_DELAY);
        }
        self.maybe_autosave_preferences();
        // Same left-to-right order as the client cards. Configuration identity
        // includes both the real URL and Key, never the editable display name.
        let active_branches = ClientTarget::ALL.map(|target| self.configuration_is_current(target));
        paint_input_connection_flow(
            ui,
            first_input_rect,
            second_input_rect,
            active_branches.iter().any(|active| *active),
        );
        if conversion_clicked {
            self.toggle_protocol_conversion();
        } else if models_clicked {
            self.start_gateway_test(true);
        }

        // 区块 1 与区块 2 之间：GUI 绘制与 cascade-flow.svg 相同的级联曲线。
        let cascade_flow_rect = Rect::from_center_size(
            egui::pos2(
                content_inner.center().x,
                second_input_rect.bottom() + layout::CASCADE_FLOW_HEIGHT / 2.0,
            ),
            Vec2::new(layout::CASCADE_FLOW_WIDTH, layout::CASCADE_FLOW_HEIGHT),
        );
        // 保留原 SVG 文件及校验；旧版四支线无条件动画入口停用：
        // paint_cascade_flow(ui, cascade_flow_rect);

        // 区块 2-1：客户端卡片面板。
        let client_panel_rect = Rect::from_min_size(
            egui::pos2(
                content_inner.left(),
                cascade_flow_rect.bottom() + layout::BLOCKS_VERTICAL_GAP,
            ),
            Vec2::new(content_inner.width(), layout::CLIENT_CARDS_BLOCK_HEIGHT),
        );
        let connected_flow_rect = Rect::from_min_max(
            cascade_flow_rect.min,
            egui::pos2(cascade_flow_rect.right(), client_panel_rect.top()),
        );
        paint_cascade_flow(ui, connected_flow_rect, client_panel_rect, active_branches);
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

        let history_anchor = Rect::from_min_max(
            first_input_rect.min,
            egui::pos2(
                first_input_rect.right()
                    - layout::INPUT_ACTION_SIZE
                    - layout::INPUT_ACTION_HORIZONTAL_GAP,
                first_input_rect.bottom(),
            ),
        );
        if self.show_history {
            self.show_models = false;
        }
        self.render_history(ui.ctx(), history_anchor);
        let models_anchor = Rect::from_min_size(
            egui::pos2(
                second_input_rect.right() - layout::INPUT_ACTION_SIZE,
                second_input_rect.top(),
            ),
            Vec2::splat(layout::INPUT_ACTION_SIZE),
        );
        self.render_models_menu(ui.ctx(), history_anchor, models_anchor);

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
            self.show_logs = render_log_window(
                ui.ctx(),
                window_rect,
                self.log_view,
                &self.log_text,
                &self.model_log_text,
            );
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

fn render_log_window(
    ctx: &egui::Context,
    window_rect: Rect,
    view: LogView,
    runtime_text: &str,
    model_text: &str,
) -> bool {
    let mut open = true;
    // Never let the floating log window exceed the launcher window;
    // this matters especially when Windows DPI scaling makes the
    // logical fixed height larger than the current viewport.
    let log_height = layout::LOG_WINDOW_HEIGHT
        .min((window_rect.height() - layout::LOG_WINDOW_MARGIN * 2.0).max(1.0));
    let log_text = if matches!(view, LogView::Runtime) {
        if runtime_text.trim().is_empty() {
            i18n::tr("暂无运行日志。", "No runtime logs yet.").to_owned()
        } else {
            compact_runtime_log(runtime_text)
        }
    } else {
        if model_text.trim().is_empty() {
            i18n::tr("暂无请求日志。", "No request logs yet.").to_owned()
        } else {
            model_text.to_owned()
        }
    };
    let title = if matches!(view, LogView::Runtime) {
        i18n::tr("运行日志", "Runtime log")
    } else {
        i18n::tr("请求日志", "Request log")
    };
    let log_window = egui::Window::new(title)
        .title_bar(false)
        .collapsible(false)
        .movable(false)
        .fixed_pos(egui::pos2(
            window_rect.center().x,
            window_rect.bottom() - layout::LOG_WINDOW_MARGIN,
        ))
        .resizable(false)
        .pivot(Align2::CENTER_BOTTOM)
        .fixed_size(Vec2::new(
            (window_rect.width() - layout::LOG_WINDOW_MARGIN * 2.0).max(1.0),
            log_height,
        ))
        .frame(
            egui::Frame::new()
                .inner_margin(egui::Margin::same(layout::HISTORY_PADDING))
                .shadow(egui::epaint::Shadow::NONE)
                .fill(Color32::TRANSPARENT)
                .stroke(Stroke::NONE)
                .corner_radius(layout::LOG_WINDOW_CORNER_RADIUS),
        );
    let log_response = log_window.show(ctx, |ui| {
        ui.visuals_mut().override_text_color = Some(TEXT_MAIN);
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        let padding = f32::from(layout::HISTORY_PADDING);
        ui.set_height((log_height - 2.0 * padding).max(1.0));
        let outer_rect = ui.max_rect().expand(padding);
        let header_bottom = outer_rect.top()
            + padding
            + layout::LOG_HEADER_HEIGHT
            + layout::LOG_HEADER_PADDING_BOTTOM;
        let painter = ui
            .painter()
            .with_clip_rect(outer_rect.intersect(window_rect));
        let radius = layout::LOG_WINDOW_CORNER_RADIUS;
        painter.rect_filled(
            Rect::from_min_max(
                outer_rect.min,
                egui::pos2(outer_rect.right(), header_bottom),
            ),
            egui::CornerRadius {
                nw: radius,
                ne: radius,
                sw: 0,
                se: 0,
            },
            Color32::from_rgb(
                layout::LOG_HEADER_BG[0],
                layout::LOG_HEADER_BG[1],
                layout::LOG_HEADER_BG[2],
            ),
        );
        painter.rect_filled(
            Rect::from_min_max(egui::pos2(outer_rect.left(), header_bottom), outer_rect.max),
            egui::CornerRadius {
                nw: 0,
                ne: 0,
                sw: radius,
                se: radius,
            },
            Color32::from_rgb(
                layout::LOG_CONTENT_BG[0],
                layout::LOG_CONTENT_BG[1],
                layout::LOG_CONTENT_BG[2],
            ),
        );
        let (header_rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), layout::LOG_HEADER_HEIGHT),
            Sense::hover(),
        );
        if render_log_header(ui, header_rect, view) {
            open = false;
        }
        let body_top = header_bottom + padding;
        // Keep the log body as a fixed viewport. A TextEdit whose
        // desired row count equals the complete log would otherwise
        // make the egui window grow with every new line and leave no
        // room for the enclosing scrollbar.
        let body_height = (outer_rect.bottom() - padding - body_top).max(1.0);
        // TextEdit owns the scroll position and selection state. Unlike a
        // selectable Label, it asks the enclosing ScrollArea to follow
        // the selection cursor while the user drags beyond the viewport.
        let mut selectable_log = log_text;
        let viewport = Vec2::new(
            ui.available_width() + f32::from(layout::SCROLLBAR_GUTTER),
            body_height,
        );
        let scroll_rect = Rect::from_min_size(egui::pos2(header_rect.left(), body_top), viewport);
        let mut scroll_ui = ui.new_child(egui::UiBuilder::new().max_rect(scroll_rect));
        egui::ScrollArea::both()
            .max_width(viewport.x)
            .max_height(viewport.y)
            .min_scrolled_height(viewport.y)
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded)
            .auto_shrink([false, false])
            .stick_to_bottom(matches!(view, LogView::Runtime))
            .show(&mut scroll_ui, |ui| {
                let row_count = selectable_log.lines().count().max(1);
                let log_font = FontId::monospace(11.0);
                let text_color = Color32::from_rgb(
                    layout::LOG_CONTENT_TEXT[0],
                    layout::LOG_CONTENT_TEXT[1],
                    layout::LOG_CONTENT_TEXT[2],
                );
                let mut log_layouter = |ui: &egui::Ui,
                                        text: &dyn egui::TextBuffer,
                                        wrap_width: f32| {
                    let mut job = egui::text::LayoutJob::default();
                    job.wrap.max_width = if matches!(view, LogView::Model) {
                        f32::INFINITY
                    } else {
                        wrap_width
                    };
                    let mut append_piece = |piece: &str, color: Color32| {
                        let mut format = egui::text::TextFormat::simple(log_font.clone(), color);
                        format.line_height = Some(layout::LOG_CONTENT_LINE_HEIGHT);
                        job.append(piece, 0.0, format);
                    };
                    for (row_index, raw_line) in
                        text.as_str().split_inclusive(char::from(10)).enumerate()
                    {
                        let has_newline = raw_line.ends_with(char::from(10));
                        let line = raw_line.strip_suffix(char::from(10)).unwrap_or(raw_line);
                        let row_color = if matches!(view, LogView::Model)
                            && row_index > 0
                            && row_index % 2 == 0
                        {
                            Color32::from_rgb(
                                layout::MODEL_LOG_EVEN_ROW_COLOR[0],
                                layout::MODEL_LOG_EVEN_ROW_COLOR[1],
                                layout::MODEL_LOG_EVEN_ROW_COLOR[2],
                            )
                        } else {
                            text_color
                        };
                        if matches!(view, LogView::Model) {
                            let parts = line.split(" • ").collect::<Vec<_>>();
                            for (part_index, part) in parts.iter().enumerate() {
                                let color = if row_index > 0 && part_index == 4 {
                                    let cancelled =
                                        part.contains("取消") || part.contains("cancelled");
                                    let http_success = part
                                        .split_whitespace()
                                        .next()
                                        .and_then(|code| code.parse::<u16>().ok())
                                        .is_some_and(|code| (200..300).contains(&code));
                                    if part.contains("完成")
                                        || part.contains("success")
                                        || (cancelled && http_success)
                                    {
                                        GREEN
                                    } else if part.contains("失败")
                                        || part.contains("failed")
                                        || part.contains("取消")
                                        || part.contains("cancelled")
                                    {
                                        Color32::from_rgb(255, 179, 107)
                                    } else {
                                        row_color
                                    }
                                } else {
                                    row_color
                                };
                                append_piece(part, color);
                                if part_index + 1 < parts.len() {
                                    append_piece(" • ", row_color);
                                }
                            }
                        } else {
                            append_piece(line, row_color);
                        }
                        if has_newline {
                            append_piece(&char::from(10).to_string(), row_color);
                        }
                    }
                    ui.fonts_mut(|fonts| fonts.layout_job(job))
                };
                let desired_log_width = if matches!(view, LogView::Runtime) {
                    // Runtime sections are meant to be readable as prose and
                    // therefore wrap long lines back into the viewport.
                    viewport.x
                } else {
                    // Model records stay one physical line. Size the editor to
                    // the longest actual record so a horizontal scrollbar is
                    // shown only when one is genuinely wider than the viewport.
                    let longest_line_width = ui.fonts_mut(|fonts| {
                        selectable_log
                            .lines()
                            .map(|line| {
                                fonts
                                    .layout_no_wrap(line.to_owned(), log_font.clone(), text_color)
                                    .size()
                                    .x
                            })
                            .fold(0.0, f32::max)
                    });
                    longest_line_width.max(viewport.x)
                };
                let mut text_edit = egui::TextEdit::multiline(&mut selectable_log)
                    .font(log_font.clone())
                    .text_color(text_color)
                    // The outer ScrollArea owns the fixed viewport; the full
                    // text height supplies its scrollable content.
                    .desired_rows(row_count)
                    .desired_width(desired_log_width)
                    .frame(egui::Frame::NONE);
                if matches!(view, LogView::Model) {
                    text_edit = text_edit.layouter(&mut log_layouter);
                }
                let response = ui.add(text_edit);

                // egui's text selection requests a scroll on selection
                // changes, but it does not continuously follow the
                // pointer when dragging past the viewport edge. Push
                // the enclosing ScrollArea while the primary button is
                // held near an edge.
                if ctx.is_being_dragged(response.id)
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
                        ctx.request_repaint();
                    }
                }
            });
    });
    if let Some(shown) = log_response {
        let painter = ctx.layer_painter(shown.response.layer_id);
        painter.rect_stroke(
            shown.response.rect,
            layout::LOG_WINDOW_CORNER_RADIUS,
            log_outline_stroke(),
            egui::StrokeKind::Inside,
        );
    }
    open
}

fn compact_runtime_log(text: &str) -> String {
    // Keep the section headers, body lines, blank lines and separators intact.
    // Long individual lines are handled by the log window's horizontal scroll area.
    text.to_owned()
}

fn model_log_time_hms(started_ms: i64) -> (u16, u16, u16) {
    #[cfg(windows)]
    {
        const WINDOWS_EPOCH_UNIX_MS: i128 = 11_644_473_600_000;
        let windows_ticks = (i128::from(started_ms) + WINDOWS_EPOCH_UNIX_MS) * 10_000;
        if windows_ticks >= 0 && windows_ticks <= i128::from(u64::MAX) {
            let file_time = FILETIME {
                dwLowDateTime: windows_ticks as u64 as u32,
                dwHighDateTime: (windows_ticks as u64 >> 32) as u32,
            };
            let mut utc = SYSTEMTIME::default();
            let mut local = SYSTEMTIME::default();
            if unsafe { FileTimeToSystemTime(&file_time, &mut utc) }.is_ok()
                && unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local) }.is_ok()
            {
                return (local.wHour, local.wMinute, local.wSecond);
            }
        }
    }
    let seconds = started_ms.div_euclid(1000).rem_euclid(86_400);
    (
        (seconds / 3_600) as u16,
        ((seconds % 3_600) / 60) as u16,
        (seconds % 60) as u16,
    )
}

fn format_model_log_entries(
    entries: &[usage::ModelLogEntry],
    history: &[gui_settings::GatewayHistoryEntry],
) -> String {
    let header = i18n::tr(
        "时间 • Agent • 供应商 • 模型 • 状态码 • Tokens(输入/缓存/输出) • 总耗时 • 首字延迟 • 请求类型 • (请求端点/协议转换) • 流式响应",
        "Time • Agent • Provider • Model • Status • Tokens(Input/Cache/Output) • Total time • First-byte latency • Request type • (Endpoint/Protocol conversion) • Streaming",
    );
    let rows = entries
        .iter()
        .map(|entry| {
            let (hour, minute, second) = model_log_time_hms(entry.started_ms);
            let configuration = history
                .iter()
                .find(|item| {
                    usage::configuration_id(&item.gateway_url, &item.api_key)
                        == entry.configuration_id
                })
                .map(|item| item.display_name().to_owned())
                .unwrap_or_else(|| {
                    format!(
                        "配置 {}",
                        entry.configuration_id.chars().take(8).collect::<String>()
                    )
                });
            let model = entry.model.as_deref().unwrap_or("-");
            let status = entry
                .status_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "-".to_owned());
            let outcome = match entry.outcome.as_str() {
                "success" => i18n::tr("完成", "success"),
                "cancelled" => i18n::tr("客户端取消", "client cancelled"),
                "pending" => i18n::tr("进行中", "pending"),
                _ => i18n::tr("失败", "failed"),
            };
            let status_label = format!("{} {}", status, outcome);
            let request_kind = if entry.request_kind == "inference" {
                i18n::tr("推理请求", "inference").to_owned()
            } else {
                entry.request_kind.clone()
            };
            let tokens = |value: Option<i64>| usage::format_tokens(value);
            // The compact log has one cache column. Prefer cache reads and
            // fall back to writes when that is all the upstream reports.
            let cache = tokens(entry.cache_read.or(entry.cache_write));
            let duration = entry
                .total_ms
                .map(|value| format!("{:.2}s", value as f64 / 1000.0))
                .unwrap_or_else(|| "-".to_owned());
            let first_byte = entry
                .first_byte_ms
                .map(|value| format!("{:.2}s", value as f64 / 1000.0))
                .unwrap_or_else(|| "-".to_owned());
            let endpoint = if let Some(conversion) = &entry.conversion {
                format!("{} → {}", entry.endpoint, conversion)
            } else {
                entry.endpoint.clone()
            };
            format!(
                "{:02}:{:02}:{:02} • {} • {} • {} • {} • ({}/{}/{}) • {} • {} • {} • {} • {}",
                hour,
                minute,
                second,
                entry.client,
                configuration,
                model,
                status_label,
                tokens(entry.input),
                cache,
                tokens(entry.output),
                duration,
                first_byte,
                request_kind,
                endpoint,
                if entry.streaming {
                    i18n::tr("流式", "stream")
                } else {
                    i18n::tr("非流式", "non-stream")
                },
            )
        })
        .collect::<Vec<_>>();
    std::iter::once(header.to_owned())
        .chain(rows)
        .collect::<Vec<_>>()
        .join("\n")
}

fn surface_outline_stroke() -> Stroke {
    Stroke::new(
        1.0,
        Color32::from_white_alpha(layout::HISTORY_OUTLINE_ALPHA),
    )
}

fn log_outline_stroke() -> Stroke {
    // Preserve the dropdown outline's visible color over both log surfaces.
    // Blending translucent white directly over the near-black body made that
    // part of the border much darker than the header and dropdown borders.
    Stroke::new(
        1.0,
        history_background_color().blend(surface_outline_stroke().color),
    )
}

fn paint_surface_outline(painter: &egui::Painter, rect: Rect, radius: u8) {
    // Keep the 1px outline within the intended bounds, including scroll edges.
    painter.rect_stroke(
        rect,
        radius,
        surface_outline_stroke(),
        egui::StrokeKind::Inside,
    );
}

fn render_log_header(ui: &mut egui::Ui, rect: Rect, view: LogView) -> bool {
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        if matches!(view, LogView::Runtime) {
            i18n::tr("运行日志", "Runtime log")
        } else {
            i18n::tr("请求日志", "Request log")
        },
        FontId::proportional(layout::LOG_WINDOW_TITLE_FONT_SIZE),
        TEXT_MAIN,
    );
    let close_rect = Rect::from_center_size(
        egui::pos2(
            rect.right() - layout::LOG_CLOSE_BUTTON_SIZE / 2.0,
            rect.center().y,
        ),
        Vec2::splat(layout::LOG_CLOSE_BUTTON_SIZE),
    );
    let response = ui
        .interact(
            close_rect,
            ui.id().with("runtime-log-close"),
            Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    ui.painter().rect_filled(
        close_rect,
        layout::LOG_CLOSE_BUTTON_RADIUS,
        if response.hovered() {
            Color32::from_rgb(
                layout::LOG_CLOSE_HOVER_BG[0],
                layout::LOG_CLOSE_HOVER_BG[1],
                layout::LOG_CLOSE_HOVER_BG[2],
            )
        } else {
            Color32::from_rgb(
                layout::LOG_CLOSE_BG[0],
                layout::LOG_CLOSE_BG[1],
                layout::LOG_CLOSE_BG[2],
            )
        },
    );
    let glyph = Rect::from_center_size(
        close_rect.center(),
        Vec2::splat(layout::LOG_CLOSE_ICON_SIZE),
    );
    let stroke = Stroke::new(layout::LOG_CLOSE_ICON_STROKE, TEXT_MAIN);
    ui.painter()
        .line_segment([glyph.left_top(), glyph.right_bottom()], stroke);
    ui.painter()
        .line_segment([glyph.right_top(), glyph.left_bottom()], stroke);
    show_hover_tooltip(&response, i18n::tr("关闭", "Close"));
    response.clicked()
}

fn input_surface_bg_color() -> Color32 {
    Color32::from_rgba_unmultiplied(
        layout::INPUT_SURFACE_BG_RED,
        layout::INPUT_SURFACE_BG_GREEN,
        layout::INPUT_SURFACE_BG_BLUE,
        layout::INPUT_SURFACE_BG_ALPHA,
    )
}

fn inline_button_hover_color() -> Color32 {
    Color32::from_rgb(
        layout::INPUT_INLINE_BUTTON_HOVER_RED,
        layout::INPUT_INLINE_BUTTON_HOVER_GREEN,
        layout::INPUT_INLINE_BUTTON_HOVER_BLUE,
    )
}

fn menu_frame_margin() -> egui::Margin {
    // The scroll viewport includes the gutter; content keeps the original 10 px inset.
    egui::Margin {
        left: layout::HISTORY_PADDING,
        top: layout::HISTORY_PADDING,
        right: layout::HISTORY_PADDING - layout::SCROLLBAR_GUTTER,
        bottom: layout::HISTORY_PADDING,
    }
}

fn paint_provider_section_title(ui: &mut egui::Ui, title: &str, line_length: f32, bottom_gap: f32) {
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), layout::PROVIDER_TITLE_HEIGHT),
        Sense::hover(),
    );
    let galley = ui.painter().layout_no_wrap(
        title.to_owned(),
        FontId::proportional(layout::PROVIDER_TITLE_FONT_SIZE),
        TEXT_MUTED,
    );
    let text_rect = Rect::from_center_size(rect.center(), galley.size());
    let [r, g, b, a] = layout::PROVIDER_TITLE_LINE_COLOR;
    let stroke = Stroke::new(1.0, Color32::from_rgba_unmultiplied(r, g, b, a));
    let y = rect.center().y;
    ui.painter().line_segment(
        [
            egui::pos2(text_rect.left() - 8.0 - line_length, y),
            egui::pos2(text_rect.left() - 8.0, y),
        ],
        stroke,
    );
    ui.painter().line_segment(
        [
            egui::pos2(text_rect.right() + 8.0, y),
            egui::pos2(text_rect.right() + 8.0 + line_length, y),
        ],
        stroke,
    );
    ui.painter().galley(text_rect.min, galley, TEXT_MUTED);
    ui.add_space(bottom_gap);
}

fn provider_column_count(width: f32, count: usize) -> usize {
    (((width + layout::PROVIDER_BUTTON_GAP + 0.01)
        / (layout::PROVIDER_BUTTON_SIZE + layout::PROVIDER_BUTTON_GAP))
        .floor() as usize)
        .clamp(1, layout::PROVIDER_MAX_COLUMNS)
        .min(count.max(1))
}

fn model_list_change_status() -> &'static str {
    i18n::tr(
        "模型列表变更 • 下次启动生效",
        "Model list changes • Takes effect on next config",
    )
}

fn is_model_list_change_status(status: &str) -> bool {
    status == "模型列表变更 • 下次启动生效"
        || status == "Model list changes • Takes effect on next config"
}

fn is_model_name_copied_status(status: &str) -> bool {
    status.starts_with("已复制模型名称 • ")
        || status.starts_with("已经复制模型名称 • ")
        || status.starts_with("Copied model name • ")
}

fn history_background_color() -> Color32 {
    Color32::from_rgba_unmultiplied(
        layout::HISTORY_BG_RED,
        layout::HISTORY_BG_GREEN,
        layout::HISTORY_BG_BLUE,
        layout::HISTORY_BG_ALPHA,
    )
}

fn history_item_background_color() -> Color32 {
    Color32::from_rgba_unmultiplied(
        layout::HISTORY_ITEM_BG_RED,
        layout::HISTORY_ITEM_BG_GREEN,
        layout::HISTORY_ITEM_BG_BLUE,
        layout::HISTORY_ITEM_BG_ALPHA,
    )
}

fn history_item_active_background() -> Color32 {
    let [r, g, b, a] = layout::HISTORY_ITEM_ACTIVE;
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

fn history_item_active_text() -> Color32 {
    let [r, g, b] = layout::HISTORY_ITEM_ACTIVE_TEXT;
    Color32::from_rgb(r, g, b)
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

fn paint_window_background(ui: &egui::Ui, rect: Rect) {
    // 整个客户区使用纯色填满；窗口最终外形由 Win32 SetWindowRgn 的直角切口裁剪。
    // 不再叠加 egui 圆角或 DWM 非客户区边框。
    ui.painter().rect_filled(
        rect,
        0.0,
        Color32::from_rgba_unmultiplied(
            layout::WINDOW_BACKGROUND[0],
            layout::WINDOW_BACKGROUND[1],
            layout::WINDOW_BACKGROUND[2],
            layout::WINDOW_BACKGROUND[3],
        ),
    );
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
        "Enter gateway URL & API key, Choose a client.",
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
        FontId::proportional(layout::HEADER_TITLE_MAIN_FONT_SIZE),
        TEXT_MAIN,
    );
    let chinese = i18n::language() == i18n::Language::ZhCn;
    let subtitle_font = FontId::proportional(11.0);
    let subtitle_link_font = FontId::proportional(11.0);
    let subtitle_prefix = if crate::gateway::CUSTOM_EDITION {
        if chinese {
            "让一切变的简单"
        } else {
            "znnz.net"
        }
    } else if chinese {
        "让一切变的简单"
    } else {
        "Simplify everything"
    };
    let subtitle_link = if chinese {
        "获取密钥"
    } else {
        "Get api key"
    };
    let subtitle_separator = " • ";
    let subtitle_prefix_galley = ui.painter().layout_no_wrap(
        subtitle_prefix.to_owned(),
        subtitle_font.clone(),
        TEXT_MUTED,
    );
    let subtitle_prefix_size = subtitle_prefix_galley.size();
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
    let subtitle_text_rect = Rect::from_min_size(
        egui::pos2(title_left, subtitle_center_y - subtitle_prefix_size.y / 2.0),
        subtitle_prefix_size,
    );
    let mut subtitle_cursor = subtitle_text_rect.right();
    if crate::gateway::CUSTOM_EDITION
        && let Some(key_page_url) = crate::gateway::KEY_PAGE_URL.filter(|url| !url.is_empty())
    {
        ui.painter().galley(
            egui::pos2(
                subtitle_cursor,
                subtitle_center_y - subtitle_separator_size.y / 2.0,
            ),
            subtitle_separator_galley.clone(),
            TEXT_MUTED,
        );
        subtitle_cursor += subtitle_separator_size.x;
        let subtitle_link_size = ui
            .painter()
            .layout_no_wrap(
                subtitle_link.to_owned(),
                subtitle_link_font.clone(),
                SUBTITLE_LINK,
            )
            .size();
        let subtitle_link_text_rect = Rect::from_min_size(
            egui::pos2(
                subtitle_cursor,
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
        if subtitle_link_response.hovered() {
            ui.output_mut(|output| output.cursor_icon = egui::CursorIcon::PointingHand);
        }
        if subtitle_link_response.clicked() {
            open_external_link(ui.ctx(), key_page_url);
        }
        subtitle_cursor = subtitle_link_rect.right();
    }

    let first_separator_pos = egui::pos2(
        subtitle_cursor,
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
    ui.painter().galley(
        second_separator_pos,
        subtitle_separator_galley.clone(),
        TEXT_MUTED,
    );
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

    let mut links_right = github_rect.right();
    for (id, texture, url, tooltip) in [
        ("header-star-link", &assets.star, STAR_URL, "Star"),
        ("header-help-link", &assets.help, HELP_URL, "Help"),
    ] {
        ui.painter().galley(
            egui::pos2(
                links_right,
                subtitle_center_y - subtitle_separator_size.y / 2.0,
            ),
            subtitle_separator_galley.clone(),
            TEXT_MUTED,
        );
        let rect = Rect::from_min_size(
            egui::pos2(
                links_right + subtitle_separator_size.x,
                subtitle_center_y + layout::HEADER_SUBTITLE_ICON_OFFSET_Y
                    - layout::HEADER_SUBTITLE_ICON_SIZE / 2.0,
            ),
            Vec2::splat(layout::HEADER_SUBTITLE_ICON_SIZE),
        );
        paint_svg_texture(
            ui,
            texture,
            rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        let response = ui.interact(rect, ui.id().with(id), Sense::click());
        if response.hovered() {
            ui.output_mut(|output| output.cursor_icon = egui::CursorIcon::PointingHand);
        }
        if response.clicked() {
            open_external_link(ui.ctx(), url);
        }
        show_hover_tooltip(&response, tooltip);
        links_right = rect.right();
    }

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
            egui::pos2(drag_right, subtitle_text_rect.top()),
        ),
        // Subtitle text before the link.
        Rect::from_min_max(subtitle_text_rect.min, subtitle_text_rect.max),
        // Empty space to the right of all subtitle links.
        Rect::from_min_max(
            egui::pos2(links_right, subtitle_text_rect.top()),
            egui::pos2(drag_right, subtitle_text_rect.bottom() + 3.0),
        ),
        // Empty band below the subtitle.
        Rect::from_min_max(
            egui::pos2(title_left, subtitle_text_rect.bottom() + 3.0),
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
    history: Option<(&mut bool, &TextureHandle)>,
    connected: bool,
    leading_tooltip: String,
    action_icon: &TextureHandle,
    action_animation: Option<&[TextureHandle]>,
    action_icon_size: Vec2,
    action_tip: &str,
    enabled: bool,
    busy: bool,
    spinner: &[TextureHandle],
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

    ui.painter().rect_filled(
        input_rect,
        layout::INPUT_CORNER_RADIUS,
        input_surface_bg_color(),
    );
    let leading_rect = Rect::from_center_size(
        egui::pos2(input_rect.left() + 14.0, input_rect.center().y),
        Vec2::splat(16.0),
    );
    let leading_response = ui.interact(
        leading_rect,
        ui.id()
            .with(("input-leading-icon", leading_tooltip.as_str())),
        Sense::hover(),
    );
    show_hover_tooltip(&leading_response, leading_tooltip);
    paint_svg_texture(
        ui,
        leading_icon,
        leading_rect,
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );

    let has_visibility = visibility.is_some() || history.is_some();
    let edit_rect = Rect::from_min_max(
        egui::pos2(input_rect.left() + 26.0, input_rect.top() + 2.0),
        egui::pos2(
            input_rect.right()
                - if has_visibility { 32.0 } else { 6.0 }
                - if connected {
                    layout::INPUT_CONNECTION_DOT_SPACE
                } else {
                    0.0
                },
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
            .interactive(true),
    );

    if let Some((show, show_icon, hide_icon)) = visibility {
        let visibility_rect = Rect::from_center_size(
            egui::pos2(input_rect.right() - 17.0, input_rect.center().y),
            Vec2::splat(layout::INPUT_INLINE_BUTTON_SIZE),
        );
        render_key_visibility_button_at(ui, visibility_rect, show, show_icon, hide_icon, true);
    }

    if let Some((open, icon)) = history {
        if connected && !edit_response.changed() {
            let dot_rect = Rect::from_center_size(
                egui::pos2(input_rect.right() - 40.0, input_rect.center().y),
                Vec2::splat(18.0),
            );
            paint_connection_dot_scaled_with_radius(
                ui,
                dot_rect.center(),
                true,
                layout::INPUT_CONNECTION_DOT_SIZE / 11.0,
                layout::INPUT_CONNECTION_DOT_PULSE_RADIUS,
            );
            let dot_response = ui.interact(
                dot_rect,
                ui.id().with("gateway-connection-status"),
                Sense::hover(),
            );
            show_hover_tooltip(&dot_response, i18n::tr("连接中", "Connected"));
        }
        let rect = Rect::from_center_size(
            egui::pos2(input_rect.right() - 17.0, input_rect.center().y),
            Vec2::splat(layout::INPUT_INLINE_BUTTON_SIZE),
        );
        let response = ui.interact(rect, ui.id().with("gateway-history-toggle"), Sense::click());
        if response.hovered() {
            ui.painter().rect_filled(
                rect,
                layout::INPUT_INLINE_BUTTON_CORNER_RADIUS,
                inline_button_hover_color(),
            );
        }
        paint_svg_texture(
            ui,
            icon,
            Rect::from_center_size(rect.center(), Vec2::splat(16.0)),
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        show_hover_tooltip(&response, i18n::tr("提供商切换", "Provider Switch"));
        if response.clicked() {
            *open = !*open;
        }
    }

    let action_clicked = render_icon_button_at(
        ui,
        action_rect,
        action_icon,
        action_icon_size,
        action_animation,
        enabled,
        busy,
        action_tip,
        spinner,
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
    if response.hovered() && enabled {
        ui.painter().rect_filled(
            rect,
            layout::INPUT_INLINE_BUTTON_CORNER_RADIUS,
            inline_button_hover_color(),
        );
    }
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
    animation: Option<&[TextureHandle]>,
    enabled: bool,
    busy: bool,
    tooltip: &str,
    spinner: &[TextureHandle],
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
    if busy {
        paint_spinner(ui, spinner, rect);
    } else {
        if let Some(frames) = animation.filter(|_| enabled) {
            let time = ui.input(|input| input.time);
            let index = ((time.rem_euclid(crate::gui_convert::PERIOD) * crate::gui_convert::FPS)
                as usize)
                .min(frames.len().saturating_sub(1));
            paint_svg_texture(
                ui,
                &frames[index],
                Rect::from_center_size(rect.center(), icon_size),
                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            ui.ctx()
                .request_repaint_after(Duration::from_secs_f64(1.0 / crate::gui_convert::FPS));
        } else {
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
        }
    }
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
fn cascade_paths(rect: Rect, client_panel: Rect) -> [Vec<egui::Pos2>; 4] {
    let scale = rect.width() / CASCADE_VIEW_WIDTH;
    let cards = client_panel.shrink(layout::CLIENT_PANEL_PADDING);
    std::array::from_fn(|index| {
        let curve = CASCADE_CURVES[index];
        let end_x = cards.left()
            + layout::CLIENT_CARD_WIDTH / 2.0
            + index as f32 * (layout::CLIENT_CARD_WIDTH + layout::CLIENT_CARD_HORIZONTAL_GAP);
        let map_start_x = |x| rect.left() + x * scale;
        // The SVG has a 5-unit inset above and below its curves. Map the
        // actual curve extent to the controls so no transparent gap remains.
        let map_y =
            |y| rect.top() + (y - curve.start.1) / (curve.end.1 - curve.start.1) * rect.height();
        sample_cascade_curve(
            CascadeCurve {
                start: (map_start_x(curve.start.0), rect.top()),
                control_1: (map_start_x(curve.control_1.0), map_y(curve.control_1.1)),
                control_2: (end_x, map_y(curve.control_2.1)),
                end: (end_x, client_panel.top()),
            },
            egui::Pos2::ZERO,
            1.0,
        )
    })
}

fn paint_cascade_flow(ui: &egui::Ui, rect: Rect, client_panel: Rect, active_branches: [bool; 4]) {
    let scale = (rect.width() / CASCADE_VIEW_WIDTH).min(rect.height() / CASCADE_VIEW_HEIGHT);
    let paths = cascade_paths(rect, client_panel);
    let painter = ui.painter_at(
        Rect::from_min_max(
            egui::pos2(client_panel.left(), rect.top()),
            egui::pos2(client_panel.right(), client_panel.top()),
        )
        .expand(1.0),
    );

    let stroke = Stroke::new((3.0 * scale).max(0.85), flow_line_color());
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
                flow_dot_color(active_branches[index], alpha),
            );
        }
    }

    // 约 60 FPS，仅重绘这个很小的原生动画，不再反复解析或光栅化 SVG。
    ui.ctx().request_repaint_after(Duration::from_millis(16));
}

fn flow_dot_color(active: bool, alpha: u8) -> Color32 {
    let [r, g, b, base_alpha] = if active {
        layout::FLOW_DOT_ACTIVE_COLOR
    } else {
        layout::FLOW_DOT_INACTIVE_COLOR
    };
    let alpha = (u16::from(base_alpha) * u16::from(alpha) / 255) as u8;
    Color32::from_rgba_unmultiplied(r, g, b, alpha)
}

fn flow_line_color() -> Color32 {
    let [r, g, b, a] = layout::FLOW_LINE_COLOR;
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// Follow the same route through all four controls, showing only the exposed
/// gaps so neither the line nor its moving dots cover text or button icons.
fn paint_input_connection_flow(ui: &egui::Ui, first_row: Rect, second_row: Rect, active: bool) {
    let action_left = first_row.right() - layout::INPUT_ACTION_SIZE;
    let input_right = action_left - layout::INPUT_ACTION_HORIZONTAL_GAP;
    let action_x = action_left + layout::INPUT_ACTION_SIZE / 2.0;
    let path = [
        egui::pos2(input_right, first_row.center().y),
        egui::pos2(action_x, first_row.center().y),
        egui::pos2(action_x, second_row.center().y),
        egui::pos2(input_right, second_row.center().y),
    ];
    let gaps = [
        Rect::from_min_max(
            egui::pos2(input_right, first_row.top()),
            egui::pos2(action_left, first_row.bottom()),
        ),
        Rect::from_min_max(
            egui::pos2(action_left, first_row.top() + layout::INPUT_ACTION_SIZE),
            egui::pos2(first_row.right(), second_row.top()),
        ),
        Rect::from_min_max(
            egui::pos2(input_right, second_row.top()),
            egui::pos2(action_left, second_row.bottom()),
        ),
    ];
    // Keep cascade-flow.svg geometry; colors come from gui_layout.rs.
    // More evenly spaced dots move at a slower pace.
    let scale = (layout::CASCADE_FLOW_WIDTH / CASCADE_VIEW_WIDTH)
        .min(layout::CASCADE_FLOW_HEIGHT / CASCADE_VIEW_HEIGHT);
    let elapsed = ui.input(|input| input.time) as f32;
    for gap in gaps {
        let painter = ui.painter_at(gap);
        painter.add(egui::Shape::line(
            path.to_vec(),
            Stroke::new((3.0 * scale).max(0.85), flow_line_color()),
        ));
        for index in 0..layout::INPUT_FLOW_DOT_COUNT {
            let motion = (elapsed / layout::INPUT_FLOW_DURATION_SECONDS
                + index as f32 / layout::INPUT_FLOW_DOT_COUNT as f32)
                .fract();
            let opacity = interpolate_keyframes(motion, &[0.0, 0.12, 1.0], &[0.0, 1.0, 1.0]);
            let alpha = (opacity * 255.0).round() as u8;
            painter.circle_filled(
                point_along_polyline(&path, motion),
                6.0 * scale,
                flow_dot_color(active, alpha),
            );
        }
    }
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

fn point_to_polyline_distance(point: egui::Pos2, path: &[egui::Pos2]) -> f32 {
    path.windows(2)
        .map(|pair| {
            let segment = pair[1] - pair[0];
            let length_sq = segment.length_sq();
            let t = if length_sq <= f32::EPSILON {
                0.0
            } else {
                ((point - pair[0]).dot(segment) / length_sq).clamp(0.0, 1.0)
            };
            point.distance(pair[0] + segment * t)
        })
        .fold(f32::INFINITY, f32::min)
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
        painter.rect_filled(notch_rect, 0.0, Color32::from_rgb(0, 221, 112));
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
        let connected_endpoint = app
            .attached_urls
            .iter()
            .find(|(client, _)| *client == target)
            .map(|(_, url)| {
                let fingerprint = app
                    .attached_configurations
                    .iter()
                    .find(|(client, _)| *client == target)
                    .map(|(_, fingerprint)| fingerprint.as_str());
                connected_configuration_name(&app.history, fingerprint, url).to_owned()
            });
        let clicked = render_client_card_at(
            ui,
            card_rect,
            target,
            app.client_selected && app.selected == target,
            app.client_is_running(target),
            app.client_is_installed(target),
            app.client_is_attached(target),
            connected_endpoint.as_deref(),
            &app.assets,
        );
        if clicked {
            let running = app.client_is_running(target);
            let installed = app.client_is_installed(target);
            let attached = app.client_is_attached(target);
            app.remember_client_inputs();
            app.selected = target;
            app.client_selected = true;
            app.select_client_configuration(target);
            app.mark_preferences_dirty();
            ui.ctx().request_repaint_after(INPUT_AUTOSAVE_DELAY);
            app.status = selected_client_status(target, installed, running, attached);
            app.status_is_warning = false;
        }
    }
}
#[allow(clippy::too_many_arguments)]
fn render_client_card_at(
    ui: &mut egui::Ui,
    rect: Rect,
    target: ClientTarget,
    selected: bool,
    running: bool,
    installed: bool,
    attached: bool,
    connected_endpoint: Option<&str>,
    assets: &GuiAssets,
) -> bool {
    let size = rect.size();
    let response = ui.interact(
        rect,
        ui.id().with(("client-card", target.id())),
        Sense::click(),
    );
    let background = assets.client_card_background(selected, running, attached, response.hovered());
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
    paint_client_status_dot(ui, dot_center, installed, running, attached);

    show_client_card_tooltip(
        &response,
        target,
        installed,
        running,
        attached,
        connected_endpoint,
    );
    response.clicked()
}

fn paint_connection_dot(ui: &egui::Ui, dot_center: egui::Pos2, attached: bool) {
    paint_connection_dot_scaled(ui, dot_center, attached, 1.0);
}

fn paint_client_status_dot(
    ui: &egui::Ui,
    dot_center: egui::Pos2,
    installed: bool,
    running: bool,
    attached: bool,
) {
    if attached && running {
        paint_connection_dot(ui, dot_center, true);
        return;
    }
    if !installed {
        let radius = 5.5;
        ui.painter()
            .circle_filled(dot_center, radius, Color32::from_rgb(91, 97, 112));
        ui.painter().line_segment(
            [
                dot_center + egui::vec2(-2.0, -2.0),
                dot_center + egui::vec2(2.0, 2.0),
            ],
            Stroke::new(1.5, Color32::from_rgb(43, 43, 43)),
        );
        ui.painter().line_segment(
            [
                dot_center + egui::vec2(2.0, -2.0),
                dot_center + egui::vec2(-2.0, 2.0),
            ],
            Stroke::new(1.5, Color32::from_rgb(43, 43, 43)),
        );
        ui.painter().circle_stroke(
            dot_center,
            radius,
            Stroke::new(2.0, Color32::from_rgba_unmultiplied(10, 12, 16, 100)),
        );
        return;
    }
    if running {
        let radius = 5.5;
        ui.painter()
            .circle_filled(dot_center, radius, Color32::from_rgb(47, 107, 253));
        ui.painter().circle_stroke(
            dot_center,
            radius,
            Stroke::new(2.0, Color32::from_rgba_unmultiplied(10, 12, 16, 100)),
        );
        return;
    }
    let radius = 5.5;
    ui.painter()
        .circle_filled(dot_center, radius, Color32::from_rgb(91, 97, 112));
    ui.painter().circle_stroke(
        dot_center,
        radius,
        Stroke::new(2.0, Color32::from_rgba_unmultiplied(10, 12, 16, 100)),
    );
}

fn paint_connection_dot_scaled(ui: &egui::Ui, dot_center: egui::Pos2, attached: bool, scale: f32) {
    paint_connection_dot_scaled_with_radius(
        ui,
        dot_center,
        attached,
        scale,
        layout::CLIENT_STATUS_DOT_PULSE_RADIUS,
    );
}

fn paint_connection_dot_scaled_with_radius(
    ui: &egui::Ui,
    dot_center: egui::Pos2,
    attached: bool,
    scale: f32,
    pulse_radius: f32,
) {
    if attached {
        // 复刻 CSS box-shadow 脉冲：从灯芯边缘向外扩散并逐渐淡出。
        let elapsed = ui.input(|input| input.time) as f32;
        let phase = (elapsed % layout::CLIENT_STATUS_DOT_PULSE_PERIOD)
            / layout::CLIENT_STATUS_DOT_PULSE_PERIOD;
        let (spread, alpha) = if phase <= 0.7 {
            let progress = phase / 0.7;
            (
                pulse_radius * progress,
                (153.0 * (1.0 - progress)).round() as u8,
            )
        } else {
            (pulse_radius, 0)
        };
        if alpha > 0 {
            let pulse_color = Color32::from_rgba_unmultiplied(98, 197, 85, alpha);
            // CSS 无模糊 box-shadow 的 spread 是从灯芯边缘向外填满的环带。
            // 先画扩大中的实心阴影，再在下方覆盖灯芯，即可让可见环宽随 spread 增长。
            ui.painter()
                .circle_filled(dot_center, (5.5 + spread) * scale, pulse_color);
        }
        // 动画期间持续重绘，保证脉冲不依赖运行任务的轮询间隔。
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }
    ui.painter().circle_filled(
        dot_center,
        5.5 * scale,
        if attached {
            GREEN
        } else {
            Color32::from_rgb(91, 97, 112)
        },
    );
    ui.painter().circle_stroke(
        dot_center,
        5.5 * scale,
        Stroke::new(
            2.0 * scale,
            Color32::from_rgba_unmultiplied(10, 12, 16, 100),
        ),
    );
}

fn render_model_source_and_launch_at(ui: &mut egui::Ui, row_rect: Rect, app: &mut LauncherApp) {
    let center_y = row_rect.center().y;

    let target = app.selected;
    let has_selected_client = app.client_selected;
    let selected_mode = app.model_list_mode(target);
    let locked =
        !has_selected_client || (app.client_is_running(target) && app.client_is_attached(target));

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
            app.client_is_installed(target),
            app.client_is_running(target),
            app.client_is_attached(target),
        );
        app.status_is_warning = false;
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
            app.client_is_installed(target),
            app.client_is_running(target),
            app.client_is_attached(target),
        );
        app.status_is_warning = false;
    }

    let selected_installed = has_selected_client && app.client_is_installed(target);
    let selected_installing = has_selected_client
        && (app.is_running(TaskKind::Install(target))
            || app.is_running(TaskKind::AccountMode(target)));
    let managed = app.gateway_managed_clients.contains(&target);
    if render_install_button_at(
        ui,
        install_rect,
        if selected_installed {
            &app.assets.login_mode
        } else {
            &app.assets.install
        },
        &app.assets.spinner,
        has_selected_client,
        has_selected_client
            && (!selected_installed || managed)
            && !selected_installing
            && !app.client_connect_pending(target)
            && !app.is_running(TaskKind::GatewayUpdate(target)),
        selected_installed,
        selected_installing,
        managed,
    ) {
        app.start_selected_client_install();
    }

    let launch_enabled = has_selected_client
        && !app.configuration_is_current(target)
        && !app.is_running(TaskKind::GatewayUpdate(target))
        && !app.is_running(TaskKind::AccountMode(target))
        && !app.client_connect_pending(target);
    let launch_busy = has_selected_client && app.client_connect_pending(target);
    let launch_tooltip = if launch_busy {
        i18n::tr("正在重启", "Restarting")
    } else if has_selected_client
        && !app.configuration_is_current(target)
        && app.client_is_running(target)
    {
        i18n::tr("重启 • 连接", "Restart • Connect")
    } else {
        i18n::tr("启动 • 连接", "Start • Connect")
    };
    if render_launch_button_at(
        ui,
        launch_rect,
        &app.assets.start,
        launch_enabled,
        launch_busy,
        &app.assets.spinner,
        launch_tooltip,
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
    let fill = secondary_surface_bg_color();
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

fn paint_spinner(ui: &egui::Ui, frames: &[TextureHandle], rect: Rect) {
    let time = ui.input(|input| input.time);
    let index = ((time.rem_euclid(crate::gui_spinner::PERIOD) * crate::gui_spinner::FPS) as usize)
        .min(frames.len() - 1);
    paint_svg_texture(
        ui,
        &frames[index],
        Rect::from_center_size(rect.center(), Vec2::splat(layout::ACTION_SPINNER_SIZE)),
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    ui.ctx()
        .request_repaint_after(Duration::from_secs_f64(1.0 / crate::gui_spinner::FPS));
}

#[allow(clippy::too_many_arguments)]
fn render_install_button_at(
    ui: &mut egui::Ui,
    rect: Rect,
    install_icon: &TextureHandle,
    spinner: &[TextureHandle],
    has_selected_client: bool,
    enabled: bool,
    installed: bool,
    installing: bool,
    managed: bool,
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
        secondary_surface_bg_color()
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
        paint_spinner(ui, spinner, rect);
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
    if installing {
        show_hover_tooltip(
            &response,
            if installed {
                i18n::tr("正在切换账号模式", "Switching account mode")
            } else {
                i18n::tr("正在安装", "Installing")
            },
        );
    } else if !has_selected_client || !installed {
        show_hover_tooltip(&response, i18n::tr("安装客户端", "Install client"));
    } else if managed {
        show_hover_tooltip(&response, i18n::tr("切换账号模式", "Switch account mode"));
    } else {
        show_colored_status_tooltip(&response, i18n::tr("账号模式", "Account mode"), GREEN);
    }
    response.clicked()
}

fn render_launch_button_at(
    ui: &mut egui::Ui,
    rect: Rect,
    icon: &TextureHandle,
    enabled: bool,
    busy: bool,
    spinner: &[TextureHandle],
    tooltip: &'static str,
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
        secondary_surface_bg_color()
    };
    ui.painter()
        .rect_filled(rect, layout::MODEL_ACTION_BUTTON_CORNER_RADIUS, fill);
    if busy {
        paint_spinner(ui, spinner, rect);
    } else {
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
    }
    show_hover_tooltip(&response, tooltip);
    response.clicked()
}

fn render_status_bar(ui: &mut egui::Ui, status_rect: Rect, app: &mut LauncherApp) {
    if app.status_is_warning {
        // 状态栏警告背景贴合窗口底边，窗口外形统一由原生 Region 裁剪。
        ui.painter().rect_filled(
            status_rect,
            0.0,
            Color32::from_rgba_unmultiplied(104, 48, 29, 82),
        );
    }

    let icon = if app.status_is_warning {
        &app.assets.prompt_warn
    } else {
        &app.assets.prompt
    };
    let color = if app.status_is_warning {
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
    // “已...”显示绿色，“未...”显示橙红色。
    let text = app.status.clone();
    let attached_endpoint = if !app.status_is_warning
        && text
            == selected_client_status(
                app.selected,
                app.client_is_installed(app.selected),
                app.client_is_running(app.selected),
                app.client_is_attached(app.selected),
            ) {
        app.attached_urls
            .iter()
            .find(|(target, _)| *target == app.selected)
            .map(|(_, url)| {
                let fingerprint = app
                    .attached_configurations
                    .iter()
                    .find(|(target, _)| *target == app.selected)
                    .map(|(_, fingerprint)| fingerprint.as_str());
                let name = connected_configuration_name(&app.history, fingerprint, url);
                (format!("{text} • "), name.to_owned(), url.clone())
            })
    } else {
        None
    };
    let endpoint_display = attached_endpoint.or_else(|| {
        let (prefix, endpoint) = status_endpoint_parts(&text)?;
        let fingerprint = app
            .gateway_result_configuration
            .as_ref()
            .filter(|(status, _)| status == &text)
            .map(|(_, fingerprint)| fingerprint.as_str());
        let name = connected_configuration_name(&app.history, fingerprint, endpoint);
        Some((prefix.to_owned(), name.to_owned(), endpoint.to_owned()))
    });
    if let Some((prefix, display, endpoint)) = endpoint_display {
        paint_status_text(ui, text_rect, &prefix, color);
        let font = FontId::proportional(11.0);
        let prefix_width = ui
            .painter()
            .layout_no_wrap(prefix, font.clone(), color)
            .size()
            .x;
        let icon_left = text_rect.left() + prefix_width;
        let icon_rect = Rect::from_center_size(
            egui::pos2(
                icon_left + layout::STATUS_ENDPOINT_ICON_SIZE / 2.0,
                text_rect.center().y,
            ),
            Vec2::splat(layout::STATUS_ENDPOINT_ICON_SIZE),
        );
        ui.painter().with_clip_rect(text_rect).image(
            app.assets.server.id(),
            icon_rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        let endpoint_left = icon_rect.right() + layout::STATUS_ENDPOINT_ICON_GAP;
        let galley = ui.painter().layout_no_wrap(display, font, color);
        let endpoint_rect = Rect::from_min_max(
            egui::pos2(endpoint_left.min(text_rect.right()), text_rect.top()),
            egui::pos2(
                (endpoint_left + galley.size().x).min(text_rect.right()),
                text_rect.bottom(),
            ),
        );
        ui.painter().with_clip_rect(text_rect).galley(
            egui::pos2(endpoint_left, text_rect.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
        show_hover_tooltip(
            &ui.interact(
                endpoint_rect,
                ui.id().with("status-endpoint"),
                Sense::hover(),
            ),
            endpoint,
        );
    } else {
        paint_status_text(ui, text_rect, &text, color);
    }

    if let Some(view) = render_log_button_at(ui, log_rect, &app.assets.log) {
        app.log_view = view;
        app.show_logs = true;
        if matches!(view, LogView::Model) {
            app.model_log_reader.refresh(300);
            app.last_model_log_scan = SystemTime::now();
        }
    }
}

fn history_configuration_index(
    history: &[gui_settings::GatewayHistoryEntry],
    fingerprint: Option<&str>,
) -> Option<usize> {
    let fingerprint = fingerprint?;
    history.iter().position(|entry| {
        local_gateway::configuration_fingerprint(&entry.gateway_url, &entry.api_key)
            .is_ok_and(|candidate| candidate == fingerprint)
    })
}

fn conversion_inputs_present(display: &str, endpoint: &str, key: &str) -> bool {
    !display.trim().is_empty() && !endpoint.trim().is_empty() && !key.trim().is_empty()
}

fn configuration_for_client(
    history: &[gui_settings::GatewayHistoryEntry],
    configurations: &gui_settings::ClientConfigurations,
    target: ClientTarget,
    connected_fingerprint: Option<&str>,
) -> Option<gui_settings::ClientConfiguration> {
    if let Some(fingerprint) = connected_fingerprint {
        if let Some(index) = history_configuration_index(history, Some(fingerprint)) {
            let entry = &history[index];
            return Some(gui_settings::ClientConfiguration {
                gateway_url: entry.gateway_url.clone(),
                api_key: entry.api_key.clone(),
                preset_provider: None,
            });
        }
        // A removed history entry may still be remembered by its client.
        // Never substitute an unrelated draft for a connected configuration.
        let configuration = configurations.get(target.id())?;
        return local_gateway::configuration_fingerprint(
            &configuration.gateway_url,
            &configuration.api_key,
        )
        .is_ok_and(|candidate| candidate == fingerprint)
        .then(|| configuration.clone());
    }
    configurations.get(target.id()).cloned()
}

fn connected_configuration_name<'a>(
    history: &'a [gui_settings::GatewayHistoryEntry],
    fingerprint: Option<&str>,
    endpoint: &'a str,
) -> &'a str {
    history
        .iter()
        .find(|entry| {
            normalized_gateway_url(&entry.gateway_url) == normalized_gateway_url(endpoint)
                && fingerprint.is_some_and(|applied| {
                    local_gateway::configuration_fingerprint(&entry.gateway_url, &entry.api_key)
                        .is_ok_and(|value| value == applied)
                })
        })
        .map(|entry| entry.display_name())
        .unwrap_or(endpoint)
}

fn status_endpoint_parts(text: &str) -> Option<(&str, &str)> {
    let (prefix, endpoint) = text.rsplit_once(" • ")?;
    (endpoint.starts_with("https://") || endpoint.starts_with("http://"))
        .then_some((&text[..prefix.len() + " • ".len()], endpoint))
}

fn configuration_matches(
    configurations: &[(ClientTarget, String)],
    target: ClientTarget,
    url: &str,
    key: &str,
) -> bool {
    let Ok(fingerprint) = local_gateway::configuration_fingerprint(url, key) else {
        return false;
    };
    configurations
        .iter()
        .any(|(client, applied)| *client == target && *applied == fingerprint)
}

fn configuration_has_running_client(
    configurations: &[(ClientTarget, String)],
    attached_clients: &[ClientTarget],
    client_processes: &[(ClientTarget, u32)],
    url: &str,
    key: &str,
) -> bool {
    let Ok(fingerprint) = local_gateway::configuration_fingerprint(url, key) else {
        return false;
    };
    configurations.iter().any(|(target, applied)| {
        *applied == fingerprint
            && attached_clients.contains(target)
            && client_processes.iter().any(|(client, _)| client == target)
    })
}

fn paint_status_text(ui: &egui::Ui, text_rect: Rect, text: &str, default_color: Color32) {
    let painter = ui.painter().with_clip_rect(text_rect);
    let regular_font = FontId::proportional(11.0);
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
        paint_status_segment(
            &painter,
            &regular_font,
            &text[start..end],
            color,
            text_rect,
            &mut x,
        );
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
    let text = text.split(" • http").next().unwrap_or(text);
    let mut ranges = Vec::new();
    for (needle, color) in [
        ("已安装", GREEN),
        ("运行中", GREEN),
        ("已连接", GREEN),
        ("未安装", NOT_INSTALLED),
        ("未运行", NOT_INSTALLED),
        ("未连接", NOT_INSTALLED),
        ("成功连接", GREEN),
        ("成功拉取", GREEN),
        ("Installed", GREEN),
        ("Not running", NOT_INSTALLED),
        ("Running", GREEN),
        ("Not installed", NOT_INSTALLED),
        ("Connected", GREEN),
        ("Not connected", NOT_INSTALLED),
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

fn render_log_button_at(ui: &mut egui::Ui, rect: Rect, icon: &TextureHandle) -> Option<LogView> {
    let ctx = ui.ctx().clone();
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
    let popup_rect = Rect::from_min_size(
        egui::pos2(
            rect.left() - layout::LOG_MENU_WIDTH,
            rect.bottom() - layout::LOG_MENU_HEIGHT,
        ),
        egui::vec2(layout::LOG_MENU_WIDTH, layout::LOG_MENU_HEIGHT),
    );
    let pointer = ctx.input(|input| input.pointer.latest_pos());
    let menu_hovered = pointer.is_some_and(|pos| popup_rect.contains(pos));
    let menu_id = ui.id().with("status-log-menu-deadline");
    let was_open = ctx.data(|data| data.get_temp::<bool>(menu_id).unwrap_or(false));
    // The menu can be entered from the button, but the log window itself never
    // counts as a hover target. This avoids reopening the menu over the log
    // content while still allowing the user to move onto an option and click it.
    let show_menu = response.hovered() || (was_open && menu_hovered);
    ctx.data_mut(|data| data.insert_temp(menu_id, show_menu));
    let mut choice = None;
    if show_menu {
        egui::Area::new(ui.id().with("status-log-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(popup_rect.left_top())
            .show(ui.ctx(), |ui| {
                let menu_bg = Color32::from_rgb(
                    layout::LOG_MENU_BG[0],
                    layout::LOG_MENU_BG[1],
                    layout::LOG_MENU_BG[2],
                );
                let button_bg = Color32::from_rgb(
                    layout::LOG_MENU_BUTTON_BG[0],
                    layout::LOG_MENU_BUTTON_BG[1],
                    layout::LOG_MENU_BUTTON_BG[2],
                );
                let frame = egui::Frame::new()
                    .fill(menu_bg)
                    .inner_margin(egui::Margin::same(layout::LOG_MENU_PADDING as i8))
                    .corner_radius(layout::LOG_MENU_CORNER_RADIUS)
                    .shadow(egui::epaint::Shadow::NONE)
                    .stroke(Stroke::NONE)
                    .show(ui, |ui| {
                        ui.set_min_size(egui::vec2(
                            layout::LOG_MENU_WIDTH - layout::LOG_MENU_PADDING * 2.0,
                            layout::LOG_MENU_HEIGHT - layout::LOG_MENU_PADDING * 2.0,
                        ));
                        ui.spacing_mut().item_spacing.y = layout::LOG_MENU_BUTTON_GAP;
                        for (index, (view, label)) in [
                            (LogView::Runtime, i18n::tr("运行日志", "Runtime log")),
                            (LogView::Model, i18n::tr("请求日志", "Request log")),
                        ]
                        .into_iter()
                        .enumerate()
                        {
                            let (button_rect, button_response) = ui.allocate_exact_size(
                                egui::vec2(
                                    layout::LOG_MENU_WIDTH - layout::LOG_MENU_PADDING * 2.0,
                                    layout::LOG_MENU_BUTTON_HEIGHT,
                                ),
                                Sense::click(),
                            );
                            ui.painter().rect_filled(
                                button_rect,
                                layout::LOG_MENU_BUTTON_RADIUS,
                                if button_response.hovered() {
                                    ORANGE
                                } else {
                                    button_bg
                                },
                            );
                            ui.painter().text(
                                button_rect.center(),
                                Align2::CENTER_CENTER,
                                label,
                                FontId::proportional(layout::LOG_MENU_BUTTON_FONT_SIZE),
                                TEXT_MAIN,
                            );
                            if button_response.clicked() {
                                choice = Some(view);
                            }
                            // Keep the loop's identity stable if the menu is
                            // later expanded with more log views.
                            let _ = index;
                        }
                    });
                // Match the history dropdown's 1px translucent outline.
                paint_surface_outline(
                    ui.painter(),
                    frame.response.rect,
                    layout::LOG_MENU_CORNER_RADIUS,
                );
            });
    }
    choice
}

fn show_hover_tooltip(response: &egui::Response, text: impl Into<String>) {
    show_hover_tooltip_at(response, text, egui::RectAlign::TOP);
}

fn show_history_key_tooltip(
    response: &egui::Response,
    entry: &gui_settings::GatewayHistoryEntry,
    item_rect: Rect,
    summary: Option<&usage::Summary>,
    assets: &GuiAssets,
    available: bool,
) -> Option<Rect> {
    let summary = summary.filter(|_| available).cloned().unwrap_or_default();
    let count = |value: i64| {
        if available && value > 0 {
            value.to_string()
        } else {
            "-".into()
        }
    };
    let blue = layout::HISTORY_USAGE_BLUE;
    let rows = [
        (
            i18n::tr("请求数", "Request"),
            [
                (
                    format!("{} {}", i18n::tr("总数", "Total"), count(summary.total)),
                    blue,
                ),
                (
                    format!("{} {}", i18n::tr("成功", "Success"), count(summary.success)),
                    layout::HISTORY_USAGE_GREEN,
                ),
                (
                    format!("{} {}", i18n::tr("失败", "Fail"), count(summary.failed)),
                    layout::HISTORY_USAGE_ORANGE,
                ),
            ],
        ),
        (
            "Tokens",
            [
                (
                    format!(
                        "{} {}",
                        i18n::tr("总数", "Total"),
                        summary.total_token_text()
                    ),
                    blue,
                ),
                (
                    format!(
                        "{} {}",
                        i18n::tr("输入", "Input"),
                        summary.token_text(summary.input, summary.input_known)
                    ),
                    blue,
                ),
                (
                    format!(
                        "{} {}",
                        i18n::tr("输出", "Output"),
                        summary.token_text(summary.output, summary.output_known)
                    ),
                    blue,
                ),
            ],
        ),
        (
            i18n::tr("缓存数", "Cache"),
            [
                (
                    format!("{} {}", i18n::tr("比例", "Ratio"), summary.hit_ratio()),
                    blue,
                ),
                (
                    format!(
                        "{} {}",
                        i18n::tr("命中", "Hits"),
                        summary.token_text(summary.cache_read, summary.read_known)
                    ),
                    blue,
                ),
                (
                    format!(
                        "{} {}",
                        i18n::tr("写入", "Writes"),
                        summary.token_text(summary.cache_write, summary.write_known)
                    ),
                    blue,
                ),
            ],
        ),
    ];
    let painter = response.ctx.layer_painter(response.layer_id);
    let measure = |text: &str, size: f32| {
        painter
            .layout_no_wrap(text.into(), FontId::proportional(size), TEXT_MUTED)
            .size()
            .x
    };
    let label_width = rows
        .iter()
        .map(|(label, _)| measure(label, layout::HISTORY_USAGE_STAT_FONT_SIZE))
        .fold(0.0_f32, f32::max);
    let prefix = layout::HISTORY_USAGE_ICON_SIZE
        + layout::HISTORY_USAGE_ICON_TEXT_GAP
        + label_width
        + measure(" : ", layout::HISTORY_USAGE_STAT_FONT_SIZE);
    let required_width = rows
        .iter()
        .map(|(_, badges)| {
            prefix
                + badges
                    .iter()
                    .map(|(text, _)| {
                        measure(text, layout::HISTORY_USAGE_BADGE_FONT_SIZE)
                            + 2.0 * layout::HISTORY_USAGE_BADGE_PADDING_X
                    })
                    .sum::<f32>()
                + 2.0 * layout::HISTORY_USAGE_ROW_GAP
                + 2.0 * f32::from(layout::TOOLTIP_PADDING_HORIZONTAL)
                + 1.0
        })
        .fold(item_rect.width(), f32::max);
    // The popup is an ordinary foreground Area rather than egui's hover
    // Tooltip. Tooltip deliberately closes while a ScrollArea is being
    // dragged, which made the statistics scrollbar impossible to use.
    let [r, g, b, a] = layout::HISTORY_USAGE_BG;
    let background = Color32::from_rgba_unmultiplied(r, g, b, a);
    let padding_x = f32::from(layout::TOOLTIP_PADDING_HORIZONTAL);
    let padding_y = f32::from(layout::TOOLTIP_PADDING_VERTICAL)
        + f32::from(layout::HISTORY_USAGE_EXTRA_PADDING_Y);
    let row_height = painter
        .layout_no_wrap(
            "总数 Total".into(),
            FontId::proportional(layout::HISTORY_USAGE_BADGE_FONT_SIZE),
            TEXT_MUTED,
        )
        .size()
        .y
        + 2.0 * layout::HISTORY_USAGE_BADGE_PADDING_Y;
    let gap = layout::HISTORY_USAGE_ROW_GAP;
    let icon_column_width = layout::HISTORY_USAGE_ICON_SIZE + layout::HISTORY_USAGE_ICON_TEXT_GAP;
    let inner_width = (item_rect.width() - 2.0 * padding_x).max(1.0);
    let statistics_content_width = (required_width - 2.0 * padding_x - icon_column_width).max(1.0);
    let statistics_view_width = (inner_width - icon_column_width).max(1.0);
    let overflow = statistics_content_width > statistics_view_width + 0.5;
    let statistics_height = row_height * 3.0
        + gap * 2.0
        + if overflow {
            f32::from(layout::SCROLLBAR_GUTTER) + 2.0
        } else {
            0.0
        };
    let inner_height = layout::HISTORY_USAGE_ROW_HEIGHT * 2.0 + gap * 2.0 + statistics_height;
    let outer_size = egui::vec2(item_rect.width(), inner_height + padding_y * 2.0);
    let screen = response.ctx.input(|input| input.content_rect());
    let mut popup_pos = egui::pos2(
        item_rect.right() - outer_size.x,
        item_rect.top() - outer_size.y - layout::TOOLTIP_ARROW_HEIGHT,
    );
    popup_pos.x = popup_pos.x.clamp(
        screen.left(),
        (screen.right() - outer_size.x).max(screen.left()),
    );
    let above = popup_pos.y >= screen.top();
    if !above {
        popup_pos.y = item_rect.bottom() + layout::TOOLTIP_ARROW_HEIGHT;
    }
    popup_pos.y = popup_pos.y.clamp(
        screen.top(),
        (screen.bottom() - outer_size.y).max(screen.top()),
    );

    let area_id = egui::Id::new((
        "history-statistics-popup",
        usage::configuration_id(&entry.gateway_url, &entry.api_key),
    ));
    let mut shown_rect = None;
    egui::Area::new(area_id)
        .order(egui::Order::Foreground)
        .fixed_pos(popup_pos)
        .default_size(outer_size)
        .constrain(false)
        .interactable(true)
        .sense(Sense::click_and_drag())
        .fade_in(true)
        .show(&response.ctx, |ui| {
            let frame = egui::Frame::new()
                .fill(background)
                .stroke(Stroke::NONE)
                .corner_radius(layout::HISTORY_USAGE_CORNER_RADIUS)
                .inner_margin(egui::Margin::symmetric(
                    layout::TOOLTIP_PADDING_HORIZONTAL,
                    layout::TOOLTIP_PADDING_VERTICAL + layout::HISTORY_USAGE_EXTRA_PADDING_Y,
                ))
                .shadow(egui::epaint::Shadow {
                    offset: [0, 1],
                    blur: 3,
                    spread: 0,
                    color: Color32::from_rgba_unmultiplied(0, 0, 0, 28),
                });
            frame.show(ui, |ui| {
                ui.set_min_size(egui::vec2(inner_width, inner_height));
                ui.spacing_mut().item_spacing.y = gap;
                ui.style_mut().interaction.selectable_labels = true;
                history_tooltip_row(ui, &assets.url, &entry.gateway_url);
                history_tooltip_row(ui, &assets.key, &entry.api_key);
                ui.allocate_ui_with_layout(
                    egui::vec2(inner_width, statistics_height),
                    egui::Layout::left_to_right(egui::Align::Min),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.allocate_ui_with_layout(
                            egui::vec2(icon_column_width, statistics_height),
                            egui::Layout::top_down(egui::Align::Min),
                            |icons| {
                                for _ in 0..3 {
                                    let (rect, _) = icons.allocate_exact_size(
                                        egui::vec2(icon_column_width, row_height),
                                        Sense::hover(),
                                    );
                                    paint_svg_texture(
                                        icons,
                                        &assets.statistics,
                                        Rect::from_center_size(
                                            egui::pos2(
                                                rect.left() + layout::HISTORY_USAGE_ICON_SIZE / 2.0,
                                                rect.center().y,
                                            ),
                                            Vec2::splat(layout::HISTORY_USAGE_ICON_SIZE),
                                        ),
                                        Rect::from_min_max(
                                            egui::pos2(0.0, 0.0),
                                            egui::pos2(1.0, 1.0),
                                        ),
                                        Color32::WHITE,
                                    );
                                }
                            },
                        );
                        egui::ScrollArea::horizontal()
                            .id_salt((
                                "history-statistics-scroll",
                                usage::configuration_id(&entry.gateway_url, &entry.api_key),
                            ))
                            .max_width(statistics_view_width)
                            .auto_shrink([false, true])
                            .content_margin(egui::Margin {
                                left: 0,
                                right: 0,
                                top: 0,
                                bottom: if overflow {
                                    layout::SCROLLBAR_GUTTER + 2
                                } else {
                                    0
                                },
                            })
                            .show(ui, |ui| {
                                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                    ui.set_width(statistics_content_width);
                                    ui.spacing_mut().item_spacing.y = gap;
                                    for (label, badges) in rows {
                                        history_statistics_row(ui, label, label_width, badges);
                                    }
                                });
                            });
                    },
                );
            });
            shown_rect = Some(ui.min_rect());
        });
    let rect = shown_rect?;
    let arrow_x = item_rect.center().x.clamp(
        rect.left() + layout::TOOLTIP_ARROW_WIDTH / 2.0,
        rect.right() - layout::TOOLTIP_ARROW_WIDTH / 2.0,
    );
    let painter = response
        .ctx
        .layer_painter(egui::LayerId::new(egui::Order::Foreground, area_id));
    response.ctx.move_to_top(painter.layer_id());
    if above {
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(arrow_x - layout::TOOLTIP_ARROW_WIDTH / 2.0, rect.bottom()),
                egui::pos2(arrow_x + layout::TOOLTIP_ARROW_WIDTH / 2.0, rect.bottom()),
                egui::pos2(arrow_x, rect.bottom() + layout::TOOLTIP_ARROW_HEIGHT),
            ],
            background,
            Stroke::NONE,
        ));
    } else {
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(arrow_x - layout::TOOLTIP_ARROW_WIDTH / 2.0, rect.top()),
                egui::pos2(arrow_x + layout::TOOLTIP_ARROW_WIDTH / 2.0, rect.top()),
                egui::pos2(arrow_x, rect.top() - layout::TOOLTIP_ARROW_HEIGHT),
            ],
            background,
            Stroke::NONE,
        ));
    }
    Some(rect)
}

fn history_tooltip_row(ui: &mut egui::Ui, icon: &TextureHandle, text: &str) {
    let width = ui.available_width();
    let text_width =
        (width - layout::HISTORY_USAGE_ICON_SIZE - layout::HISTORY_USAGE_ICON_TEXT_GAP).max(1.0);
    let font = FontId::proportional(layout::HISTORY_USAGE_FONT_SIZE);
    let measure = |value: &str| {
        ui.painter()
            .layout_no_wrap(value.to_owned(), font.clone(), TEXT_MUTED)
            .size()
            .x
    };
    let display_text = if measure(text) <= text_width {
        text.to_owned()
    } else {
        let boundaries = text
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(text.len()))
            .collect::<Vec<_>>();
        let mut low = 0;
        let mut high = boundaries.len().saturating_sub(1);
        while low < high {
            let middle = (low + high).div_ceil(2);
            if measure(&format!("{}...", &text[..boundaries[middle]])) <= text_width {
                low = middle;
            } else {
                high = middle.saturating_sub(1);
            }
        }
        format!("{}...", &text[..boundaries[low]])
    };
    ui.allocate_ui_with_layout(
        egui::vec2(width, layout::HISTORY_USAGE_ROW_HEIGHT),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = layout::HISTORY_USAGE_ICON_TEXT_GAP;
            let (icon_rect, _) = ui
                .allocate_exact_size(Vec2::splat(layout::HISTORY_USAGE_ICON_SIZE), Sense::hover());
            paint_svg_texture(
                ui,
                icon,
                icon_rect,
                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            ui.allocate_ui_with_layout(
                egui::vec2(text_width, layout::HISTORY_USAGE_ROW_HEIGHT),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(display_text)
                                .size(layout::HISTORY_USAGE_FONT_SIZE)
                                .color(TEXT_MUTED),
                        )
                        .truncate()
                        .halign(egui::Align::Min)
                        .selectable(true),
                    );
                },
            );
        },
    );
}

fn history_statistics_row(
    ui: &mut egui::Ui,
    label: &str,
    label_width: f32,
    badges: [(String, [u8; 4]); 3],
) {
    let row_height = ui
        .painter()
        .layout_no_wrap(
            "总数 Total".into(),
            FontId::proportional(layout::HISTORY_USAGE_BADGE_FONT_SIZE),
            TEXT_MUTED,
        )
        .size()
        .y
        + 2.0 * layout::HISTORY_USAGE_BADGE_PADDING_Y;
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), row_height),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.allocate_ui_with_layout(
                egui::vec2(label_width, row_height),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_min_width(label_width);
                    ui.add(
                        egui::Label::new(
                            RichText::new(label)
                                .size(layout::HISTORY_USAGE_STAT_FONT_SIZE)
                                .color(TEXT_MUTED),
                        )
                        .extend()
                        .halign(egui::Align::Min)
                        .selectable(true),
                    );
                },
            );
            ui.add(
                egui::Label::new(
                    RichText::new(" : ")
                        .size(layout::HISTORY_USAGE_STAT_FONT_SIZE)
                        .color(TEXT_MUTED),
                )
                .halign(egui::Align::Min)
                .selectable(true),
            );
            for (index, (text, rgba)) in badges.into_iter().enumerate() {
                if index > 0 {
                    ui.add_space(layout::HISTORY_USAGE_ROW_GAP);
                }
                egui::Frame::new()
                    .fill(Color32::from_rgba_unmultiplied(
                        rgba[0], rgba[1], rgba[2], rgba[3],
                    ))
                    .corner_radius(layout::HISTORY_USAGE_BADGE_RADIUS)
                    .inner_margin(egui::Margin::symmetric(
                        layout::HISTORY_USAGE_BADGE_PADDING_X.round() as i8,
                        layout::HISTORY_USAGE_BADGE_PADDING_Y.round() as i8,
                    ))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(text)
                                    .size(layout::HISTORY_USAGE_BADGE_FONT_SIZE)
                                    .color(Color32::from_rgb(rgba[0], rgba[1], rgba[2])),
                            )
                            .halign(egui::Align::Min)
                            .selectable(true),
                        );
                    });
            }
        },
    );
}

fn show_client_card_tooltip(
    response: &egui::Response,
    target: ClientTarget,
    installed: bool,
    running: bool,
    attached: bool,
    connected_endpoint: Option<&str>,
) {
    let states = client_tooltip_states(installed, running, attached, connected_endpoint);
    let state_text = states.join(" • ");
    let width = tooltip_width_for_texts(&response.ctx, &[target.title(), &state_text], 12.0);
    show_hover_tooltip_contents_width(response, egui::RectAlign::TOP, width, |ui| {
        // Match the previous plain multiline tooltip: no extra vertical
        // line gap and no layout gap around the bullet separator.
        ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
        ui.label(
            RichText::new(target.title())
                .size(12.0)
                .color(Color32::WHITE),
        );
        let mut states_line = egui::text::LayoutJob::default();
        for (index, state) in states.into_iter().enumerate() {
            if index > 0 {
                states_line.append(
                    " • ",
                    0.0,
                    egui::TextFormat::simple(FontId::proportional(12.0), Color32::WHITE),
                );
            }
            states_line.append(
                state,
                0.0,
                egui::TextFormat::simple(
                    FontId::proportional(12.0),
                    if index == 1
                        && running
                        && attached
                        && connected_endpoint.is_some_and(|endpoint| !endpoint.is_empty())
                    {
                        Color32::WHITE
                    } else {
                        client_status_color(state)
                    },
                ),
            );
        }
        ui.add(egui::Label::new(states_line).halign(egui::Align::Center));
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
    let width = tooltip_width_for_texts(&response.ctx, &[text], 12.0);
    show_hover_tooltip_contents_width(response, egui::RectAlign::TOP, width, |ui| {
        ui.add(egui::Label::new(
            RichText::new(text).size(12.0).color(color),
        ));
    });
}

fn show_hover_tooltip_at(
    response: &egui::Response,
    text: impl Into<String>,
    align: egui::RectAlign,
) {
    let text = text.into();
    let width = tooltip_width_for_texts(&response.ctx, &[&text], 12.0);
    show_hover_tooltip_contents_width(response, align, width, |ui| {
        ui.add(
            egui::Label::new(RichText::new(text).size(12.0).color(Color32::WHITE))
                .halign(egui::Align::Center),
        );
    });
}

fn tooltip_width_for_texts(ctx: &egui::Context, texts: &[&str], font_size: f32) -> f32 {
    let content_width = ctx.fonts_mut(|fonts| {
        texts
            .iter()
            .map(|text| {
                fonts
                    .layout_no_wrap(
                        (*text).to_owned(),
                        FontId::proportional(font_size),
                        Color32::WHITE,
                    )
                    .size()
                    .x
            })
            .fold(0.0_f32, f32::max)
    });
    (content_width + 2.0 + 2.0 * f32::from(layout::TOOLTIP_PADDING_HORIZONTAL)).max(1.0)
}

fn show_hover_tooltip_contents_width(
    response: &egui::Response,
    align: egui::RectAlign,
    width: f32,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    show_hover_tooltip_contents_with_frame(
        response,
        align,
        (width > 0.0).then_some(width),
        tooltip_background_color(),
        false,
        add_contents,
    );
}

fn show_hover_tooltip_contents_with_frame(
    response: &egui::Response,
    align: egui::RectAlign,
    width: Option<f32>,
    background: Color32,
    usage_style: bool,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    let mut tooltip = egui::Tooltip::for_enabled(response);
    if let Some(width) = width {
        tooltip = tooltip.width(width);
    }
    tooltip.popup = tooltip
        .popup
        .align(align)
        .align_alternatives(&[])
        .gap(layout::TOOLTIP_GAP)
        .frame(
            egui::Frame::popup(response.ctx.global_style().as_ref())
                .fill(background)
                .stroke(Stroke::NONE)
                .inner_margin(egui::Margin::symmetric(
                    layout::TOOLTIP_PADDING_HORIZONTAL,
                    layout::TOOLTIP_PADDING_VERTICAL
                        + if usage_style {
                            layout::HISTORY_USAGE_EXTRA_PADDING_Y
                        } else {
                            0
                        },
                ))
                .corner_radius(if usage_style {
                    layout::HISTORY_USAGE_CORNER_RADIUS
                } else {
                    layout::TOOLTIP_CORNER_RADIUS
                })
                .shadow(egui::epaint::Shadow {
                    offset: [0, 1],
                    blur: 3,
                    spread: 0,
                    color: Color32::from_rgba_unmultiplied(0, 0, 0, 28),
                }),
        );
    let shown = tooltip.show(|ui| {
        if let Some(width) = width {
            ui.set_width((width - 2.0 * f32::from(layout::TOOLTIP_PADDING_HORIZONTAL)).max(1.0));
        }
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
        let fill = background;
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
    installed: bool,
    running: bool,
    attached: bool,
) -> String {
    selected_client_status_for_language(target, installed, running, attached, i18n::language())
}

fn selected_client_status_for_language(
    target: ClientTarget,
    installed: bool,
    running: bool,
    attached: bool,
    language: i18n::Language,
) -> String {
    let runtime = if running {
        language.text("运行中", "Running")
    } else {
        language.text("未运行", "Not running")
    };
    let attachment = if attached {
        language.text("已连接", "Connected")
    } else {
        language.text("未连接", "Not connected")
    };
    let _ = installed;
    format!("{} • {} • {}", target.title(), runtime, attachment)
}

#[cfg(test)]
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

fn gateway_attachment_label(attached: bool) -> &'static str {
    if attached {
        i18n::tr("已连接", "Connected")
    } else {
        i18n::tr("未连接", "Not connected")
    }
}

fn client_installation_label(installed: bool) -> &'static str {
    if installed {
        i18n::tr("已安装", "Installed")
    } else {
        i18n::tr("未安装", "Not installed")
    }
}

fn client_status_color(status: &str) -> Color32 {
    if status.starts_with('未') || status.starts_with("Not ") {
        NOT_INSTALLED
    } else {
        GREEN
    }
}

fn client_tooltip_states(
    installed: bool,
    running: bool,
    attached: bool,
    connected_endpoint: Option<&str>,
) -> [&str; 2] {
    if running
        && attached
        && let Some(endpoint) = connected_endpoint.filter(|endpoint| !endpoint.is_empty())
    {
        return [gateway_attachment_label(true), endpoint];
    }
    if running {
        return [client_running_label(), gateway_attachment_label(attached)];
    }
    [
        client_installation_label(installed),
        gateway_attachment_label(attached),
    ]
}

fn client_running_label() -> &'static str {
    i18n::tr("运行中", "Running")
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
#[cfg(test)]
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

fn is_gateway_result_status(status: &str) -> bool {
    status.starts_with("成功连接 • ")
        || status.starts_with("成功拉取 • ")
        || status.starts_with("Connected • ")
        || status.starts_with("Fetched • ")
}

fn is_protocol_conversion_status(status: &str) -> bool {
    status.starts_with("协议转换 • ") || status.starts_with("Protocol conversion • ")
}
fn is_account_mode_status(status: &str) -> bool {
    status.contains("已切换账号模式") || status.contains("Switched to account mode")
}

fn failed_task_status(task: &RunningTask) -> String {
    if task.log_text.contains("需手动重启")
        || task.log_text.contains("Manual restart required")
        || task.log_text.contains("自动重启 • 失败!")
        || task.log_text.contains("Auto-restart • failed!")
    {
        format!(
            "{} • {}",
            task.label,
            i18n::tr("自动重启 • 失败!", "Auto-restart • failed!")
        )
    } else if task
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
                "失败 • 请查看运行日志 ...",
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
        TaskKind::GatewayUpdate(target) => format!(
            "{} • {}",
            target.title(),
            i18n::tr("协议转换", "Protocol conversion")
        ),
        TaskKind::Client(target) => format!(
            "{} • {}",
            target.title(),
            i18n::tr("启动 • 连接", "Start • Connect")
        ),
        TaskKind::Install(target) => {
            format!("{} • {}", target.title(), i18n::tr("安装", "Install"))
        }
        TaskKind::AccountMode(target) => format!(
            "{} • {}",
            target.title(),
            i18n::tr("切换账号模式", "Switch account mode")
        ),
    }
}

fn task_log_slug(kind: TaskKind, label: &str) -> String {
    match kind {
        TaskKind::GatewayTest if is_fetch_models_label(label) => "gateway-models".to_owned(),
        TaskKind::GatewayTest => "gateway-test".to_owned(),
        TaskKind::GatewayUpdate(target) => format!("{}-gateway-update", target.id()),
        TaskKind::Client(target) => format!("{}-launch", target.id()),
        TaskKind::Install(target) => format!("{}-install", target.id()),
        TaskKind::AccountMode(target) => format!("{}-account-mode", target.id()),
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

fn gateway_update_arguments(target: ClientTarget, gateway_url: &str) -> Vec<String> {
    vec![
        "internal-gateway-update".to_owned(),
        target.id().to_owned(),
        "--gateway-url".to_owned(),
        normalized_gateway_url(gateway_url),
    ]
}

fn validate_gateway_inputs(gateway_url: &str, api_key: &str) -> Result<()> {
    let parsed = match url::Url::parse(gateway_url.trim()) {
        Ok(parsed) => parsed,
        Err(_) => bail!(
            "{}",
            i18n::tr("接口地址格式无效!", "Invalid gateway URL format!")
        ),
    };
    if parsed.host_str().is_none() {
        bail!(
            "{}",
            i18n::tr("接口地址缺少主机名!", "Gateway URL is missing a host!")
        );
    }
    if parsed.scheme() != "https"
        && !matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
    {
        bail!(
            "{}",
            i18n::tr("接口地址必须使用 HTTPS!", "Gateway URLs must use HTTPS!")
        );
    }
    if api_key.trim().is_empty() {
        bail!(
            "{}",
            i18n::tr("请输入 API Key!", "Please enter the API key!")
        );
    }
    Ok(())
}

fn gateway_input_tooltip(display_value: &str, endpoint: &str) -> String {
    let display_value = display_value.trim();
    let endpoint = endpoint.trim();
    if display_value.is_empty() {
        return i18n::tr("接口地址", "Endpoint").to_owned();
    }

    let endpoint_is_url = url::Url::parse(endpoint).ok().is_some_and(|parsed| {
        parsed.host_str().is_some()
            && (parsed.scheme() == "https"
                || (parsed.scheme() == "http"
                    && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "::1"))))
    });
    if endpoint_is_url {
        endpoint.to_owned()
    } else {
        i18n::tr("接口地址", "Endpoint").to_owned()
    }
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
    let (pixels, _) = rgba.as_chunks_mut::<4>();
    for pixel in pixels {
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
        style.animation_time = layout::MENU_ANIMATION_SECONDS;
        style.spacing.button_padding = Vec2::new(8.0, 5.0);
        style.spacing.scroll = egui::style::ScrollStyle {
            floating: true,
            bar_width: layout::SCROLLBAR_HOVER_WIDTH,
            floating_width: layout::SCROLLBAR_IDLE_WIDTH,
            floating_allocated_width: 0.0,
            content_margin: egui::Margin {
                left: 0,
                top: 0,
                right: layout::SCROLLBAR_GUTTER,
                bottom: 0,
            },
            dormant_handle_opacity: 0.4,
            ..egui::style::ScrollStyle::floating()
        };
        style.interaction.tooltip_delay = layout::TOOLTIP_DELAY_SECONDS;
        style.interaction.tooltip_grace_time = 0.15;
        style.interaction.show_tooltips_only_when_still = false;
        style.visuals.dark_mode = true;
        style.visuals.panel_fill = Color32::TRANSPARENT;
        style.visuals.window_fill = Color32::TRANSPARENT;
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
    let directory = local.join("Agent-Switch").join("logs");
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

    #[test]
    fn provider_grid_fits_seven_icons_inside_the_real_scroll_content_width() {
        let ctx = egui::Context::default();
        configure_style(&ctx);
        let mut content_width = 0.0;
        let input_width = layout::WINDOW_WIDTH
            - layout::CONTENT_PADDING_LEFT
            - layout::CONTENT_PADDING_RIGHT
            - layout::INPUT_ACTION_HORIZONTAL_GAP
            - layout::INPUT_ACTION_SIZE;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            egui::Frame::new()
                .inner_margin(menu_frame_margin())
                .show(ui, |ui| {
                    ui.set_width(
                        input_width - 2.0 * f32::from(layout::HISTORY_PADDING)
                            + f32::from(layout::SCROLLBAR_GUTTER),
                    );
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            content_width = ui.available_width();
                            paint_provider_section_title(
                                ui,
                                "Provider records",
                                layout::PROVIDER_RECORDS_TITLE_LINE_LENGTH,
                                layout::PROVIDER_RECORDS_TITLE_BOTTOM_GAP,
                            );
                            ui.allocate_exact_size(Vec2::new(content_width, 500.0), Sense::hover());
                        });
                });
        });
        assert_eq!(provider_column_count(content_width, 8), 7);
        let row_width = 7.0 * layout::PROVIDER_BUTTON_SIZE + 6.0 * layout::PROVIDER_BUTTON_GAP;
        assert!(
            row_width <= content_width + 0.01,
            "row={row_width}, content={content_width}"
        );
        assert_eq!(provider_column_count(content_width, 3), 3);
        assert_eq!(provider_column_count(content_width, 0), 1);
        assert!(is_model_list_change_status("模型列表变更 • 下次配置生效"));
        assert!(is_model_list_change_status(
            "Model list changes • Takes effect on next config"
        ));
    }

    #[test]
    fn all_branches_animate_with_matching_active_and_inactive_colors() {
        let ctx = egui::Context::default();
        let first = Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(350.0, 34.0));
        let second = first.translate(egui::vec2(0.0, 44.0));
        let cascade = Rect::from_min_size(egui::pos2(20.0, 100.0), egui::vec2(260.0, 36.0));
        let panel =
            Rect::from_min_size(egui::pos2(0.0, cascade.bottom()), egui::vec2(350.0, 100.0));
        let branch_paths = cascade_paths(cascade, panel);
        let mut masks = vec![[false; 4], [true; 4]];
        for index in 0..4 {
            let mut mask = [false; 4];
            mask[index] = true;
            masks.push(mask);
        }
        let mut time = 24.0;
        for mask in masks {
            let mut seen = [false; 4];
            let mut rested = [false; 4];
            for _ in 0..48 {
                time += 0.5;
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(400.0, 200.0),
                        )),
                        time: Some(time),
                        ..Default::default()
                    },
                    |ui| {
                        paint_input_connection_flow(
                            ui,
                            first,
                            second,
                            mask.iter().any(|active| *active),
                        );
                        paint_cascade_flow(ui, cascade, panel, mask);
                    },
                );
                let lines = output
                    .shapes
                    .iter()
                    .filter(|shape| {
                        matches!(&shape.shape,
                            egui::Shape::Path(path) if !path.closed && path.stroke.width > 0.0
                        )
                    })
                    .count();
                assert_eq!(
                    lines, 7,
                    "all input and client lines remain visible: {mask:?}"
                );
                let mut input_dots = 0;
                let mut frame_seen = [false; 4];
                for shape in &output.shapes {
                    if let egui::Shape::Circle(dot) = &shape.shape {
                        if dot.center.y < cascade.top() {
                            input_dots += 1;
                            let expected = if mask.iter().any(|active| *active) {
                                Color32::from_rgba_unmultiplied(0, 221, 112, dot.fill.a())
                            } else {
                                Color32::from_black_alpha(dot.fill.a())
                            };
                            assert_eq!(dot.fill, expected);
                        } else {
                            let branch = branch_paths
                                .iter()
                                .enumerate()
                                .min_by(|(_, left), (_, right)| {
                                    point_to_polyline_distance(dot.center, left)
                                        .partial_cmp(&point_to_polyline_distance(dot.center, right))
                                        .unwrap_or(std::cmp::Ordering::Equal)
                                })
                                .map(|(index, _)| index)
                                .expect("dot must lie on a client branch");
                            let expected = if mask[branch] {
                                Color32::from_rgba_unmultiplied(0, 221, 112, dot.fill.a())
                            } else {
                                Color32::from_black_alpha(dot.fill.a())
                            };
                            assert_eq!(
                                dot.fill, expected,
                                "branch {branch} color must match its state"
                            );
                            seen[branch] = true;
                            frame_seen[branch] = true;
                        }
                    }
                }
                assert!(input_dots > 0);
                for (index, visible) in frame_seen.into_iter().enumerate() {
                    rested[index] |= !visible;
                }
            }
            assert_eq!(seen, [true; 4], "every branch must animate over its cycle");
            assert_eq!(
                rested, [true; 4],
                "every branch keeps its staggered rest periods"
            );
        }
    }

    #[test]
    fn log_window_keeps_centered_title_fixed_bounds_and_working_close_button() {
        for size in [egui::vec2(380.0, 430.0), egui::vec2(320.0, 300.0)] {
            let ctx = egui::Context::default();
            install_fonts(&ctx);
            configure_style(&ctx);
            let screen = Rect::from_min_size(egui::Pos2::ZERO, size);
            let log =
                "A long log line that must wrap inside the viewport without widening the window. "
                    .repeat(60);
            let mut time = 0.0;
            let mut frame = |events| {
                time += 0.25;
                let mut open = true;
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        time: Some(time),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        open = render_log_window(ui.ctx(), screen, LogView::Runtime, &log, "");
                    },
                );
                (open, output)
            };
            for _ in 0..3 {
                frame(Vec::new());
            }
            let (open, output) = frame(Vec::new());
            assert!(open);
            let surface = |rgb: [u8; 3]| {
                output
                    .shapes
                    .iter()
                    .find_map(|shape| match &shape.shape {
                        egui::Shape::Rect(rect)
                            if rect.fill == Color32::from_rgb(rgb[0], rgb[1], rgb[2]) =>
                        {
                            Some(rect.rect)
                        }
                        _ => None,
                    })
                    .expect("log surface background")
            };
            let header = surface(layout::LOG_HEADER_BG);
            let content = surface(layout::LOG_CONTENT_BG);
            assert_eq!(header.bottom(), content.top());
            let bounds = header.union(content);
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::Rect(rect) if rect.rect == bounds
                    && rect.stroke == log_outline_stroke()
                    && rect.stroke_kind == egui::StrokeKind::Inside
                    && rect.corner_radius == egui::CornerRadius::same(layout::LOG_WINDOW_CORNER_RADIUS)
            )));
            assert!((bounds.width() - (size.x - 20.0)).abs() < 1.0, "{bounds:?}");
            assert!(
                (bounds.height() - layout::LOG_WINDOW_HEIGHT.min(size.y - 20.0)).abs() < 1.0,
                "{bounds:?}"
            );
            let title_center = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text)
                        if text.galley.text() == i18n::tr("运行日志", "Runtime log") =>
                    {
                        Some(text.pos + text.galley.size() / 2.0)
                    }
                    _ => None,
                })
                .unwrap();
            assert!((title_center.x - bounds.center().x).abs() < 1.0);
            assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::LineSegment { points, stroke }
                if points[0].y == points[1].y && *stroke == surface_outline_stroke()
                    && (points[1].x - points[0].x - bounds.width()).abs() < 1.0
            )));
            let close = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Rect(rect)
                        if rect.fill == Color32::from_rgb(60, 60, 60)
                            && rect.rect.size() == Vec2::splat(layout::LOG_CLOSE_BUTTON_SIZE) =>
                    {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .expect("close button uses the gray default surface");
            assert_eq!(close.size(), egui::vec2(24.0, 24.0));
            frame(vec![egui::Event::PointerMoved(close.center())]);
            let (_, hovered) = frame(vec![egui::Event::PointerMoved(close.center())]);
            assert!(hovered.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::Rect(rect) if rect.rect == close && rect.fill == Color32::from_rgb(201, 0, 0)
            )));
            frame(vec![egui::Event::PointerButton {
                pos: close.center(),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }]);
            let (open, _) = frame(vec![egui::Event::PointerButton {
                pos: close.center(),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            assert!(!open, "custom close button must close the log window");
        }
    }

    #[test]
    fn statistics_popup_keeps_icons_and_supports_selection_while_scrolling() {
        for width in [240.0, 520.0] {
            let ctx = egui::Context::default();
            install_fonts(&ctx);
            configure_style(&ctx);
            let assets = GuiAssets::load(&ctx);
            let entry = gui_settings::GatewayHistoryEntry {
                gateway_url: "https://example.test".into(),
                api_key: "synthetic-test-key".into(),
                name: String::new(),
                hidden_models: Vec::new(),
                conversion_enabled: false,
            };
            let summary = usage::Summary {
                total: 1234567890,
                success: 1234567880,
                failed: 10,
                input: Some(1234567890),
                output: Some(987654321),
                ..Default::default()
            };
            let item = Rect::from_min_size(egui::pos2(20.0, 300.0), egui::vec2(width, 34.0));
            let frame = |events: Vec<egui::Event>| {
                let mut popup = Rect::NOTHING;
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(600.0, 600.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ui| {
                        let response = ui.interact(item, ui.id().with("test-row"), Sense::click());
                        popup = show_history_key_tooltip(
                            &response,
                            &entry,
                            item,
                            Some(&summary),
                            &assets,
                            true,
                        )
                        .unwrap();
                    },
                );
                (popup, output)
            };
            // Area needs a sizing frame and a completed hit-test frame.
            for _ in 0..3 {
                frame(Vec::new());
            }
            let (popup, output) = frame(Vec::new());
            assert!(
                (popup.width() - width).abs() < 1.5,
                "popup width: {popup:?}"
            );
            let icon_rects = |output: &egui::FullOutput| {
                output
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::Shape::Mesh(mesh) if mesh.texture_id == assets.statistics.id() => {
                            Some(mesh.calc_bounds())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            let before = icon_rects(&output);
            assert_eq!(before.len(), 3);
            let expected_left = before[0].left()
                + layout::HISTORY_USAGE_ICON_SIZE
                + layout::HISTORY_USAGE_ICON_TEXT_GAP;
            for label in [
                "https://example.test",
                "synthetic-test-key",
                "请求数",
                "Tokens",
                "缓存数",
            ] {
                let position = output
                    .shapes
                    .iter()
                    .find_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) if text.galley.text() == label => Some(text.pos),
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("missing statistics text {label}"));
                assert!(
                    (position.x - expected_left).abs() < 1.0,
                    "{label} must align with the left edge: {position:?}, expected {expected_left}"
                );
            }
            assert!(
                before
                    .windows(2)
                    .all(|pair| (pair[0].left() - pair[1].left()).abs() < 0.1)
            );
            let mut badge_rows = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Rect(rect)
                        if rect.corner_radius
                            == egui::CornerRadius::same(layout::HISTORY_USAGE_BADGE_RADIUS)
                            && rect.rect.height() > layout::HISTORY_USAGE_ICON_SIZE =>
                    {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            badge_rows.sort_by(|a, b| a.top().total_cmp(&b.top()));
            badge_rows.dedup_by(|a, b| (a.top() - b.top()).abs() < 0.1);
            assert_eq!(badge_rows.len(), 3);
            for rows in badge_rows.windows(2) {
                assert!(
                    (rows[1].top() - rows[0].bottom() - 4.0).abs() < 0.1,
                    "statistics badge rows must have a 4 px gap: {rows:?}"
                );
            }
            let text = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == "Tokens" => {
                        Some((text.pos, text.galley.size()))
                    }
                    _ => None,
                })
                .expect("Tokens label is rendered");
            let start = text.0 + egui::vec2(1.0, text.1.y / 2.0);
            let end = start + egui::vec2(text.1.x - 1.0, 0.0);
            let button = |pos, pressed| egui::Event::PointerButton {
                pos,
                pressed,
                button: egui::PointerButton::Primary,
                modifiers: egui::Modifiers::NONE,
            };
            frame(vec![egui::Event::PointerMoved(start)]);
            frame(vec![button(start, true)]);
            frame(vec![egui::Event::PointerMoved(end)]);
            frame(vec![button(end, false)]);
            let (_, copied) = frame(vec![egui::Event::Copy]);
            assert!(
                copied
                    .platform_output
                    .commands
                    .iter()
                    .any(|command| matches!(
                        command, egui::OutputCommand::CopyText(text) if text.contains("Token")
                    )),
                "statistics text must be selectable and copyable"
            );
            let pointer = egui::pos2(popup.center().x, before[1].center().y);
            frame(vec![
                egui::Event::PointerMoved(pointer),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    phase: egui::TouchPhase::Move,
                    delta: egui::vec2(-120.0, 0.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            for _ in 0..8 {
                frame(Vec::new());
            }
            let (_, scrolled) = frame(Vec::new());
            assert_eq!(
                icon_rects(&scrolled),
                before,
                "horizontal scrolling must not move or hide the icons"
            );
            let after_text = scrolled.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Tokens" => Some(text.pos),
                _ => None,
            });
            if width < 300.0 {
                assert!(
                    after_text.is_none_or(|position| position.x < text.0.x - 10.0),
                    "overflowing statistics should scroll horizontally"
                );
                let handle = scrolled
                    .shapes
                    .iter()
                    .rev()
                    .find_map(|shape| match &shape.shape {
                        egui::Shape::Rect(rect)
                            if rect.rect.height() <= layout::SCROLLBAR_HOVER_WIDTH
                                && rect.rect.width() > 10.0
                                && rect.rect.top() > before[2].bottom() =>
                        {
                            Some(rect.rect)
                        }
                        _ => None,
                    })
                    .expect("overflowing statistics have a horizontal scrollbar");
                let start = handle.center();
                let end = egui::pos2(popup.left() + 1.0, start.y);
                frame(vec![egui::Event::PointerMoved(start)]);
                frame(vec![button(start, true)]);
                frame(vec![egui::Event::PointerMoved(end)]);
                frame(vec![button(end, false)]);
                let (_, dragged) = frame(Vec::new());
                assert_eq!(icon_rects(&dragged), before);
                let restored = dragged.shapes.iter().find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == "Tokens" => Some(text.pos),
                    _ => None,
                });
                assert_eq!(
                    restored,
                    Some(text.0),
                    "scrollbar dragging restores the leftmost statistics"
                );
            } else {
                assert_eq!(after_text, Some(text.0));
            }
        }
    }

    #[test]
    fn renaming_does_not_change_history_row_spacing() {
        for editing in [false, true] {
            let context = egui::Context::default();
            let mut name = "Renamed configuration".to_owned();
            let _ = context.run_ui(egui::RawInput::default(), |ui| {
                ui.spacing_mut().item_spacing.y = layout::HISTORY_ITEM_VERTICAL_GAP;
                let (first, _) = ui.allocate_exact_size(
                    egui::vec2(280.0, layout::HISTORY_ITEM_HEIGHT),
                    Sense::hover(),
                );
                if editing {
                    ui.place(
                        first.shrink2(egui::vec2(24.0, 2.0)),
                        egui::TextEdit::singleline(&mut name).frame(egui::Frame::NONE),
                    );
                }
                let (second, _) = ui.allocate_exact_size(
                    egui::vec2(280.0, layout::HISTORY_ITEM_HEIGHT),
                    Sense::hover(),
                );
                assert!(
                    (second.top() - first.bottom() - layout::HISTORY_ITEM_VERTICAL_GAP).abs()
                        < 0.01
                );
            });
        }
    }

    #[test]
    fn current_configuration_matches_client_url_and_key() {
        let applied = vec![(
            ClientTarget::CodexDesktop,
            local_gateway::configuration_fingerprint("https://api.example.test", "first-key")
                .unwrap(),
        )];
        assert!(configuration_matches(
            &applied,
            ClientTarget::CodexDesktop,
            " https://api.example.test/ ",
            "first-key"
        ));
        assert!(!configuration_matches(
            &applied,
            ClientTarget::CodexDesktop,
            "https://api.example.test",
            "second-key"
        ));
        assert!(!configuration_matches(
            &applied,
            ClientTarget::CodexDesktop,
            "https://other.example.test",
            "first-key"
        ));
        assert!(!configuration_matches(
            &applied,
            ClientTarget::ClaudeDesktop,
            "https://api.example.test",
            "first-key"
        ));
        assert!(!configuration_matches(
            &[],
            ClientTarget::CodexDesktop,
            "https://api.example.test",
            "first-key"
        ));
        assert!(!configuration_matches(
            &applied,
            ClientTarget::CodexDesktop,
            "invalid",
            "first-key"
        ));
    }

    #[test]
    fn configuration_light_requires_a_matching_running_and_attached_client() {
        let url = "https://api.example.test";
        let key = "first-key";
        let fingerprint = local_gateway::configuration_fingerprint(url, key).unwrap();
        let applied: Vec<_> = ClientTarget::ALL
            .into_iter()
            .map(|target| (target, fingerprint.clone()))
            .collect();
        let check = |attached: &[ClientTarget], processes: &[(ClientTarget, u32)], key: &str| {
            configuration_has_running_client(&applied, attached, processes, url, key)
        };
        assert!(!check(&ClientTarget::ALL, &[], key));
        for target in ClientTarget::ALL {
            let running = [(target, 123)];
            assert!(check(&[target], &running, key));
            assert!(!check(&[], &running, key));
            assert!(!check(&[target], &running, "second-key"));
        }
        let running = [
            (ClientTarget::CodexDesktop, 123),
            (ClientTarget::ClaudeCode, 456),
        ];
        assert!(check(&ClientTarget::ALL, &running, key));
        assert!(check(&ClientTarget::ALL, &running[1..], key));
        assert!(!check(&[ClientTarget::ClaudeDesktop], &running, key));
        assert!(!configuration_has_running_client(
            &applied,
            &ClientTarget::ALL,
            &running,
            "https://other.example.test",
            key
        ));
    }

    #[test]
    fn status_tooltip_targets_only_the_endpoint() {
        let status = "Codex Desktop • 运行中 • 已连接";
        assert!(status_endpoint_parts(status).is_none());
        assert!(status_endpoint_parts("Codex Desktop • 自动重启 • 失败!").is_none());
        let url = "https://api.example.test/long/path";
        let text = format!("{status} • {url}");
        let (prefix, endpoint) = status_endpoint_parts(&text).unwrap();
        assert_eq!(endpoint, url);
        assert_eq!(format!("{prefix}{endpoint}"), text);
    }

    #[test]
    fn connected_name_matches_the_applied_key_not_just_the_url() {
        let endpoint = "https://example.test";
        let mut history = vec![
            gui_settings::GatewayHistoryEntry {
                gateway_url: endpoint.into(),
                api_key: "key-a".into(),
                name: "First".into(),
                hidden_models: Vec::new(),
                conversion_enabled: false,
            },
            gui_settings::GatewayHistoryEntry {
                gateway_url: endpoint.into(),
                api_key: "key-b".into(),
                name: "Second • 已连接".into(),
                hidden_models: Vec::new(),
                conversion_enabled: false,
            },
        ];
        let applied = local_gateway::configuration_fingerprint(endpoint, "key-b").unwrap();
        assert_eq!(
            history_configuration_index(&history, Some(&applied)),
            Some(1)
        );
        assert_eq!(history_configuration_index(&history, None), None);
        assert_eq!(history_configuration_index(&history, Some("unknown")), None);
        assert_eq!(
            connected_configuration_name(&history, Some(&applied), endpoint),
            "Second • 已连接"
        );
        history[1].name = "Renamed".into();
        assert_eq!(
            connected_configuration_name(&history, Some(&applied), endpoint),
            "Renamed"
        );
        history.remove(1);
        assert_eq!(history_configuration_index(&history, Some(&applied)), None);
        assert_eq!(
            connected_configuration_name(&history, Some(&applied), endpoint),
            endpoint
        );
        assert_eq!(
            connected_configuration_name(&history, None, endpoint),
            endpoint
        );
    }

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
            "Codex Desktop • 启动 • 连接"
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
            "\u{feff}===== Claude Desktop • 安装 =====\r\n第一段\r\n\r\n\r\n第二段\r\n\r\n=========================================\r\n",
        );
        assert_eq!(
            formatted,
            "===== Claude Desktop • 安装 =====\n\n第一段\n\n第二段\n\n========================================="
        );
        assert_eq!(
            formatted
                .lines()
                .filter(|line| line.starts_with("===== Claude Desktop"))
                .count(),
            1
        );
        assert_eq!(formatted.matches(LOG_SECTION_FOOTER).count(), 1);
    }

    #[test]
    fn log_formatter_indents_inline_error_details() {
        assert_eq!(
            format_log_section("Claude Desktop • 安装", "错误: 下载超时"),
            "===== Claude Desktop • 安装 =====\n\n错误：\n  下载超时\n\n========================================="
        );
    }

    #[test]
    fn log_sections_keep_creation_order_and_requested_spacing() {
        let first = format_log_section("Codex Desktop • 启动 • 连接", "日志段落1\n\n日志段落2");
        let second = format_log_section("Codex Desktop • 安装", "日志段落1\n\n日志段落2");
        let history = join_ordered_log_sections(vec![(2, second), (1, first)]);
        assert_eq!(
            history,
            "===== Codex Desktop • 启动 • 连接 =====\n\n日志段落1\n\n日志段落2\n\n=========================================\n\n===== Codex Desktop • 安装 =====\n\n日志段落1\n\n日志段落2\n\n========================================="
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
        let (pixels, _) = rgba.as_chunks::<4>();
        assert!(pixels.iter().any(|pixel| pixel[3] != 0));
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
            configuration_fingerprint: None,
            sequence: 1,
            kind: TaskKind::Client(ClientTarget::CodexDesktop),
            child: None,
            job: None,
            worker_pid: 4321,
            log_path: PathBuf::from(r"C:\Temp\codex-desktop.log"),
            label: "Codex Desktop".to_owned(),
            log_title: "Codex Desktop • 启动 • 连接".to_owned(),
            log_text: String::new(),
            last_log_read: UNIX_EPOCH,
        }];

        assert!(tasks_contain_client(&tasks, ClientTarget::CodexDesktop));
        assert!(!tasks_contain_client(&tasks, ClientTarget::ClaudeDesktop));
    }

    #[test]
    fn client_status_labels_use_green_for_positive_states() {
        assert_eq!(gateway_attachment_label(true), "已连接");
        assert_eq!(gateway_attachment_label(false), "未连接");
        assert_eq!(client_installation_label(true), "已安装");
        assert_eq!(client_installation_label(false), "未安装");
        assert_eq!(client_status_color("已连接"), GREEN);
        assert_eq!(client_status_color("未连接"), NOT_INSTALLED);
    }

    #[test]
    fn selected_client_status_reports_runtime_and_connection() {
        assert_eq!(
            selected_client_status(ClientTarget::ClaudeCode, true, true, true),
            "Claude Code • 运行中 • 已连接"
        );
        assert_eq!(
            selected_client_status(ClientTarget::ClaudeCode, true, false, false),
            "Claude Code • 未运行 • 未连接"
        );
        assert_eq!(
            selected_client_status(ClientTarget::ClaudeCode, false, false, false),
            "Claude Code • 未运行 • 未连接"
        );
    }

    #[test]
    fn english_status_uses_the_requested_order() {
        assert_eq!(
            selected_client_status_for_language(
                ClientTarget::CodexDesktop,
                true,
                true,
                true,
                i18n::Language::En,
            ),
            "Codex Desktop • Running • Connected"
        );
        assert_eq!(
            selected_client_status_for_language(
                ClientTarget::CodexDesktop,
                true,
                false,
                false,
                i18n::Language::En,
            ),
            "Codex Desktop • Not running • Not connected"
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
            configuration_fingerprint: None,
            sequence: 1,
            kind: TaskKind::GatewayTest,
            child: None,
            job: None,
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
    fn gateway_result_status_is_not_replaced_by_client_state() {
        assert!(is_gateway_result_status(
            "成功连接 • https://api.example.com"
        ));
        assert!(is_gateway_result_status(
            "成功拉取 • 4 个可见模型 • https://api.example.com"
        ));
        assert!(!is_gateway_result_status("Codex Desktop • 运行中 • 已连接"));
    }

    #[test]
    fn model_copy_result_is_preserved_in_both_languages() {
        for status in [
            "已复制模型名称 • gpt-6.1-sol",
            "Copied model name • gpt-6.1-sol",
        ] {
            assert!(is_model_name_copied_status(status));
            assert!(!is_model_list_change_status(status));
            assert!(status_endpoint_parts(status).is_none());
        }
        assert!(!is_model_name_copied_status(
            "Codex Desktop • 运行中 • 已连接"
        ));
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
    fn attached_endpoint_does_not_receive_status_highlights() {
        let text = "Codex Desktop • 运行中 • 已连接 • https://example.com/Started/已连接";
        let ranges = status_highlight_ranges(text);
        assert_eq!(ranges.len(), 2);
        assert!(
            ranges
                .iter()
                .all(|(_, end, _)| *end < text.find("https://").unwrap())
        );
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
            configuration_fingerprint: None,
            sequence: 1,
            kind: TaskKind::GatewayTest,
            child: None,
            job: None,
            worker_pid: 0,
            log_path: PathBuf::new(),
            label: "连接测试".to_owned(),
            log_title: "网关 • 连接测试".to_owned(),
            log_text: format!(
                "错误: 无法连接模型接口。{}、请配置代理后重试",
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
    fn client_tooltip_uses_two_states_when_running() {
        assert_eq!(
            client_tooltip_states(true, true, true, None),
            ["运行中", "已连接"]
        );
        assert_eq!(
            client_tooltip_states(true, true, false, None),
            ["运行中", "未连接"]
        );
        assert_eq!(
            client_tooltip_states(true, false, false, None),
            ["已安装", "未连接"]
        );
        assert_eq!(
            client_tooltip_states(false, false, false, None),
            ["未安装", "未连接"]
        );
        assert_eq!(
            client_tooltip_states(true, true, true, Some("常用提供商")),
            ["已连接", "常用提供商"]
        );
        assert_eq!(
            client_tooltip_states(true, true, true, Some("https://example.test/v1")),
            ["已连接", "https://example.test/v1"]
        );
        assert_eq!(
            client_tooltip_states(true, false, true, Some("常用提供商")),
            ["已安装", "已连接"]
        );
        assert_eq!(
            client_tooltip_states(true, true, false, Some("常用提供商")),
            ["运行中", "未连接"]
        );
    }

    #[test]
    fn conversion_requires_both_endpoint_and_key_even_for_named_inputs() {
        assert!(conversion_inputs_present(
            "常用供应商",
            "https://example.test/v1",
            "key"
        ));
        assert!(conversion_inputs_present(
            "https://example.test/v1",
            "https://example.test/v1",
            "key"
        ));
        assert!(!conversion_inputs_present(
            "",
            "https://example.test/v1",
            "key"
        ));
        assert!(!conversion_inputs_present("供应商", "", "key"));
        assert!(!conversion_inputs_present(
            "供应商",
            "https://example.test/v1",
            "  "
        ));
    }

    #[test]
    fn connected_client_configuration_takes_priority_over_separate_remembered_drafts() {
        let mut configurations = gui_settings::ClientConfigurations::new();
        for target in ClientTarget::ALL {
            configurations.insert(
                target.id().into(),
                gui_settings::ClientConfiguration {
                    gateway_url: format!("https://{}.test/v1", target.id()),
                    api_key: target.id().into(),
                    preset_provider: Some("预设名称".into()),
                },
            );
        }
        let mut history = Vec::new();
        gui_settings::record_history(&mut history, "https://active.test/v1", "active-key");
        history[0].name = "已连接供应商".into();
        let fingerprint =
            local_gateway::configuration_fingerprint("https://active.test/v1", "active-key")
                .unwrap();
        for target in ClientTarget::ALL {
            let draft = configuration_for_client(&history, &configurations, target, None).unwrap();
            assert_eq!(draft.api_key, target.id());
            assert_eq!(draft.preset_provider.as_deref(), Some("预设名称"));
            let connected =
                configuration_for_client(&history, &configurations, target, Some(&fingerprint))
                    .unwrap();
            assert_eq!(connected.api_key, "active-key");
            assert_eq!(
                connected_configuration_name(&history, Some(&fingerprint), &connected.gateway_url),
                "已连接供应商"
            );
            assert!(
                configuration_for_client(&history, &configurations, target, Some("unknown"))
                    .is_none()
            );
        }
        configurations.insert(
            ClientTarget::CodexCli.id().into(),
            gui_settings::ClientConfiguration {
                gateway_url: "https://active.test/v1".into(),
                api_key: "active-key".into(),
                preset_provider: None,
            },
        );
        assert_eq!(
            configuration_for_client(
                &[],
                &configurations,
                ClientTarget::CodexCli,
                Some(&fingerprint)
            )
            .unwrap()
            .api_key,
            "active-key"
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
        assert_eq!(format!("{error:#}"), "接口地址格式无效!");
        assert!(!format!("{error:#}").contains("relative URL"));
    }

    #[test]
    fn gateway_input_tooltip_uses_endpoint_without_connection_state() {
        assert_eq!(gateway_input_tooltip("", ""), "接口地址");
        assert_eq!(
            gateway_input_tooltip("OpenAI", "https://api.openai.com/v1"),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            gateway_input_tooltip("https://api.openai.com/v1", "https://api.openai.com/v1"),
            "https://api.openai.com/v1"
        );
        assert_eq!(gateway_input_tooltip("Draft", "not a URL"), "接口地址");
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
            ("star", include_bytes!("../assets/gui/svg/star.svg"), 48, 48),
            ("help", include_bytes!("../assets/gui/svg/help.svg"), 48, 48),
            ("url", include_bytes!("../assets/gui/svg/url.svg"), 64, 64),
            (
                "history-edit",
                include_bytes!("../assets/gui/svg/history-edit.svg"),
                64,
                64,
            ),
            (
                "history-confirm",
                include_bytes!("../assets/gui/svg/history-confirm.svg"),
                64,
                64,
            ),
            ("key", include_bytes!("../assets/gui/svg/key.svg"), 64, 64),
            (
                "login-mode",
                include_bytes!("../assets/gui/svg/Login-mode.svg"),
                64,
                64,
            ),
            (
                "statistics",
                include_bytes!("../assets/gui/svg/statistics.svg"),
                64,
                64,
            ),
            (
                "history-expand",
                include_bytes!("../assets/gui/svg/history-expand.svg"),
                64,
                64,
            ),
            (
                "history-delete",
                include_bytes!("../assets/gui/svg/history-delete.svg"),
                64,
                64,
            ),
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
                "spinners",
                include_bytes!("../assets/gui/svg/spinners.svg"),
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
                "client-card-hover-selected",
                include_bytes!("../assets/gui/svg/client-card-hover-selected.svg"),
                100,
                100,
            ),
            (
                "client-card-running",
                include_bytes!("../assets/gui/svg/client-card-running.svg"),
                100,
                100,
            ),
            (
                "client-card-running-connected",
                include_bytes!("../assets/gui/svg/client-card-running-connected.svg"),
                100,
                100,
            ),
        ];

        for (name, bytes, width, height) in assets {
            let rgba = render_svg_rgba(bytes, *width, *height)
                .unwrap_or_else(|error| panic!("{name}.svg 渲染失败: {error:#}"));
            assert_eq!(rgba.len(), (*width * *height * 4) as usize);
            assert!(
                rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] != 0),
                "{name}.svg 渲染结果完全透明"
            );
        }
    }
}
