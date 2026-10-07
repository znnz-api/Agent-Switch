use crate::gateway::DEFAULT_GATEWAY_URL;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use zeroize::{Zeroize, Zeroizing};

const KEY_MAGIC: &[u8] = b"ZNKEY1\0";

#[derive(Clone, Serialize, Deserialize)]
pub struct ClientConfiguration {
    pub gateway_url: String,
    pub api_key: String,
    #[serde(default)]
    pub preset_provider: Option<String>,
}

impl Drop for ClientConfiguration {
    fn drop(&mut self) {
        self.api_key.zeroize();
    }
}

pub type ClientConfigurations = std::collections::BTreeMap<String, ClientConfiguration>;

pub fn load_client_configurations() -> Result<ClientConfigurations> {
    let path = settings_root()?.join("client-configurations.dpapi");
    if !path.is_file() {
        return Ok(ClientConfigurations::new());
    }
    let encrypted = fs::read(path).context("无法读取客户端配置记忆")?;
    let plain = Zeroizing::new(protect_or_unprotect(&encrypted, false)?);
    serde_json::from_slice(&plain).context("客户端配置记忆格式无效")
}

pub fn save_client_configurations(configurations: &ClientConfigurations) -> Result<()> {
    let plain = Zeroizing::new(serde_json::to_vec(configurations)?);
    let encrypted = protect(&plain)?;
    crate::backup::atomic_write(
        &settings_root()?.join("client-configurations.dpapi"),
        &encrypted,
    )
    .context("无法保存客户端配置记忆")
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GatewayHistoryEntry {
    pub gateway_url: String,
    pub api_key: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub hidden_models: Vec<String>,
    #[serde(default)]
    pub conversion_enabled: bool,
}

impl GatewayHistoryEntry {
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.gateway_url
        } else {
            &self.name
        }
    }
}

impl Drop for GatewayHistoryEntry {
    fn drop(&mut self) {
        self.api_key.zeroize();
    }
}

pub fn record_history(history: &mut Vec<GatewayHistoryEntry>, gateway_url: &str, api_key: &str) {
    let gateway_url = gateway_url.trim().trim_end_matches('/');
    let api_key = api_key.trim();
    if api_key.is_empty() || gateway_url.is_empty() {
        return;
    }
    // An existing pair keeps its user-defined name and position. Reusing a
    // configuration must not undo a drag-and-drop ordering.
    if history
        .iter()
        .any(|entry| entry.gateway_url == gateway_url && entry.api_key == api_key)
    {
        return;
    }
    history.insert(
        0,
        GatewayHistoryEntry {
            gateway_url: gateway_url.to_owned(),
            api_key: api_key.to_owned(),
            name: String::new(),
            hidden_models: Vec::new(),
            conversion_enabled: false,
        },
    );
}

pub fn load_history() -> Result<Vec<GatewayHistoryEntry>> {
    let path = settings_root()?.join("gateway-history.dpapi");
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let encrypted = fs::read(path).context("无法读取配置历史")?;
    let plain = Zeroizing::new(protect_or_unprotect(&encrypted, false)?);
    serde_json::from_slice(&plain).context("配置历史格式无效")
}

pub fn save_history(history: &[GatewayHistoryEntry]) -> Result<()> {
    let plain = Zeroizing::new(serde_json::to_vec(history)?);
    let encrypted = protect(&plain)?;
    crate::backup::atomic_write(&settings_root()?.join("gateway-history.dpapi"), &encrypted)
        .context("无法保存配置历史")
}

pub fn hidden_models_for_configuration(gateway_url: &str, api_key: &str) -> Result<Vec<String>> {
    let url = gateway_url.trim().trim_end_matches('/');
    Ok(load_history()?
        .iter()
        .find(|entry| entry.gateway_url == url && entry.api_key == api_key.trim())
        .map(|entry| entry.hidden_models.clone())
        .unwrap_or_default())
}

