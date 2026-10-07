//! Track only the configuration fields taken over by Agent-Switch. Account
//! credentials are encrypted in the snapshot and never copied over a newer login.
use crate::{backup, config::ClientPaths, gui_worker::ClientTarget, i18n};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use toml_edit::{DocumentMut, Item};
use zeroize::Zeroizing;

const MARKER: &str = "agent-switch-account-mode.dpapi";

#[derive(Default, Serialize, Deserialize)]
struct Snapshot {
    files: BTreeMap<String, SavedFile>,
}
#[derive(Serialize, Deserialize)]
struct SavedFile {
    before: Option<String>,
    written: String,
}

pub fn paths(target: ClientTarget) -> Result<ClientPaths> {
    if target == ClientTarget::CodexCli {
        ClientPaths::launcher_codex_cli_profile()
    } else {
        ClientPaths::discover(None, None)
    }
}
fn marker(paths: &ClientPaths, target: ClientTarget) -> PathBuf {
    match target {
        ClientTarget::ClaudeCode => paths.claude_home.join(MARKER),
        _ => paths.codex_home.join(MARKER),
    }
}
fn optional_text(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(None),
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context(i18n::tr(
            "无法读取客户端配置",
            "Unable to read client configuration",
        )),
    }
}
fn document(raw: Option<&str>) -> Result<DocumentMut> {
    raw.unwrap_or("").parse().context(i18n::tr(
        "客户端配置不是有效 TOML",
        "Client configuration is not valid TOML",
    ))
}
fn object(raw: Option<&str>) -> Result<Map<String, Value>> {
    let value: Value = serde_json::from_str(raw.unwrap_or("{}")).context(i18n::tr(
        "客户端配置不是有效 JSON",
        "Client configuration is not valid JSON",
    ))?;
    value.as_object().cloned().context(i18n::tr(
        "客户端配置必须是 JSON 对象",
        "Client configuration must be a JSON object",
    ))
}
fn loopback(value: Option<&str>) -> bool {
    value
        .and_then(|value| url::Url::parse(value).ok())
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]"))
}
fn codex_gateway(doc: &DocumentMut) -> bool {
    loopback(doc.get("openai_base_url").and_then(Item::as_str))
        || doc
            .get("model_provider")
            .and_then(Item::as_str)
            .is_some_and(|id| {
                loopback(
                    doc.get("model_providers")
                        .and_then(|item| item.get(id))
                        .and_then(|item| item.get("base_url"))
                        .and_then(Item::as_str),
                )
            })
}
pub fn is_managed(target: ClientTarget) -> Result<bool> {
    if target == ClientTarget::ClaudeDesktop {
        return crate::claude_desktop_registry::is_agent_switch_gateway();
    }
    is_managed_at(&paths(target)?, target)
}
fn is_managed_at(paths: &ClientPaths, target: ClientTarget) -> Result<bool> {
    if marker(paths, target).is_file() {
        return Ok(true);
    }
    if target == ClientTarget::ClaudeCode {
        let settings = object(optional_text(&paths.claude_settings)?.as_deref())?;
        Ok(settings
            .get("env")
            .and_then(|v| v.get("ANTHROPIC_BASE_URL"))
            .and_then(Value::as_str)
            .is_some_and(|url| loopback(Some(url)))
            && settings
                .get("env")
                .and_then(|v| v.get("CLAUDE_GATEWAY_ALLOW_LOOPBACK"))
                .and_then(Value::as_str)
                == Some("1"))
    } else {
        let doc = document(optional_text(&paths.codex_config)?.as_deref())?;
        let auth = object(optional_text(&paths.codex_auth)?.as_deref())?;
        let local_credential = auth
            .get("OPENAI_API_KEY")
            .and_then(Value::as_str)
            .is_some_and(|key| key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        Ok(codex_gateway(&doc)
            && (local_credential
                || paths.codex_catalog.is_file()
                || doc.get("model_provider").and_then(Item::as_str) == Some("agent_gateway")))
    }
}
fn load(path: &Path) -> Result<Snapshot> {
    if !path.is_file() {
        return Ok(Snapshot::default());
    }
    let encrypted = fs::read(path)?;
    let plain = Zeroizing::new(crate::gui_settings::protect_or_unprotect(
        &encrypted, false,
    )?);
    serde_json::from_slice(&plain).context(i18n::tr(
        "账号模式备份已损坏",
        "The account-mode backup is damaged",
    ))
}
fn save(path: &Path, snapshot: &Snapshot) -> Result<()> {
    let plain = Zeroizing::new(serde_json::to_vec(snapshot)?);
    backup::atomic_write(path, &crate::gui_settings::protect(&plain)?)
}
fn tracked(paths: &ClientPaths, target: ClientTarget, path: &Path) -> bool {
    if target == ClientTarget::ClaudeCode {
        path == paths.claude_settings || path == paths.claude_model_cache
    } else {
        path == paths.codex_config || path == paths.codex_auth || path == paths.codex_catalog
    }
}

fn official_cli_account() -> Result<Option<String>> {
    let official = ClientPaths::discover(None, None)?;
    let mut auth = object(optional_text(&official.codex_auth)?.as_deref())?;
    if !auth.get("tokens").is_some_and(|tokens| !tokens.is_null()) {
        let snapshot = load(&marker(&official, ClientTarget::CodexDesktop))?;
        if let Some(saved) = snapshot
            .files
            .get(official.codex_auth.to_string_lossy().as_ref())
        {
            auth = object(saved.before.as_deref())?;
        }
    }
    auth.remove("OPENAI_API_KEY");
    if auth.get("tokens").is_some_and(|tokens| !tokens.is_null()) {
        auth.insert("auth_mode".into(), Value::String("chatgpt".into()));
        Ok(Some(serde_json::to_string_pretty(&auth)?))
    } else {
        Ok(None)
    }
}

/// Persist the first pre-gateway version and the latest owned values before any
/// write. A failed/partial write remains recoverable and reconnect never replaces
/// the original account snapshot with another gateway configuration.
pub fn apply_gateway_files(
    paths: &ClientPaths,
    target: ClientTarget,
    files: Vec<(PathBuf, Vec<u8>)>,
) -> Result<()> {
    let marker = marker(paths, target);
    let mut snapshot = load(&marker)?;
    let legacy = snapshot.files.is_empty() && is_managed_at(paths, target)?;
    for (path, bytes) in &files {
        if !tracked(paths, target, path) {
            continue;
        }
        let id = path.to_string_lossy().into_owned();
        let mut before = optional_text(path)?;
        if legacy {
            before = clean_legacy(paths, target, path, before.as_deref())?;
        }
        if !cfg!(test)
            && target == ClientTarget::CodexCli
            && path == &paths.codex_auth
            && !object(before.as_deref())?
                .get("tokens")
                .is_some_and(|tokens| !tokens.is_null())
        {
            // Seed a first CLI gateway session with the official account login,
            // without sharing Desktop's gateway endpoint or modifying its files.
            if let Some(account) = official_cli_account()? {
                before = Some(account);
            }
        }
        let written = String::from_utf8(bytes.clone())?;
        if path == &paths.codex_auth
            && let Some(saved) = snapshot.files.get_mut(&id)
        {
            let recent = object(before.as_deref())?;
            if recent.get("tokens").is_some_and(|tokens| !tokens.is_null()) {
                let mut original = object(saved.before.as_deref())?;
                for key in ["tokens", "last_refresh"] {
                    if let Some(value) = recent.get(key) {
                        original.insert(key.into(), value.clone());
                    } else {
                        original.remove(key);
                    }
                }
                original.insert("auth_mode".into(), Value::String("chatgpt".into()));
                saved.before = Some(serde_json::to_string_pretty(&original)?);
            }
        }
        snapshot
            .files
            .entry(id)
            .and_modify(|saved| saved.written = written.clone())
            .or_insert(SavedFile { before, written });
    }
    save(&marker, &snapshot)?;
    for (path, bytes) in files {
        backup::atomic_write(&path, &bytes)?;
    }
    Ok(())
}

fn restore_json(
    current: &mut Map<String, Value>,
    before: &Map<String, Value>,
    written: &Map<String, Value>,
) {
    let keys: std::collections::BTreeSet<_> =
        before.keys().chain(written.keys()).cloned().collect();
    for key in keys {
        if before.get(&key) == written.get(&key) {
            continue;
        }
        if let (Some(Value::Object(owned)), Some(Value::Object(now))) =
            (written.get(&key), current.get_mut(&key))
        {
            if before.get(&key).is_none_or(Value::is_object) {
                restore_json(
                    now,
                    before
                        .get(&key)
                        .and_then(Value::as_object)
                        .unwrap_or(&Map::new()),
                    owned,
                );
                if now.is_empty() && !before.contains_key(&key) {
                    current.remove(&key);
                }
            }
        } else if current.get(&key) == written.get(&key) {
            if let Some(value) = before.get(&key) {
                current.insert(key, value.clone());
            } else {
                current.remove(&key);
            }
        }
    }
}
fn restore_toml(current: &mut DocumentMut, before: &DocumentMut, written: &DocumentMut) {
    // Only these fields are owned. Replacing a provider restores only that
    // provider, keeping unrelated providers and user settings intact.
    for key in [
        "model",
        "model_provider",
        "openai_base_url",
        "model_catalog_json",
        "cli_auth_credentials_store",
    ] {
        let old = before.get(key).map(ToString::to_string);
        let owned = written.get(key).map(ToString::to_string);
        if old != owned && current.get(key).map(ToString::to_string) == owned {
            if let Some(value) = before.get(key) {
                current[key] = value.clone();
            } else {
                current.remove(key);
            }
        }
    }
    if let Some(id) = written.get("model_provider").and_then(Item::as_str) {
        let old = before
            .get("model_providers")
            .and_then(|table| table.get(id));
        let owned = written
            .get("model_providers")
            .and_then(|table| table.get(id));
        let now = current
            .get("model_providers")
            .and_then(|table| table.get(id));
        if old.map(ToString::to_string) != owned.map(ToString::to_string)
            && now.map(ToString::to_string) == owned.map(ToString::to_string)
            && let Some(table) = current
                .get_mut("model_providers")
                .and_then(Item::as_table_mut)
        {
            if let Some(old) = old {
                table.insert(id, old.clone());
            } else {
                table.remove(id);
            }
        }
    }
}
fn clean_legacy(
    paths: &ClientPaths,
    target: ClientTarget,
    path: &Path,
    raw: Option<&str>,
) -> Result<Option<String>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if path == paths.codex_config {
        let mut doc = document(Some(raw))?;
        if codex_gateway(&doc) {
            let id = doc
                .get("model_provider")
                .and_then(Item::as_str)
                .unwrap_or("openai")
                .to_owned();
            if let Some(table) = doc.get_mut("model_providers").and_then(Item::as_table_mut) {
                table.remove(&id);
            }
            for key in [
                "openai_base_url",
                "model_catalog_json",
                "model",
                "cli_auth_credentials_store",
            ] {
                doc.remove(key);
            }
            doc["model_provider"] = toml_edit::value("openai");
        }
        return Ok(Some(doc.to_string()));
    }
    if path == paths.codex_auth {
        let mut auth = object(Some(raw))?;
        auth.remove("OPENAI_API_KEY");
        if auth.contains_key("tokens") {
            auth.insert("auth_mode".into(), Value::String("chatgpt".into()));
        } else {
            auth.remove("auth_mode");
        }
        return Ok(Some(serde_json::to_string_pretty(&auth)?));
    }
    if target == ClientTarget::ClaudeCode && path == paths.claude_settings {
        let mut settings = object(Some(raw))?;
        settings.remove("modelPicker");
        settings.remove("model");
        if let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) {
            env.retain(|key, _| {
                !(matches!(
                    key.as_str(),
                    "ANTHROPIC_BASE_URL"
                        | "ANTHROPIC_AUTH_TOKEN"
                        | "CLAUDE_GATEWAY_ALLOW_LOOPBACK"
                        | "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"
                        | "CLAUDE_CODE_MODEL_LIST_REFRESH_INTERVAL_MS"
                ) || key.starts_with("ANTHROPIC_DEFAULT_"))
            });
        }
        return Ok(Some(serde_json::to_string_pretty(&settings)?));
    }
    // Old versions did not retain a pre-takeover catalog. Delete only the
    // dedicated gateway cache, never a client's general-purpose cache/history.
    Ok(None)
}

