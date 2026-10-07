# Contributing

Issues and pull requests are welcome.

Please keep the project simple:

- Never record or commit API Keys.
- Do not break users' existing Codex or Claude settings and sessions.

Run these checks before submitting:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
node --test tests/renderer-inject.test.js
```

When changing configuration, registry, installers, or authentication, include relevant tests.

## 中文说明

欢迎提交 Issue 和 Pull Request。

请保持项目简洁：

- 不记录或提交 API Key
- 不破坏用户现有的 Codex、Claude 配置和会话

提交前请运行上面的检查命令。修改配置、注册表、安装器或鉴权功能时，请同时补充相关测试。
