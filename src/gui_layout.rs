//! GUI 布局参数区。
//!
//! 这里相当于 C 程序顶部的 #define 样式宏区域。
//! 调整界面布局时，优先只修改“可调布局参数区”；gui.rs 会直接引用这里的值。
//!
//! 整体布局结构：
//!   顶部标题栏
//!   1px 分割线
//!   中间功能栏
//!     区块 1：两个输入框 + 两个操作按钮
//!     区块 2
//!       区块 2-1：客户端卡片
//!       区块 2-2：模型列表选择 + 启动按钮
//!   1px 分割线
//!   底部状态栏

// ============================================================================
// 可调布局参数区：以后主要修改这里
// ============================================================================

// ---------------------------------------------------------------------------
// 窗口整体
// ---------------------------------------------------------------------------

// 窗口整体宽度。窗口本身不设置内边距，各个区域分别控制自己的内边距。
pub const WINDOW_WIDTH: f32 = 380.0;

// 窗口整体高度不在这里手动设置；它会在文件底部根据标题栏、功能栏、
// 状态栏和两条分割线的实际高度自动计算。

// 主窗口四角直角切口边长，按 96 DPI 下的逻辑像素定义；设为 0 可关闭。
pub const WINDOW_CORNER_CUT_SIZE: f32 = 5.0;
// 主窗口左右侧边小切口宽度，按 96 DPI 下的逻辑像素定义；设为 0 可关闭。
pub const WINDOW_SIDE_CUT_WIDTH: f32 = 5.0;
// 主窗口左右侧边小切口高度，按 96 DPI 下的逻辑像素定义；设为 0 可关闭。
pub const WINDOW_SIDE_CUT_HEIGHT: f32 = 2.0;
// 左右侧边小切口顶边位置自动派生：小切口中线始终与标题栏下方第一条分割线中线对齐。
// 因此以后调整标题栏高度、分割线高度或切口大小时，不需要再手工同步这个位置。
pub const WINDOW_SIDE_CUT_OFFSET_Y: f32 =
    HEADER_HEIGHT + DIVIDER_HEIGHT / 2.0 - WINDOW_SIDE_CUT_HEIGHT / 2.0;

// 每次启动时，窗口距离当前主屏幕工作区右边缘的距离。
pub const WINDOW_START_MARGIN_RIGHT: f32 = 15.0;
// 每次启动时，窗口距离当前主屏幕工作区底边缘（任务栏上沿）的距离。
pub const WINDOW_START_MARGIN_BOTTOM: f32 = 15.0;

// 已启动客户端右下角状态灯的脉冲周期，对应 CSS animation: 1.2s infinite。
pub const CLIENT_STATUS_DOT_PULSE_PERIOD: f32 = 1.2;
// 客户端卡片状态灯脉冲向外扩散的最大半径增量。
pub const CLIENT_STATUS_DOT_PULSE_RADIUS: f32 = 10.0;
// 接口输入框小灯的脉冲最大直径比原值增加 12px（半径增加 6px）。
pub const INPUT_CONNECTION_DOT_PULSE_RADIUS: f32 = 16.0;

// 主窗口整体背景色，使用 rgba(48 48 48 / 100%)；外形由 Win32 直角切口区域裁剪。
pub const WINDOW_BACKGROUND: [u8; 4] = [48, 48, 48, 255];
// 这里不增加背景圆角或边框参数，避免再次引入锯齿与系统边线。

// ---------------------------------------------------------------------------
// 其他组件背景颜色（不属于主窗口或单张客户端卡片背景）
// ---------------------------------------------------------------------------

// 区块 1 输入框和右侧操作按钮背景，对应 CSS：rgb(0 0 0 / 33%)。
pub const INPUT_SURFACE_BG_RED: u8 = 32;
pub const INPUT_SURFACE_BG_GREEN: u8 = 32;
pub const INPUT_SURFACE_BG_BLUE: u8 = 32;
pub const INPUT_SURFACE_BG_ALPHA: u8 = 255;

