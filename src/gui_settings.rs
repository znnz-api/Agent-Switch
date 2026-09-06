use crate::gateway::DEFAULT_GATEWAY_URL;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use zeroize::Zeroizing;

const KEY_MAGIC: &[u8] = b"ZNKEY1\0";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuiPreferences {
    pub gateway_url: String,
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
    let preferences = if preferences_path()?.is_file() {
        let raw = fs::read_to_string(preferences_path()?).context("无法读取 GUI 设置")?;
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
    Ok(local.join("znnz-client").join("gui"))
}

fn preferences_path() -> Result<PathBuf> {
    Ok(settings_root()?.join("settings.json"))
}

fn key_path() -> Result<PathBuf> {
    Ok(settings_root()?.join("api-key.dpapi"))
}

fn protect(input: &[u8]) -> Result<Vec<u8>> {
    protect_or_unprotect(input, true)
}

#[cfg(windows)]
fn protect_or_unprotect(input: &[u8], encrypt: bool) -> Result<Vec<u8>> {
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
fn protect_or_unprotect(_input: &[u8], _encrypt: bool) -> Result<Vec<u8>> {
    bail!("当前平台尚未接入系统钥匙串，不能保存 API Key")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

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
