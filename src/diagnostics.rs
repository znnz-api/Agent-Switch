use crate::{catalog, config, config::ClientPaths, platform};
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;
use toml_edit::{DocumentMut, Item};

pub async fn run(paths: &ClientPaths, gateway_url: &str, online: bool) -> Result<()> {
    println!("znnz-client 只读诊断\n");
    println!("用户目录:          {}", paths.user_home.display());
    println!("Codex home:        {}", paths.codex_home.display());
    println!("Claude home:       {}", paths.claude_home.display());

    diagnose_codex(paths);
    diagnose_claude(paths);

    let running = platform::running_codex_processes()
        .unwrap_or_else(|error| vec![format!("进程检查失败: {error}")]);
    if running.is_empty() {
        println!("运行中的 Codex:    未检测到");
    } else {
        println!("运行中的 Codex:    {}", running.join("；"));
    }

    if online {
        println!("\n在线检查:");
        match diagnostic_key(paths) {
            Ok(Some(key)) => {
                println!("  API Key:         {}", config::key_fingerprint(&key));
                match catalog::fetch_catalog(gateway_url, &key, "0.145.0").await {
                    Ok(value) => println!(
                        "  网关模型接口:    正常，{} 个可见模型",
                        catalog::visible_slugs(&value).len()
                    ),
                    Err(error) => println!("  网关模型接口:    失败: {error:#}"),
                }
            }
            Ok(None) => println!("  网关模型接口:    跳过，没有找到可用 API Key"),
            Err(error) => println!("  网关模型接口:    跳过，读取 Key 失败: {error:#}"),
        }
    }

    println!("\n诊断全程未修改任何文件，也未输出完整 API Key。");
    Ok(())
}

fn diagnose_codex(paths: &ClientPaths) {
    println!("\nCodex:");
    match read_toml(&paths.codex_config) {
        Ok(Some(doc)) => {
            let provider = doc
                .get("model_provider")
                .and_then(Item::as_str)
                .unwrap_or("<未设置>");
            let model = doc
                .get("model")
                .and_then(Item::as_str)
                .unwrap_or("<未设置>");
            let catalog_path = doc
                .get("model_catalog_json")
                .and_then(Item::as_str)
                .unwrap_or("<未设置>");
            println!("  config.toml:     有效");
            println!("  Provider:        {provider}");
            println!("  当前模型:        {model}");
            println!("  模型目录配置:    {catalog_path}");
        }
        Ok(None) => println!("  config.toml:     不存在"),
        Err(error) => println!("  config.toml:     无效: {error:#}"),
    }

    match catalog::read_catalog(&paths.codex_catalog) {
        Ok(value) => println!(
            "  网关模型目录:    有效，{} 个可见模型",
            catalog::visible_slugs(&value).len()
        ),
        Err(_error) if !paths.codex_catalog.exists() => println!("  网关模型目录:    不存在"),
        Err(error) => println!("  网关模型目录:    无效: {error:#}"),
    }

    match config::read_codex_api_key(&paths.codex_auth) {
        Ok(Some(key)) => println!("  auth.json Key:   {}", config::key_fingerprint(&key)),
        Ok(None) => println!("  auth.json Key:   未设置"),
        Err(error) => println!("  auth.json Key:   读取失败: {error:#}"),
    }

    match read_json(&paths.codex_global_state) {
        Ok(Some(value)) => {
            let advanced = value
                .pointer("/electron-persisted-atom-state/composer-model-picker-menu-view-v1")
                .and_then(Value::as_str);
            println!(
                "  Desktop 高级菜单: {}",
                if advanced == Some("advanced") {
                    "已启用"
                } else {
                    "未启用"
                }
            );
        }
        Ok(None) => println!("  Desktop 全局状态: 不存在"),
        Err(error) => println!("  Desktop 全局状态: 无效: {error:#}"),
    }

    println!(
        "  历史会话文件:    {}",
        count_files_recursive(&paths.codex_sessions).unwrap_or(0)
    );
    println!(
        "  state_*.sqlite:  {}",
        count_state_databases(&paths.codex_home).unwrap_or(0)
    );
    println!(
        "  installation_id: {}",
        if paths.codex_home.join("installation_id").is_file() {
            "存在"
        } else {
            "不存在"
        }
    );
}

fn diagnose_claude(paths: &ClientPaths) {
    println!("\nClaude Code:");
    match read_json(&paths.claude_settings) {
        Ok(Some(value)) => {
            let base = value
                .pointer("/env/ANTHROPIC_BASE_URL")
                .and_then(Value::as_str)
                .unwrap_or("<未设置>");
            let discovery = value
                .pointer("/env/CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY")
                .and_then(Value::as_str);
            println!("  settings.json:   有效");
            println!("  网关地址:        {base}");
            println!(
                "  模型发现:        {}",
                if discovery == Some("1") {
                    "已启用"
                } else {
                    "未启用"
                }
            );
        }
        Ok(None) => println!("  settings.json:   不存在"),
        Err(error) => println!("  settings.json:   无效: {error:#}"),
    }
    match read_json(&paths.claude_state) {
        Ok(Some(value)) => println!(
            "  初始化引导:      {}",
            if value.get("hasCompletedOnboarding").and_then(Value::as_bool) == Some(true) {
                "已跳过"
            } else {
                "未跳过"
            }
        ),
        Ok(None) => println!("  Claude 状态:     不存在"),
        Err(error) => println!("  Claude 状态:     无效: {error:#}"),
    }
}

fn diagnostic_key(paths: &ClientPaths) -> Result<Option<String>> {
    for name in ["ZNNZ_API_KEY", "LITEAPI_API_KEY", "ANTHROPIC_AUTH_TOKEN"] {
        if let Ok(value) = std::env::var(name)
            && !value.trim().is_empty()
        {
            return Ok(Some(value));
        }
    }
    config::read_codex_api_key(&paths.codex_auth)
}

fn read_toml(path: &Path) -> Result<Option<DocumentMut>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).with_context(|| format!("无法读取 {}", path.display()))?;
    Ok(Some(raw.parse::<DocumentMut>().with_context(|| {
        format!("{} 不是有效 TOML", path.display())
    })?))
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).with_context(|| format!("无法读取 {}", path.display()))?;
    Ok(Some(serde_json::from_str(&raw).with_context(|| {
        format!("{} 不是有效 JSON", path.display())
    })?))
}

fn count_files_recursive(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in
            fs::read_dir(&directory).with_context(|| format!("无法读取 {}", directory.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.is_file() {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn count_state_databases(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    Ok(fs::read_dir(root)?
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("state_") && name.ends_with(".sqlite"))
        .count())
}