// 区块 2 客户端总面板、模型列表选框和状态栏日志按钮背景、“安装客户端”与“启动/重启 • 连接”禁用背景色，对应 CSS：rgb(255 255 255 / 5%)。
pub const SECONDARY_SURFACE_BG_RED: u8 = 59;
pub const SECONDARY_SURFACE_BG_GREEN: u8 = 59;
pub const SECONDARY_SURFACE_BG_BLUE: u8 = 59;
pub const SECONDARY_SURFACE_BG_ALPHA: u8 = 255;
// ---------------------------------------------------------------------------
// 顶部标题栏
// ---------------------------------------------------------------------------

// 顶部标题栏上右下左内边距。
pub const HEADER_PADDING_TOP: f32 = 11.0;
pub const HEADER_PADDING_RIGHT: f32 = 15.0;
pub const HEADER_PADDING_BOTTOM: f32 = 12.0;
pub const HEADER_PADDING_LEFT: f32 = 13.0;
// 标题栏 Logo 宽高。
pub const HEADER_LOGO_WIDTH: f32 = 32.0;
pub const HEADER_LOGO_HEIGHT: f32 = 32.0;
// Logo 相对标题栏内容区垂直中心的偏移；正数向下，负数向上。
pub const HEADER_LOGO_OFFSET_Y: f32 = 1.5;
// Logo 与右侧标题文字之间的横向距离。
pub const HEADER_LOGO_TITLE_GAP: f32 = 8.0;
// 主标题字号。
pub const HEADER_TITLE_MAIN_FONT_SIZE: f32 = 15.0;
// 主标题行盒高度。
pub const HEADER_TITLE_MAIN_LINE_HEIGHT: f32 = 19.0;
// 主标题行盒与副标题行盒之间的垂直距离。
pub const HEADER_TITLE_LINES_GAP: f32 = 1.0;
// 主副标题文字块相对标题栏内容区垂直中心的偏移；正数向下，负数向上。
pub const HEADER_TITLE_BLOCK_OFFSET_Y: f32 = -0.0;
// 副标题行盒高度。
pub const HEADER_TITLE_SUBTITLE_LINE_HEIGHT: f32 = 14.0;
// 副标题右侧检查更新、GitHub、Star、Help 链接图标的宽高。
pub const HEADER_SUBTITLE_ICON_SIZE: f32 = 12.0;
// 副标题链接图标相对副标题文字垂直中心的偏移；正数向下，负数向上。
pub const HEADER_SUBTITLE_ICON_OFFSET_Y: f32 = 0.7;
// 标题栏右侧红、黄、绿窗口控制圆点的直径。
pub const HEADER_TRAFFIC_DOT_SIZE: f32 = 17.0;
// 标题栏右侧窗口控制圆点之间的间距。
pub const HEADER_TRAFFIC_DOT_GAP: f32 = 7.0;
// 分割线高度。
pub const DIVIDER_HEIGHT: f32 = 1.0;
// 分割线左右侧保留的空白距离。
pub const DIVIDER_PADDING_LEFT: f32 = 20.0;
pub const DIVIDER_PADDING_RIGHT: f32 = 20.0;

// ---------------------------------------------------------------------------
// 中间功能栏整体
// ---------------------------------------------------------------------------

// 中间功能栏上右下左内边距。
pub const CONTENT_PADDING_TOP: f32 = 12.0;
pub const CONTENT_PADDING_RIGHT: f32 = 15.0;
pub const CONTENT_PADDING_BOTTOM: f32 = 12.0;
pub const CONTENT_PADDING_LEFT: f32 = 15.0;

// ---------------------------------------------------------------------------
// 区块 1：两个输入框 + 两个操作按钮
// ---------------------------------------------------------------------------

