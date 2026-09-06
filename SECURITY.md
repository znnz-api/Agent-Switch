# Security Policy

Please report security issues privately through the official znnz.net contact. Do not post API Keys, authenticated requests, user configuration, or complete logs in public Issues.

Include the affected version, reproduction steps, and sanitized error information.

The launcher encrypts remembered API Keys with Windows DPAPI and does not pass Keys through command-line arguments. The Claude Desktop local proxy listens only on the loopback address. Windows installers are signature-checked before execution.

## 中文

安全问题请通过 znnz.net 官方联系方式私下报告，不要在公开 Issue 中提交 API Key、鉴权请求、用户配置或完整日志。

报告请包含受影响版本、复现步骤和已脱敏的错误信息。

启动器使用 Windows DPAPI 加密记住的 API Key，不通过命令行传递 Key；Claude Desktop 的本地代理只监听回环地址。下载的 Windows 安装包在运行前进行签名校验。