pub fn restore(target: ClientTarget) -> Result<()> {
    if target == ClientTarget::ClaudeDesktop {
        crate::claude_desktop_registry::restore_account_mode()?;
        return Ok(());
    }
    let paths = paths(target)?;
    restore_at(&paths, target)?;
    if !cfg!(test)
        && target == ClientTarget::CodexCli
        && !object(optional_text(&paths.codex_auth)?.as_deref())?
            .get("tokens")
            .is_some_and(|tokens| !tokens.is_null())
        && let Some(account) = official_cli_account()?
    {
        backup::atomic_write(&paths.codex_auth, account.as_bytes())?;
    }
    Ok(())
}

pub fn validate_restore(target: ClientTarget) -> Result<()> {
    if target == ClientTarget::ClaudeDesktop {
        return crate::claude_desktop_registry::validate_account_restore();
    }
    let paths = paths(target)?;
    validate_at(&paths, target, &load(&marker(&paths, target))?)
}
fn validate_at(paths: &ClientPaths, target: ClientTarget, snapshot: &Snapshot) -> Result<()> {
    for (id, saved) in &snapshot.files {
        let path = Path::new(id);
        if !tracked(paths, target, path) {
            bail!(
                "{}",
                i18n::tr(
                    "账号模式备份包含无效路径",
                    "The account-mode backup contains an invalid path"
                )
            );
        }
        if path == paths.codex_config {
            document(saved.before.as_deref())?;
            document(Some(&saved.written))?;
        } else {
            if let Some(before) = &saved.before {
                serde_json::from_str::<Value>(before)?;
            }
            serde_json::from_str::<Value>(&saved.written)?;
        }
    }
    if target == ClientTarget::ClaudeCode {
        object(optional_text(&paths.claude_settings)?.as_deref())?;
    } else {
        document(optional_text(&paths.codex_config)?.as_deref())?;
        object(optional_text(&paths.codex_auth)?.as_deref())?;
    }
    Ok(())
}
fn restore_at(paths: &ClientPaths, target: ClientTarget) -> Result<()> {
    let marker = marker(paths, target);
    let snapshot = load(&marker)?;
    validate_at(paths, target, &snapshot)?;
    let allowed = if target == ClientTarget::ClaudeCode {
        vec![&paths.claude_settings, &paths.claude_model_cache]
    } else {
        vec![&paths.codex_config, &paths.codex_auth, &paths.codex_catalog]
    };
    // Validate every stored path before writing any file.
    if snapshot.files.keys().any(|id| {
        !allowed
            .iter()
            .any(|path| path.to_string_lossy() == id.as_str())
    }) {
        bail!(
            "{}",
            i18n::tr(
                "账号模式备份包含无效路径",
                "The account-mode backup contains an invalid path"
            )
        );
    }
    if snapshot.files.is_empty() && !is_managed_at(paths, target)? {
        return Ok(());
    }
    for path in allowed {
        let raw = optional_text(path)?;
        let saved = snapshot.files.get(path.to_string_lossy().as_ref());
        let restored = if let Some(saved) = saved {
            if path == &paths.codex_config {
                let mut doc = document(raw.as_deref())?;
                restore_toml(
                    &mut doc,
                    &document(saved.before.as_deref())?,
                    &document(Some(&saved.written))?,
                );
                Some(doc.to_string())
            } else if path == &paths.codex_auth || path == &paths.claude_settings {
                let mut now = object(raw.as_deref())?;
                let before = object(saved.before.as_deref())?;
                let written = object(Some(&saved.written))?;
                // A login performed while the gateway was active takes precedence
                // over the old snapshot. Do not resurrect an older refresh token.
                let newer_login = path == &paths.codex_auth
                    && now.get("tokens").is_some_and(|tokens| !tokens.is_null());
                if newer_login {
                    if now.get("OPENAI_API_KEY") == written.get("OPENAI_API_KEY") {
                        now.remove("OPENAI_API_KEY");
                    }
                    now.insert("auth_mode".into(), Value::String("chatgpt".into()));
                } else {
                    restore_json(&mut now, &before, &written);
                }
                Some(serde_json::to_string_pretty(&now)?)
            } else if raw.as_deref() == Some(saved.written.as_str()) {
                saved.before.clone()
            } else {
                raw.clone()
            }
        } else if snapshot.files.is_empty() {
            clean_legacy(paths, target, path, raw.as_deref())?
        } else {
            raw.clone()
        };
        match restored {
            Some(text) => {
                if raw.as_deref() != Some(text.as_str()) {
                    backup::atomic_write(path, text.as_bytes())?;
                }
            }
            None => {
                backup::remove_file_if_exists(path)?;
            }
        }
    }
    backup::remove_file_if_exists(&marker)?;
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    fn test_paths() -> ClientPaths {
        let root = std::env::temp_dir().join(format!(
            "agent-switch-account-test-{:032x}",
            rand::random::<u128>()
        ));
        fs::create_dir_all(&root).unwrap();
        ClientPaths::discover(Some(root.join(".codex")), Some(root.join(".claude"))).unwrap()
    }
    #[test]
    fn snapshot_survives_reconnect_and_preserves_new_settings_and_login() {
        let paths = test_paths();
        backup::atomic_write(
            &paths.codex_config,
            b"model = 'original'\n[unrelated]\nkeep = true\n",
        )
        .unwrap();
        backup::atomic_write(
            &paths.codex_auth,
            br#"{"auth_mode":"chatgpt","tokens":{"refresh_token":"old-secret"}}"#,
        )
        .unwrap();
        for port in [1234, 5678] {
            apply_gateway_files(&paths, ClientTarget::CodexDesktop, vec![
                (paths.codex_config.clone(), format!("model = 'gateway'\nmodel_provider = 'openai'\nopenai_base_url = 'http://127.0.0.1:{port}'\n[unrelated]\nkeep = true\n").into_bytes()),
                (paths.codex_auth.clone(), br#"{"auth_mode":"apikey","OPENAI_API_KEY":"local-secret"}"#.to_vec()),
            ]).unwrap();
        }
        let encrypted = fs::read(marker(&paths, ClientTarget::CodexDesktop)).unwrap();
        assert!(!encrypted.windows(10).any(|bytes| bytes == b"old-secret"));
        fs::write(&paths.codex_auth, br#"{"auth_mode":"chatgpt","OPENAI_API_KEY":"local-secret","tokens":{"refresh_token":"new-secret"}}"#).unwrap();
        let config = fs::read_to_string(&paths.codex_config)
            .unwrap()
            .replace("keep = true", "keep = false");
        fs::write(&paths.codex_config, config).unwrap();
        restore_at(&paths, ClientTarget::CodexDesktop).unwrap();
        let doc = document(optional_text(&paths.codex_config).unwrap().as_deref()).unwrap();
        assert_eq!(doc["model"].as_str(), Some("original"));
        assert_eq!(doc["unrelated"]["keep"].as_bool(), Some(false));
        assert!(doc.get("openai_base_url").is_none());
        let auth = object(optional_text(&paths.codex_auth).unwrap().as_deref()).unwrap();
        assert_eq!(auth["tokens"]["refresh_token"], "new-secret");
        assert!(!auth.contains_key("OPENAI_API_KEY"));
        assert!(!is_managed_at(&paths, ClientTarget::CodexDesktop).unwrap());
        fs::remove_dir_all(paths.codex_home.parent().unwrap()).unwrap();
    }
    #[test]
    fn account_credentials_return_without_erasing_other_claude_environment() {
        let paths = test_paths();
        backup::atomic_write(
            &paths.claude_settings,
            br#"{"env":{"MY_SETTING":"original"},"permissions":{"allow":["Read"]}}"#,
        )
        .unwrap();
        apply_gateway_files(&paths, ClientTarget::ClaudeCode, vec![(paths.claude_settings.clone(), br#"{"env":{"MY_SETTING":"original","ANTHROPIC_BASE_URL":"http://127.0.0.1:1234","ANTHROPIC_AUTH_TOKEN":"local-secret","CLAUDE_GATEWAY_ALLOW_LOOPBACK":"1"},"modelPicker":{},"permissions":{"allow":["Read"]}}"#.to_vec())]).unwrap();
        let mut now = object(optional_text(&paths.claude_settings).unwrap().as_deref()).unwrap();
        now["env"]["MY_SETTING"] = Value::String("edited".into());
        fs::write(&paths.claude_settings, serde_json::to_vec(&now).unwrap()).unwrap();
        restore_at(&paths, ClientTarget::ClaudeCode).unwrap();
        let restored = object(optional_text(&paths.claude_settings).unwrap().as_deref()).unwrap();
        assert_eq!(restored["env"]["MY_SETTING"], "edited");
        assert!(restored["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
        assert!(restored.get("modelPicker").is_none());
        assert_eq!(restored["permissions"]["allow"][0], "Read");
        fs::remove_dir_all(paths.codex_home.parent().unwrap()).unwrap();
    }
    #[test]
    fn legacy_codex_cleanup_keeps_existing_account_tokens() {
        let paths = test_paths();
        backup::atomic_write(
            &paths.codex_config,
            b"model_provider='openai'\nopenai_base_url='http://127.0.0.1:1234'\nmodel='gateway'\n",
        )
        .unwrap();
        backup::atomic_write(&paths.codex_auth, br#"{"OPENAI_API_KEY":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","tokens":{"refresh_token":"account-secret"}}"#).unwrap();
        restore_at(&paths, ClientTarget::CodexDesktop).unwrap();
        let restored = object(optional_text(&paths.codex_auth).unwrap().as_deref()).unwrap();
        assert_eq!(restored["tokens"]["refresh_token"], "account-secret");
        assert!(!restored.contains_key("OPENAI_API_KEY"));
        assert!(!is_managed_at(&paths, ClientTarget::CodexDesktop).unwrap());
        fs::remove_dir_all(paths.codex_home.parent().unwrap()).unwrap();
    }
    #[test]
    fn refreshed_login_is_saved_before_another_gateway_connection() {
        let paths = test_paths();
        backup::atomic_write(
            &paths.codex_auth,
            br#"{"auth_mode":"chatgpt","tokens":{"refresh_token":"first"}}"#,
        )
        .unwrap();
        let gateway = br#"{"auth_mode":"apikey","OPENAI_API_KEY":"local-secret"}"#;
        apply_gateway_files(
            &paths,
            ClientTarget::CodexDesktop,
            vec![(paths.codex_auth.clone(), gateway.to_vec())],
        )
        .unwrap();
        fs::write(
            &paths.codex_auth,
            br#"{"auth_mode":"chatgpt","tokens":{"refresh_token":"refreshed"}}"#,
        )
        .unwrap();
        apply_gateway_files(
            &paths,
            ClientTarget::CodexDesktop,
            vec![(paths.codex_auth.clone(), gateway.to_vec())],
        )
        .unwrap();
        restore_at(&paths, ClientTarget::CodexDesktop).unwrap();
        let restored = object(optional_text(&paths.codex_auth).unwrap().as_deref()).unwrap();
        assert_eq!(restored["auth_mode"], "chatgpt");
        assert_eq!(restored["tokens"]["refresh_token"], "refreshed");
        assert!(!restored.contains_key("OPENAI_API_KEY"));
        fs::remove_dir_all(paths.codex_home.parent().unwrap()).unwrap();
    }
    #[test]
    fn restoration_keeps_new_environment_when_no_environment_existed_before() {
        let paths = test_paths();
        apply_gateway_files(&paths, ClientTarget::ClaudeCode, vec![(paths.claude_settings.clone(), br#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:1234","ANTHROPIC_AUTH_TOKEN":"local","CLAUDE_GATEWAY_ALLOW_LOOPBACK":"1"}}"#.to_vec())]).unwrap();
        let mut now = object(optional_text(&paths.claude_settings).unwrap().as_deref()).unwrap();
        now["env"]["USER_ADDED"] = Value::String("keep".into());
        fs::write(&paths.claude_settings, serde_json::to_vec(&now).unwrap()).unwrap();
        restore_at(&paths, ClientTarget::ClaudeCode).unwrap();
        let restored = object(optional_text(&paths.claude_settings).unwrap().as_deref()).unwrap();
        assert_eq!(restored["env"]["USER_ADDED"], "keep");
        assert!(restored["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
        fs::remove_dir_all(paths.codex_home.parent().unwrap()).unwrap();
    }
    #[test]
    fn unrelated_loopback_provider_is_not_taken_over_or_deleted() {
        let paths = test_paths();
        let config = "model_provider='local'\n[model_providers.local]\nbase_url='http://127.0.0.1:1234/v1'\n";
        backup::atomic_write(&paths.codex_config, config.as_bytes()).unwrap();
        backup::atomic_write(&paths.codex_auth, br#"{"OPENAI_API_KEY":"user-key"}"#).unwrap();
        assert!(!is_managed_at(&paths, ClientTarget::CodexDesktop).unwrap());
        restore_at(&paths, ClientTarget::CodexDesktop).unwrap();
        assert_eq!(fs::read_to_string(&paths.codex_config).unwrap(), config);
        fs::remove_dir_all(paths.codex_home.parent().unwrap()).unwrap();
    }
    #[test]
    fn invalid_backup_path_is_rejected_before_any_configuration_write() {
        let paths = test_paths();
        backup::atomic_write(&paths.codex_config, b"model='keep'").unwrap();
        let mut snapshot = Snapshot::default();
        snapshot.files.insert(
            paths
                .codex_home
                .join("unrelated.json")
                .to_string_lossy()
                .into_owned(),
            SavedFile {
                before: None,
                written: "{}".into(),
            },
        );
        save(&marker(&paths, ClientTarget::CodexDesktop), &snapshot).unwrap();
        assert!(restore_at(&paths, ClientTarget::CodexDesktop).is_err());
        assert_eq!(
            fs::read_to_string(&paths.codex_config).unwrap(),
            "model='keep'"
        );
        assert!(marker(&paths, ClientTarget::CodexDesktop).is_file());
        fs::remove_dir_all(paths.codex_home.parent().unwrap()).unwrap();
    }
}