// 区块 1 中，第一行输入框与第二行输入框之间的竖向距离。
pub const INPUT_ROWS_VERTICAL_GAP: f32 = 10.0;
// 区块 1 中，输入框与右侧操作按钮之间的横向距离。
pub const INPUT_ACTION_HORIZONTAL_GAP: f32 = 10.0;
// 输入框和右侧操作按钮所在行的高度。
pub const INPUT_ROW_HEIGHT: f32 = 34.0;
// 区块 1 中，右侧操作按钮的宽度和高度。
pub const INPUT_ACTION_SIZE: f32 = 34.0;
// 四个操作按钮共用的动态 SVG 尺寸。
pub const ACTION_SPINNER_SIZE: f32 = 20.0;
// 接口输入框已连接绿灯：灯芯直径及其占位宽度。
pub const INPUT_CONNECTION_DOT_SIZE: f32 = 6.0;
pub const INPUT_CONNECTION_DOT_SPACE: f32 = 14.0;

// 输入框为空时默认提示文字颜色，对应 CSS：rgb(109 115 128 / 70%)。
pub const INPUT_HINT_TEXT_RED: u8 = 109;
pub const INPUT_HINT_TEXT_GREEN: u8 = 115;
pub const INPUT_HINT_TEXT_BLUE: u8 = 128;
pub const INPUT_HINT_TEXT_ALPHA: u8 = 179;

// 输入框与提供商记录共用圆角；列表宽度和左右位置跟随地址输入框。
pub const HISTORY_HEIGHT: f32 = 240.0;
pub const INPUT_CORNER_RADIUS: u8 = 10;
pub const HISTORY_PADDING: i8 = 10;
pub const HISTORY_VERTICAL_GAP: f32 = 10.0;
pub const HISTORY_ITEM_HEIGHT: f32 = 34.0;
pub const HISTORY_ITEM_VERTICAL_GAP: f32 = 5.0;
pub const HISTORY_ITEM_CORNER_RADIUS: u8 = 10;

// 确认重命名后延迟恢复删除按钮，避免连续点击误删。
pub const HISTORY_DELETE_DELAY_MS: u64 = 300;
// 提供商记录底部提供商快捷按钮，横向间距按可用宽度均分。
pub const PROVIDER_BUTTON_SIZE: f32 = 34.0;
pub const PROVIDER_ICON_SIZE: f32 = 20.0;
pub const PROVIDER_CORNER_RADIUS: u8 = 12;
// 预设卡片背景透明；描边沿用历史配置下拉菜单的 1px 内描边。
pub const PROVIDER_BACKGROUND: [u8; 3] = [0, 0, 0];
pub const PROVIDER_BACKGROUND_ALPHA: u8 = 0;
// Hover outline for provider preset cards: rgb(20 126 255 / 33%).
pub const PROVIDER_HOVER_OUTLINE: [u8; 4] = [20, 126, 255, 84];
pub const PROVIDER_TOP_GAP: f32 = 5.0;
// 提供商预设图标采用固定间距；每行最多 7 个，超出后换行。
pub const PROVIDER_MAX_COLUMNS: usize = 7;
pub const PROVIDER_BUTTON_GAP: f32 = 8.0;
pub const PROVIDER_ROW_GAP: f32 = 8.0;
pub const PROVIDER_TITLE_FONT_SIZE: f32 = 11.0;
pub const PROVIDER_TITLE_HEIGHT: f32 = 16.0;
pub const PROVIDER_TITLE_LINE_COLOR: [u8; 4] = [22, 22, 22, 255];
// 两个提供商标题两侧的固定线段长度。
pub const PROVIDER_RECORDS_TITLE_LINE_LENGTH: f32 = 52.0;
pub const PROVIDER_PRESET_TITLE_LINE_LENGTH: f32 = 52.0;
// 两个标题各自独立控制标题与下面内容之间的距离。
pub const PROVIDER_RECORDS_TITLE_BOTTOM_GAP: f32 = 5.0;
pub const PROVIDER_PRESET_TITLE_BOTTOM_GAP: f32 = 5.0;

// 输入框右侧展开/显示隐藏按钮的悬停背景。
pub const INPUT_INLINE_BUTTON_SIZE: f32 = 26.0;
pub const INPUT_INLINE_BUTTON_CORNER_RADIUS: u8 = 9;
pub const INPUT_INLINE_BUTTON_HOVER_RED: u8 = 48;
pub const INPUT_INLINE_BUTTON_HOVER_GREEN: u8 = 48;
pub const INPUT_INLINE_BUTTON_HOVER_BLUE: u8 = 48;

