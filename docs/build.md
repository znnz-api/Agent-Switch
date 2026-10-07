# 构建 Agent-Switch / Building Agent-Switch

## 中文

在 Windows 上安装 Rust 稳定版、MSVC C++ 构建工具和 Windows SDK，然后在项目根目录运行：

```powershell
cargo build --locked --release
New-Item -ItemType Directory -Force release | Out-Null
Copy-Item target\release\znnz-agent-launcher.exe release\Agent-Switch-v1.1.exe
```

默认构建为公开版，接口地址初始为空，占位提示为 `https://api-endpoint`。目前 Cargo 产物名称仍为 `znnz-agent-launcher.exe`，复制后使用 Agent-Switch 的发布文件名。

`release/` 用于存放本地发布文件，不参与 Git 提交。需要发布可执行文件时，将其附加到 GitHub Release。

界面布局和样式参数位于 `src/gui_layout.rs`。调整后重新运行上述命令即可。

提交代码前的检查命令见 [CONTRIBUTING.md](../CONTRIBUTING.md)。

## English

Install stable Rust, the MSVC C++ build tools, and the Windows SDK on Windows, then run the commands above from the project root.

The default build is the public edition. Its endpoint starts empty, with `https://api-endpoint` as the placeholder. Cargo currently produces `znnz-agent-launcher.exe`; the copy command gives it the Agent-Switch release filename.

The `release/` directory holds local build artifacts and is excluded from Git. Attach executables to a GitHub Release when publishing a binary.

Layout and style settings are in `src/gui_layout.rs`. Run the same build commands after changing them.

See [CONTRIBUTING.md](../CONTRIBUTING.md) for checks to run before submitting code.