pub fn conversion_for_configuration(gateway_url: &str, api_key: &str) -> Result<bool> {
    let url = gateway_url.trim().trim_end_matches('/');
    Ok(load_history()?.iter().any(|entry| {
        entry.gateway_url == url && entry.api_key == api_key.trim() && entry.conversion_enabled
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuiPreferences {
    pub gateway_url: String,
    #[serde(default)]
    pub preset_provider: Option<String>,
    pub selected_client: String,
    pub remember_key: bool,
    #[serde(default = "default_gateway_model_mode")]
    pub codex_cli_model_mode: String,
    #[serde(default = "default_gateway_model_mode")]
    pub codex_desktop_model_mode: String,
    #[serde(default = "default_gateway_model_mode")]
    pub claude_code_model_mode: String,
    // 保留旧字段名，兼容 0.6.0 已保存的 four-slots/full-catalog 值。
    #[serde(default = "default_claude_desktop_model_mode")]
    pub claude_desktop_model_mode: String,
}

fn default_gateway_model_mode() -> String {
    "gateway".to_owned()
}

fn default_claude_desktop_model_mode() -> String {
    "four-slots".to_owned()
}

impl Default for GuiPreferences {
    fn default() -> Self {
        Self {
            gateway_url: DEFAULT_GATEWAY_URL.to_owned(),
            preset_provider: None,
            selected_client: "codex-desktop".to_owned(),
            remember_key: true,
            codex_cli_model_mode: default_gateway_model_mode(),
            codex_desktop_model_mode: default_gateway_model_mode(),
            claude_code_model_mode: default_gateway_model_mode(),
            claude_desktop_model_mode: default_claude_desktop_model_mode(),
        }
    }
}

pub fn load() -> Result<(GuiPreferences, Option<Zeroizing<String>>)> {
    let preferences_file = preferences_path()?;
    let preferences = if preferences_file.is_file() {
        let raw = fs::read_to_string(&preferences_file).context("无法读取 GUI 设置")?;
        serde_json::from_str(&raw).context("GUI 设置文件格式无效")?
    } else {
        GuiPreferences::default()
    };
    let key = if preferences.remember_key {
        load_key()?
    } else {
        None
    };
    Ok((preferences, key))
}

pub fn save(preferences: &GuiPreferences, api_key: &str) -> Result<()> {
    let path = preferences_path()?;
    let raw = format!("{}\n", serde_json::to_string_pretty(preferences)?);
    crate::backup::atomic_write(&path, raw.as_bytes()).context("无法保存 GUI 设置")?;

    let key_path = key_path()?;
    if preferences.remember_key {
        if api_key.trim().is_empty() {
            let _ = crate::backup::remove_file_if_exists(&key_path)?;
            return Ok(());
        }
        let encrypted = protect(api_key.as_bytes())?;
        let mut bytes = Vec::with_capacity(KEY_MAGIC.len() + encrypted.len());
        bytes.extend_from_slice(KEY_MAGIC);
        bytes.extend_from_slice(&encrypted);
        crate::backup::atomic_write(&key_path, &bytes).context("无法保存加密 API Key")?;
    } else {
        let _ = crate::backup::remove_file_if_exists(&key_path)?;
    }
    Ok(())
}

fn load_key() -> Result<Option<Zeroizing<String>>> {
    let path = key_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let encrypted = fs::read(&path).context("无法读取加密 API Key")?;
    if !encrypted.starts_with(KEY_MAGIC) {
        bail!("加密 API Key 文件格式无效，请在界面中重新输入 Key");
    }
    let plain = protect_or_unprotect(&encrypted[KEY_MAGIC.len()..], false)?;
    let key = String::from_utf8(plain).context("解密后的 API Key 不是有效文本")?;
    Ok(Some(Zeroizing::new(key)))
}

fn settings_root() -> Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("无法确定 LOCALAPPDATA")?;
    Ok(local.join("Agent-Switch").join("gui"))
}

fn preferences_path() -> Result<PathBuf> {
    Ok(settings_root()?.join("settings.json"))
}

fn key_path() -> Result<PathBuf> {
    Ok(settings_root()?.join("api-key.dpapi"))
}

pub(crate) fn protect(input: &[u8]) -> Result<Vec<u8>> {
    protect_or_unprotect(input, true)
}

#[cfg(windows)]
pub(crate) fn protect_or_unprotect(input: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use windows::core::PCWSTR;

    if input.len() > u32::MAX as usize {
        bail!("API Key 数据过大");
    }
    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        if encrypt {
            CryptProtectData(
                &input_blob,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .context("Windows DPAPI 无法加密 API Key")?;
        } else {
            CryptUnprotectData(
                &input_blob,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .context("Windows DPAPI 无法解密 API Key")?;
        }
    }
    if output.cbData > 0 && output.pbData.is_null() {
        bail!("Windows DPAPI 返回了无效数据");
    }
    let bytes = if output.cbData == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() }
    };
    if !output.pbData.is_null() {
        unsafe {
            let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        }
    }
    Ok(bytes)
}

#[cfg(not(windows))]
pub(crate) fn protect_or_unprotect(_input: &[u8], _encrypt: bool) -> Result<Vec<u8>> {
    bail!("当前平台尚未接入系统钥匙串，不能保存 API Key")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn encrypted_client_memory_keeps_independent_and_partial_inputs() {
        let mut configurations = ClientConfigurations::new();
        for client in [
            "codex-cli",
            "codex-desktop",
            "claude-code",
            "claude-desktop",
        ] {
            configurations.insert(
                client.into(),
                ClientConfiguration {
                    gateway_url: format!("https://{client}.test/v1"),
                    api_key: format!("secret-{client}"),
                    preset_provider: Some(format!("供应商-{client}")),
                },
            );
        }
        configurations.get_mut("codex-cli").unwrap().api_key.clear();
        let plain = Zeroizing::new(serde_json::to_vec(&configurations).unwrap());
        let encrypted = protect(&plain).unwrap();
        assert!(!encrypted.windows(7).any(|window| window == b"secret-"));
        let decrypted = Zeroizing::new(protect_or_unprotect(&encrypted, false).unwrap());
        let restored: ClientConfigurations = serde_json::from_slice(&decrypted).unwrap();
        assert_eq!(restored.len(), 4);
        assert_eq!(restored["codex-cli"].api_key, "");
        for client in ["codex-desktop", "claude-code", "claude-desktop"] {
            assert_eq!(restored[client].api_key, format!("secret-{client}"));
            assert_eq!(
                restored[client].preset_provider.as_deref(),
                Some(format!("供应商-{client}").as_str())
            );
        }
    }

    #[test]
    fn history_deduplicates_pairs_without_resetting_user_order() {
        let mut history = Vec::new();
        record_history(&mut history, "https://api.example.com", "key-one");
        history[0].name = "常用配置".into();
        history[0].hidden_models = vec!["old-model".into()];
        record_history(&mut history, "https://api.example.com", "key-two");
        record_history(&mut history, " https://api.example.com/ ", " key-one ");
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].api_key, "key-one");
        assert_eq!(history[1].display_name(), "常用配置");
        assert_eq!(history[1].hidden_models, vec!["old-model"]);
        assert!(history[0].hidden_models.is_empty());
        assert!(history[0].name.is_empty());
        assert_eq!(history[0].api_key, "key-two");
        record_history(&mut history, "https://other.example.com", "key-one");
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].gateway_url, "https://other.example.com");
        record_history(&mut history, "", "key-one");
        record_history(&mut history, "https://api.example.com", "  ");
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn encrypted_history_preserves_url_key_pairs_and_deletion() {
        let mut history = Vec::new();
        record_history(
            &mut history,
            "https://api.example.com",
            "history-secret-one",
        );
        record_history(
            &mut history,
            "https://api.example.com",
            "history-secret-two",
        );
        history.remove(1);
        history[0].name = "备用配置".into();
        history[0].hidden_models = vec!["retired-model".into(), "missing-model".into()];
        let raw = Zeroizing::new(serde_json::to_vec(&history).unwrap());
        let encrypted = protect(&raw).unwrap();
        assert!(
            !encrypted
                .windows(b"history-secret".len())
                .any(|value| value == b"history-secret")
        );
        let decrypted = Zeroizing::new(protect_or_unprotect(&encrypted, false).unwrap());
        let restored: Vec<GatewayHistoryEntry> = serde_json::from_slice(&decrypted).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].gateway_url, "https://api.example.com");
        assert_eq!(restored[0].api_key, "history-secret-two");
        assert_eq!(restored[0].name, "备用配置");
        assert_eq!(
            restored[0].hidden_models,
            vec!["retired-model", "missing-model"]
        );
    }

    #[test]
    fn history_without_names_remains_readable() {
        let history: Vec<GatewayHistoryEntry> = serde_json::from_str(
            r#"[{"gateway_url":"https://api.example.com","api_key":"old-key"}]"#,
        )
        .unwrap();
        assert!(history[0].name.is_empty());
        assert!(history[0].hidden_models.is_empty());
        assert_eq!(history[0].gateway_url, "https://api.example.com");
    }

    #[test]
    fn dpapi_round_trip_does_not_store_plaintext() {
        let secret = b"temporary-gui-key";
        let encrypted = protect(secret).unwrap();
        assert_ne!(encrypted, secret);
        assert_eq!(protect_or_unprotect(&encrypted, false).unwrap(), secret);
    }

    #[test]
    fn old_gui_preferences_keep_safe_per_client_defaults() {
        let preferences: GuiPreferences = serde_json::from_str(
            r#"{"gateway_url":"https://api.example.com","selected_client":"claude-desktop","remember_key":true}"#,
        )
        .unwrap();
        assert_eq!(preferences.codex_cli_model_mode, "gateway");
        assert_eq!(preferences.codex_desktop_model_mode, "gateway");
        assert_eq!(preferences.claude_code_model_mode, "gateway");
        assert_eq!(preferences.claude_desktop_model_mode, "four-slots");
    }
    #[test]
    fn legacy_claude_desktop_mode_is_preserved_for_runtime_migration() {
        let preferences: GuiPreferences = serde_json::from_str(
            r#"{"gateway_url":"https://api.example.com","selected_client":"claude-desktop","remember_key":true,"claude_desktop_model_mode":"full-catalog"}"#,
        )
        .unwrap();
        assert_eq!(preferences.claude_desktop_model_mode, "full-catalog");
        assert_eq!(
            crate::gui_worker::ModelListMode::from_id(&preferences.claude_desktop_model_mode),
            crate::gui_worker::ModelListMode::Gateway
        );
    }
}