// 提供商记录列表背景与选项悬停/选中背景。
pub const HISTORY_BG_RED: u8 = 48;
pub const HISTORY_BG_GREEN: u8 = 48;
pub const HISTORY_BG_BLUE: u8 = 48;
pub const HISTORY_BG_ALPHA: u8 = 255;
// 提供商记录列表选项默认背景。
pub const HISTORY_ITEM_BG_RED: u8 = 32;
pub const HISTORY_ITEM_BG_GREEN: u8 = 32;
pub const HISTORY_ITEM_BG_BLUE: u8 = 32;
pub const HISTORY_ITEM_BG_ALPHA: u8 = 255;
// 提供商记录列表选项悬停/选中背景/文字颜色。
pub const HISTORY_ITEM_ACTIVE: [u8; 4] = [16, 38, 77, 255];
pub const HISTORY_ITEM_ACTIVE_TEXT: [u8; 3] = [71, 160, 255];
// 提供商记录列表选项重命名时背景/外边线。
pub const HISTORY_EDIT_BG: u8 = 12;
pub const HISTORY_OUTLINE_ALPHA: u8 = 26; // 白色 10% 的 1px 外描边

// 提供商记录选项左侧 URL 图标与输入框左侧图标保持相同的几何关系。
pub const HISTORY_ITEM_ICON_SIZE: f32 = 16.0;
pub const HISTORY_ITEM_ICON_LEFT: f32 = 6.0;
pub const HISTORY_ITEM_ICON_TEXT_GAP: f32 = 4.0;
// 编辑与删除按钮。
pub const HISTORY_ITEM_DELETE_WIDTH: f32 = 20.0;
pub const HISTORY_DELETE_HOVER_COLOR: [u8; 3] = [255, 95, 87];
pub const HISTORY_ITEM_EDIT_WIDTH: f32 = 20.0;
// 灯芯右边距与上下边距一致（行高 34，灯直径 11）。
pub const HISTORY_ITEM_DOT_RIGHT: f32 = HISTORY_ITEM_HEIGHT / 2.0;
// 编辑/删除/连接灯图标可见边缘之间的统一间距。
pub const HISTORY_ITEM_ACTION_GAP: f32 = 8.0;
// 连接灯与左侧删除图标之间间距
pub const HISTORY_ITEM_DOT_ACTION_GAP: f32 = 7.0;
// 提供商记录列表与拉取模型菜单共用位置、尺寸和外观。
// 所有区域使用同一浮动滚动条，不占内容宽度。
pub const SCROLLBAR_IDLE_WIDTH: f32 = 2.0;
pub const SCROLLBAR_HOVER_WIDTH: f32 = 6.0;
pub const SCROLLBAR_GUTTER: i8 = 6;
pub const MENU_ANIMATION_SECONDS: f32 = 0.16;

// 提供商记录卡片悬停统计提示：接口、Key、请求、Tokens、缓存。
// 统计浮层独立延时，避免鼠标经过相邻配置时立即遮挡列表。
pub const HISTORY_USAGE_DELAY_SECONDS: f32 = 0.88;
pub const HISTORY_USAGE_ROW_HEIGHT: f32 = 18.0;
pub const HISTORY_USAGE_EXTRA_PADDING_Y: i8 = 4;
pub const HISTORY_USAGE_CORNER_RADIUS: u8 = 10;
pub const HISTORY_USAGE_ROW_GAP: f32 = 4.0;
pub const HISTORY_USAGE_ICON_SIZE: f32 = 13.0;
pub const HISTORY_USAGE_ICON_TEXT_GAP: f32 = 4.0;
pub const HISTORY_USAGE_FONT_SIZE: f32 = 13.0;
pub const HISTORY_USAGE_STAT_FONT_SIZE: f32 = 10.0;
pub const HISTORY_USAGE_BADGE_FONT_SIZE: f32 = 10.0;
pub const HISTORY_USAGE_BADGE_RADIUS: u8 = 5;
pub const HISTORY_USAGE_BADGE_PADDING_X: f32 = 5.0;
pub const HISTORY_USAGE_BADGE_PADDING_Y: f32 = 3.0;
pub const HISTORY_USAGE_BG: [u8; 4] = [0, 5, 33, 245];
pub const HISTORY_USAGE_BLUE: [u8; 4] = [71, 160, 255, 51];
pub const HISTORY_USAGE_GREEN: [u8; 4] = [0, 210, 106, 38];
pub const HISTORY_USAGE_ORANGE: [u8; 4] = [255, 87, 34, 51];

