# Privacy

znnz.net Agent Launcher contains no telemetry, advertising, or behavioral analytics.

The launcher only connects to the following services when the related action is requested:

- The AI gateway entered by the user
- Client installation sources
- The official npm registry or npmmirror

The endpoint, UI preferences, logs, and client configuration backups are stored in the current Windows user's local data directory. A remembered API Key is encrypted with Windows DPAPI.

When a custom gateway is used, the API Key and model requests are sent to that gateway operator. They are not sent to znnz.net.

## 中文

znnz.net Agent Launcher 不包含遥测、广告或行为分析功能。

程序只在用户执行相应操作时访问：

- 用户填写的 AI 网关
- 客户端安装源
- npm 官方源或 npmmirror

接口地址、界面偏好、日志和客户端配置备份保存在当前 Windows 用户目录。选择记住 API Key 时，使用 Windows DPAPI 加密保存。

使用自定义网关时，API Key 和模型请求会发送给该网关运营者，不会发送给 znnz.net。
