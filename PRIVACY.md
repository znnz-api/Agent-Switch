# Privacy / 隐私说明

## English

Agent-Switch contains no telemetry, advertising, or behavioral analytics.

Configuration, logs, client configuration backups, and usage statistics are stored locally under `%LOCALAPPDATA%\Agent-Switch`. Saved API Keys are encrypted with Windows DPAPI.

In gateway mode, API Keys and model requests are sent to the provider selected by the user. Client installation accesses the relevant download sources or npm registries. The application also fetches the cloud provider preset list and icons; these requests do not upload user configuration or API Keys.

Usage statistics and request metadata are stored in the local SQLite database, `usage.sqlite3`, and are not uploaded to an analytics service. The database does not store plaintext API Keys, prompts, or model replies. Agent-Switch does not estimate billing.

## 中文

Agent-Switch 不包含遥测、广告或行为分析功能。

配置、日志、客户端配置备份和用量统计保存在本机 `%LOCALAPPDATA%\Agent-Switch` 目录。保存的 API Key 使用 Windows DPAPI 加密。

网关模式下，API Key 和模型请求会发送给用户选择的提供商。安装客户端时访问相应下载源或 npm 源。程序还会获取云端提供商预设清单与图标，此过程不会上传用户配置或 API Key。

用量统计与请求元数据保存在本地 SQLite 数据库 `usage.sqlite3`，不上传到统计服务。数据库不保存明文 API Key、请求正文或模型回答，也不估算费用。