// 第一行“测试连接”按钮内部图标的宽度。
pub const CONNECT_ACTION_ICON_WIDTH: f32 = 16.0;
// 第一行“测试连接”按钮内部图标的高度。
pub const CONNECT_ACTION_ICON_HEIGHT: f32 = 16.0;
// 第二行“拉取模型”按钮内部图标的宽度。
pub const MODEL_FETCH_ACTION_ICON_WIDTH: f32 = 16.0;
// 第二行“拉取模型”按钮内部图标的高度。
pub const MODEL_FETCH_ACTION_ICON_HEIGHT: f32 = 16.0;
// 模型菜单右侧复制按钮与勾选图标的布局。
pub const MODEL_MENU_ACTION_SIZE: f32 = 20.0;
pub const MODEL_MENU_ACTION_ICON_SIZE: f32 = 16.0;
pub const MODEL_MENU_CHECK_ICON_SIZE: f32 = 13.0;
pub const MODEL_MENU_ACTION_GAP: f32 = 4.0;
pub const MODEL_MENU_COPY_OFFSET_X: f32 = 2.0;

// ---------------------------------------------------------------------------
// 区块 1 与区块 2 之间光点流动
// ---------------------------------------------------------------------------

// 区块 1 与区块 2 之间水平居中的级联流 GUI 绘制宽度。
pub const CASCADE_FLOW_WIDTH: f32 = 260.0;
// 区块 1 与区块 2 之间水平居中的级联流 GUI 绘制高度。
pub const CASCADE_FLOW_HEIGHT: f32 = 36.0;
// 级联流 GUI 绘制区域与区块 2 之间的附加竖向距离。
pub const BLOCKS_VERTICAL_GAP: f32 = 0.0;

// 已连接配置串联线的光点数量和单程时间；不影响下方四条级联曲线。
pub const INPUT_FLOW_DOT_COUNT: usize = 8;
pub const INPUT_FLOW_DURATION_SECONDS: f32 = 3.2;
// 上方串联线与下方四条分支线的底线颜色：rgb(22 22 22 / 100%)。
pub const FLOW_LINE_COLOR: [u8; 4] = [25, 25, 25, 255];
// 光点已激活与未激活的颜色；动画淡入淡出在此基础上调整透明度。
pub const FLOW_DOT_ACTIVE_COLOR: [u8; 4] = [0, 221, 112, 255];
pub const FLOW_DOT_INACTIVE_COLOR: [u8; 4] = [0, 0, 0, 255];
// ---------------------------------------------------------------------------
// 区块 2-1：客户端卡片整体
// ---------------------------------------------------------------------------

// 单张客户端卡片宽度。卡片背景样式和圆角由 SVG 负责。
pub const CLIENT_CARD_WIDTH: f32 = 71.0;
// 单张客户端卡片高度。卡片背景样式和圆角由 SVG 负责。
pub const CLIENT_CARD_HEIGHT: f32 = 71.0;
// 以下参数只负责卡片区的排列、间距和选中缺口，不参与单张卡片背景绘制。
// 客户端卡片整体面板的内边距。
pub const CLIENT_PANEL_PADDING: f32 = 15.0;
// 客户端卡片整体面板的圆角半径。
pub const CLIENT_PANEL_CORNER_RADIUS: f32 = 30.0;
// 四个客户端卡片之间的横向距离。
pub const CLIENT_CARD_HORIZONTAL_GAP: f32 = 12.0;
// 被选中客户端卡片在区块 2-1 底部显示的缺口宽度。
pub const CLIENT_SELECTED_NOTCH_WIDTH: f32 = 16.0;
// 被选中客户端卡片在区块 2-1 底部显示的缺口高度。
pub const CLIENT_SELECTED_NOTCH_HEIGHT: f32 = 4.0;

// ---------------------------------------------------------------------------
// 区块 2-2：模型列表选择 + 启动按钮
// ---------------------------------------------------------------------------

// 区块 2-1 客户端卡片整体与区块 2-2 操作行之间的竖向距离。
pub const CLIENT_CARDS_ACTIONS_VERTICAL_GAP: f32 = 12.0;
// 区块 2-2 中，模型列表选择按钮之间的横向距离。
pub const MODEL_CHIP_HORIZONTAL_GAP: f32 = 10.0;
// 区块 2-2 中，模型列表选择按钮的高度。
pub const MODEL_CHIP_HEIGHT: f32 = 34.0;
// 模型列表选择按钮的左内边距。
pub const MODEL_CHIP_PADDING_LEFT: f32 = 8.0;
// 模型列表选择按钮的右内边距。
pub const MODEL_CHIP_PADDING_RIGHT: f32 = 8.0;
// 模型列表选择按钮前方勾选区域的宽度。
pub const MODEL_CHECK_WIDTH: f32 = 16.0;
// 模型列表选择按钮前方勾选区域的高度。
pub const MODEL_CHECK_HEIGHT: f32 = 16.0;
// 前方勾选区域与右侧模型列表文字之间的横向距离。
pub const MODEL_CHECK_TEXT_GAP: f32 = 6.0;
// 区块 2-2 中，启动按钮的宽度和高度。
pub const MODEL_INSTALL_SIZE: f32 = 34.0;
// 安装按钮与启动按钮之间的距离。
pub const MODEL_INSTALL_LAUNCH_GAP: f32 = 10.0;
// 安装图标的宽度和高度。
pub const MODEL_INSTALL_ICON_WIDTH: f32 = 16.0;
pub const MODEL_INSTALL_ICON_HEIGHT: f32 = 16.0;
// 安装和启动按钮采用相同的圆角半径。
pub const MODEL_ACTION_BUTTON_CORNER_RADIUS: f32 = 12.0;

pub const MODEL_LAUNCH_SIZE: f32 = 34.0;
// 启动按钮内部图标的宽高。
pub const MODEL_LAUNCH_ICON_WIDTH: f32 = 16.0;
pub const MODEL_LAUNCH_ICON_HEIGHT: f32 = 16.0;

// ---------------------------------------------------------------------------
// 底部状态栏
// ---------------------------------------------------------------------------

// 底部状态栏上右下左内边距。
pub const STATUS_PADDING_TOP: f32 = 6.0;
pub const STATUS_PADDING_RIGHT: f32 = 15.0;
pub const STATUS_PADDING_BOTTOM: f32 = 10.0;
pub const STATUS_PADDING_LEFT: f32 = 15.0;
// 状态栏左侧图标与提示文字之间的横向距离。
pub const STATUS_ICON_TEXT_HORIZONTAL_GAP: f32 = 8.0;
// 状态栏左侧状态图标的宽高。
pub const STATUS_ICON_SIZE: f32 = 16.0;
// 状态栏提供商图标。
pub const STATUS_ENDPOINT_ICON_SIZE: f32 = 16.0;
pub const STATUS_ENDPOINT_ICON_GAP: f32 = 3.0;
// 状态栏右侧日志按钮的宽度和高度。
pub const STATUS_LOG_BUTTON_SIZE: f32 = 34.0;

// 日志按钮悬停菜单。菜单只在日志按钮或菜单本身处于悬停过渡期时显示。
pub const LOG_MENU_WIDTH: f32 = 90.0;
pub const LOG_MENU_HEIGHT: f32 = 72.0;
pub const LOG_MENU_PADDING: f32 = 6.0;
pub const LOG_MENU_CORNER_RADIUS: u8 = 12;
pub const LOG_MENU_BG: [u8; 3] = [48, 48, 48];
// 日志按钮悬停菜单里的按钮
pub const LOG_MENU_BUTTON_HEIGHT: f32 = 28.0;
pub const LOG_MENU_BUTTON_RADIUS: u8 = 8;
pub const LOG_MENU_BUTTON_GAP: f32 = 4.0;
pub const LOG_MENU_BUTTON_BG: [u8; 3] = [59, 59, 59];
pub const LOG_MENU_BUTTON_FONT_SIZE: f32 = 11.0;

// 日志浮动窗口的固定高度。
pub const LOG_WINDOW_HEIGHT: f32 = 317.0;
// 日志浮动窗口与主窗口左侧、右侧和下侧之间的距离。
pub const LOG_WINDOW_MARGIN: f32 = 10.0;
// 日志浮动窗口标题文字字号。
pub const LOG_WINDOW_TITLE_FONT_SIZE: f32 = HEADER_TITLE_MAIN_FONT_SIZE;
// 日志窗口分别绘制标题栏和内容栏背景。
pub const LOG_HEADER_BG: [u8; 3] = [48, 48, 48];
pub const LOG_CONTENT_BG: [u8; 3] = [2, 7, 33];
pub const LOG_CONTENT_TEXT: [u8; 3] = [200, 206, 216];
pub const LOG_HEADER_HEIGHT: f32 = INPUT_INLINE_BUTTON_SIZE;
pub const LOG_HEADER_PADDING_BOTTOM: f32 = 10.0;
pub const LOG_CLOSE_BUTTON_SIZE: f32 = 24.0;
pub const LOG_CLOSE_BUTTON_RADIUS: u8 = 9;
pub const LOG_CLOSE_ICON_SIZE: f32 = 7.0;
pub const LOG_CLOSE_ICON_STROKE: f32 = 2.0;
pub const LOG_CLOSE_HOVER_BG: [u8; 3] = [201, 0, 0];
pub const LOG_CLOSE_BG: [u8; 3] = [60, 60, 60];
pub const LOG_WINDOW_CORNER_RADIUS: u8 = 14;
// 日志正文的行高，略高于默认字体行高以便区分相邻记录。
pub const LOG_CONTENT_LINE_HEIGHT: f32 = 16.0;
pub const MODEL_LOG_EVEN_ROW_COLOR: [u8; 3] = [112, 211, 255];

// 鼠标悬停提示首次出现前的等待时间（秒）。
pub const TOOLTIP_DELAY_SECONDS: f32 = 0.12;
// 鼠标悬停提示与目标元素之间的距离。
pub const TOOLTIP_GAP: f32 = 6.0;
// 鼠标悬停提示底部居中倒三角箭头的宽度。
pub const TOOLTIP_ARROW_WIDTH: f32 = 10.0;
// 鼠标悬停提示底部居中倒三角箭头的高度。
pub const TOOLTIP_ARROW_HEIGHT: f32 = 5.0;
// 鼠标悬停提示的圆角半径。
pub const TOOLTIP_CORNER_RADIUS: u8 = 9;
// 鼠标悬停提示背景透明度（0 完全透明，255 完全不透明）。
pub const TOOLTIP_BACKGROUND_ALPHA: u8 = 220;
// 鼠标悬停提示内容区的最小高度，用于让文字垂直居中。
pub const TOOLTIP_CONTENT_MIN_HEIGHT: f32 = 18.0;
// 鼠标悬停提示内部的水平与竖直边距。
pub const TOOLTIP_PADDING_HORIZONTAL: i8 = 8;
pub const TOOLTIP_PADDING_VERTICAL: i8 = 5;

// ============================================================================
// 派生布局参数区：通常不需要修改
// ============================================================================

// 标题文字块高度由两行文字与行距自动相加。
pub const HEADER_TITLE_BLOCK_HEIGHT: f32 =
    HEADER_TITLE_MAIN_LINE_HEIGHT + HEADER_TITLE_LINES_GAP + HEADER_TITLE_SUBTITLE_LINE_HEIGHT;

// 标题栏内容高度取 Logo、标题文字块和窗口控制圆点中的最大值，不再手工填写固定高度。
pub const HEADER_CONTENT_HEIGHT: f32 = if HEADER_LOGO_HEIGHT > HEADER_TITLE_BLOCK_HEIGHT {
    if HEADER_LOGO_HEIGHT > HEADER_TRAFFIC_DOT_SIZE {
        HEADER_LOGO_HEIGHT
    } else {
        HEADER_TRAFFIC_DOT_SIZE
    }
} else if HEADER_TITLE_BLOCK_HEIGHT > HEADER_TRAFFIC_DOT_SIZE {
    HEADER_TITLE_BLOCK_HEIGHT
} else {
    HEADER_TRAFFIC_DOT_SIZE
};

// 顶部标题栏总高度 = 上内边距 + 自动计算的标题内容高度 + 下内边距。
pub const HEADER_HEIGHT: f32 = HEADER_PADDING_TOP + HEADER_CONTENT_HEIGHT + HEADER_PADDING_BOTTOM;

// 状态栏内容高度取状态图标和日志按钮中的最大值，不再手工填写固定高度。
pub const STATUS_CONTENT_HEIGHT: f32 = if STATUS_ICON_SIZE > STATUS_LOG_BUTTON_SIZE {
    STATUS_ICON_SIZE
} else {
    STATUS_LOG_BUTTON_SIZE
};

// 底部状态栏总高度 = 上内边距 + 自动计算的状态内容高度 + 下内边距。
pub const STATUS_HEIGHT: f32 = STATUS_PADDING_TOP + STATUS_CONTENT_HEIGHT + STATUS_PADDING_BOTTOM;

// 区块 1 总高度 = 两行输入框 + 两行之间的竖向距离。
pub const INPUT_BLOCK_HEIGHT: f32 = INPUT_ROW_HEIGHT * 2.0 + INPUT_ROWS_VERTICAL_GAP;

// 区块 2-1 总高度 = 客户端卡片高度 + 面板上下内边距。
pub const CLIENT_CARDS_BLOCK_HEIGHT: f32 = CLIENT_CARD_HEIGHT + CLIENT_PANEL_PADDING * 2.0;

// 区块 2-2 总高度取模型列表按钮和启动按钮中较高的一项，确保全部垂直居中。
pub const MODEL_ACTION_BUTTON_HEIGHT: f32 = if MODEL_INSTALL_SIZE > MODEL_LAUNCH_SIZE {
    MODEL_INSTALL_SIZE
} else {
    MODEL_LAUNCH_SIZE
};

pub const MODEL_ACTION_ROW_HEIGHT: f32 = if MODEL_CHIP_HEIGHT > MODEL_ACTION_BUTTON_HEIGHT {
    MODEL_CHIP_HEIGHT
} else {
    MODEL_ACTION_BUTTON_HEIGHT
};

// 区块 2 总高度 = 区块 2-1 + 两个子区块之间的距离 + 区块 2-2。
pub const CLIENT_BLOCK_HEIGHT: f32 =
    CLIENT_CARDS_BLOCK_HEIGHT + CLIENT_CARDS_ACTIONS_VERTICAL_GAP + MODEL_ACTION_ROW_HEIGHT;

// 中间功能栏总高度完全由内部内容决定：
// 上内边距 + 区块 1 + 级联流 SVG + 附加区块间距 + 区块 2 + 下内边距。
pub const CONTENT_HEIGHT: f32 = CONTENT_PADDING_TOP
    + INPUT_BLOCK_HEIGHT
    + CASCADE_FLOW_HEIGHT
    + BLOCKS_VERTICAL_GAP
    + CLIENT_BLOCK_HEIGHT
    + CONTENT_PADDING_BOTTOM;

// 窗口总高度自动适应全部内容：
// 标题栏 + 第一条分割线 + 中间功能栏 + 第二条分割线 + 状态栏。
pub const WINDOW_HEIGHT: f32 =
    HEADER_HEIGHT + DIVIDER_HEIGHT + CONTENT_HEIGHT + DIVIDER_HEIGHT + STATUS_HEIGHT;
