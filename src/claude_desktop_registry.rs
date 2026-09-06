#[cfg(windows)]
mod imp {
    use crate::backup;
    use anyhow::{Context, Result, bail};
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    use windows::core::PCWSTR;
    use winreg::enums::{
        HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_BINARY, REG_DWORD, REG_DWORD_BIG_ENDIAN,
        REG_EXPAND_SZ, REG_FULL_RESOURCE_DESCRIPTOR, REG_LINK, REG_MULTI_SZ, REG_NONE, REG_QWORD,
        REG_RESOURCE_LIST, REG_RESOURCE_REQUIREMENTS_LIST, REG_SZ, RegType,
    };
    use winreg::{RegKey, RegValue};

    const POLICY_PATH: &str = r"SOFTWARE\Policies\Claude";
    const BACKUP_MAGIC: &[u8] = b"ZNDP1\0";
    pub const MANAGED_VALUES: [&str; 6] = [
        "inferenceGatewayBaseUrl",
        "inferenceGatewayApiKey",
        "inferenceProvider",
        "inferenceCredentialKind",
        "inferenceModels",
        "modelDiscoveryEnabled",
    ];

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct StoredValue {
        value_type: u32,
        bytes: Vec<u8>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    struct RegistrySnapshot {
        key_existed: bool,
        values: BTreeMap<String, Option<StoredValue>>,
    }

    #[derive(Debug, Clone)]
    pub struct RegistryStatus {
        pub policy_key_exists: bool,
        pub managed_values_present: Vec<String>,
        pub pending_backup: bool,
        pub backup_path: PathBuf,
    }

    pub fn status() -> Result<RegistryStatus> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = match hkcu.open_subkey_with_flags(POLICY_PATH, KEY_READ) {
            Ok(value) => Some(value),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("无法读取 Claude Desktop 策略注册表"),
        };
        let managed_values_present = key
            .as_ref()
            .map(|key| {
                MANAGED_VALUES
                    .iter()
                    .filter(|name| key.get_raw_value(**name).is_ok())
                    .map(|name| (*name).to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let backup_path = backup_path()?;
        Ok(RegistryStatus {
            policy_key_exists: key.is_some(),
            managed_values_present,
            pending_backup: backup_path.is_file(),
            backup_path,
        })
    }

    pub fn backup_path() -> Result<PathBuf> {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .context("无法确定 LOCALAPPDATA，不能安全保存 Claude Desktop 注册表备份")?;
        Ok(local
            .join("znnz-client")
            .join("claude-desktop")
            .join("registry-backup.dpapi"))
    }

    pub fn restore_pending() -> Result<bool> {
        let path = backup_path()?;
        if !path.exists() {
            return Ok(false);
        }
        if !path.is_file() {
            bail!("Claude Desktop 注册表备份路径不是文件: {}", path.display());
        }
        let encrypted = fs::read(&path)
            .with_context(|| format!("无法读取 Claude Desktop 注册表备份 {}", path.display()))?;
        if !encrypted.starts_with(BACKUP_MAGIC) {
            bail!("Claude Desktop 注册表备份格式无效，已拒绝覆盖当前注册表");
        }
        let plain = dpapi_unprotect(&encrypted[BACKUP_MAGIC.len()..])?;
        let snapshot: RegistrySnapshot = serde_json::from_slice(&plain)
            .context("Claude Desktop 注册表备份内容无效，已拒绝覆盖当前注册表")?;
        restore_snapshot(&snapshot)?;
        fs::remove_file(&path)
            .with_context(|| format!("注册表已恢复，但无法删除备份文件 {}", path.display()))?;
        Ok(true)
    }

    pub fn capture_and_store() -> Result<()> {
        let path = backup_path()?;
        if path.exists() {
            bail!(
                "检测到尚未恢复的 Claude Desktop 注册表备份: {}。请先运行 claude-desktop restore",
                path.display()
            );
        }
        let snapshot = capture_snapshot()?;
        let plain = serde_json::to_vec(&snapshot)?;
        let encrypted = dpapi_protect(&plain)?;
        let mut file = Vec::with_capacity(BACKUP_MAGIC.len() + encrypted.len());
        file.extend_from_slice(BACKUP_MAGIC);
        file.extend_from_slice(&encrypted);
        backup::atomic_write(&path, &file)
            .with_context(|| format!("无法保存 Claude Desktop 注册表备份 {}", path.display()))
    }

    pub fn write_temporary_gateway(
        base_url: &str,
        local_token: &str,
        inference_models: &str,
        model_discovery_enabled: Option<bool>,
    ) -> Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _) = hkcu
            .create_subkey_with_flags(POLICY_PATH, KEY_READ | KEY_WRITE)
            .context("无法创建或打开 Claude Desktop 策略注册表")?;
        let values = [
            ("inferenceGatewayBaseUrl", base_url),
            ("inferenceGatewayApiKey", local_token),
            ("inferenceProvider", "gateway"),
            ("inferenceCredentialKind", "static"),
            ("inferenceModels", inference_models),
        ];
        for (name, value) in values {
            key.set_value(name, &value)
                .with_context(|| format!("无法写入 Claude Desktop 注册表值 {name}"))?;
        }
        if let Some(enabled) = model_discovery_enabled {
            let value = u32::from(enabled);
            key.set_value("modelDiscoveryEnabled", &value)
                .context("无法写入 Claude Desktop 注册表值 modelDiscoveryEnabled")?;
        }
        Ok(())
    }

    fn capture_snapshot() -> Result<RegistrySnapshot> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = match hkcu.open_subkey_with_flags(POLICY_PATH, KEY_READ) {
            Ok(value) => Some(value),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("无法读取 Claude Desktop 策略注册表"),
        };
        let mut values = BTreeMap::new();
        for name in MANAGED_VALUES {
            let value = match key.as_ref().map(|key| key.get_raw_value(name)) {
                Some(Ok(value)) => Some(StoredValue {
                    value_type: value.vtype as u32,
                    bytes: value.bytes,
                }),
                Some(Err(error)) if error.kind() == io::ErrorKind::NotFound => None,
                Some(Err(error)) => {
                    return Err(error)
                        .with_context(|| format!("无法读取 Claude Desktop 注册表值 {name}"));
                }
                None => None,
            };
            values.insert(name.to_owned(), value);
        }
        Ok(RegistrySnapshot {
            key_existed: key.is_some(),
            values,
        })
    }

    fn restore_snapshot(snapshot: &RegistrySnapshot) -> Result<()> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let needs_key = snapshot.key_existed || snapshot.values.values().any(Option::is_some);
        let key = if needs_key {
            Some(
                hkcu.create_subkey_with_flags(POLICY_PATH, KEY_READ | KEY_WRITE)
                    .context("无法打开 Claude Desktop 策略注册表以恢复配置")?
                    .0,
            )
        } else {
            match hkcu.open_subkey_with_flags(POLICY_PATH, KEY_READ | KEY_WRITE) {
                Ok(value) => Some(value),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error).context("无法打开 Claude Desktop 策略注册表"),
            }
        };

        if let Some(key) = key.as_ref() {
            for name in MANAGED_VALUES {
                match snapshot.values.get(name) {
                    Some(Some(stored)) => {
                        let raw = RegValue {
                            bytes: stored.bytes.clone(),
                            vtype: reg_type(stored.value_type)?,
                        };
                        key.set_raw_value(name, &raw)
                            .with_context(|| format!("无法恢复 Claude Desktop 注册表值 {name}"))?;
                    }
                    Some(None) => match key.delete_value(name) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(error)
                                .with_context(|| format!("无法删除临时注册表值 {name}"));
                        }
                    },
                    // 兼容旧版本生成的五值备份：备份中没有的新字段不能擅自删除。
                    None => continue,
                }
            }
        }

        if !snapshot.key_existed {
            let empty = key
                .as_ref()
                .map(|key| key.enum_values().next().is_none() && key.enum_keys().next().is_none())
                .unwrap_or(false);
            drop(key);
            if empty {
                match hkcu.delete_subkey(POLICY_PATH) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).context("无法删除原本不存在的空 Claude 策略键");
                    }
                }
            }
        }
        Ok(())
    }

    fn reg_type(value: u32) -> Result<RegType> {
        Ok(match value {
            0 => REG_NONE,
            1 => REG_SZ,
            2 => REG_EXPAND_SZ,
            3 => REG_BINARY,
            4 => REG_DWORD,
            5 => REG_DWORD_BIG_ENDIAN,
            6 => REG_LINK,
            7 => REG_MULTI_SZ,
            8 => REG_RESOURCE_LIST,
            9 => REG_FULL_RESOURCE_DESCRIPTOR,
            10 => REG_RESOURCE_REQUIREMENTS_LIST,
            11 => REG_QWORD,
            other => bail!("注册表备份包含不支持的值类型 {other}"),
        })
    }

    fn dpapi_protect(input: &[u8]) -> Result<Vec<u8>> {
        if input.len() > u32::MAX as usize {
            bail!("注册表备份过大");
        }
        let input_blob = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(
                &input_blob,
                PCWSTR::null(),
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .context("Windows DPAPI 无法加密 Claude Desktop 注册表备份")?;
        }
        copy_and_free(output)
    }

    fn dpapi_unprotect(input: &[u8]) -> Result<Vec<u8>> {
        if input.len() > u32::MAX as usize {
            bail!("注册表备份过大");
        }
        let input_blob = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptUnprotectData(
                &input_blob,
                None,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
            .context("Windows DPAPI 无法解密 Claude Desktop 注册表备份")?;
        }
        copy_and_free(output)
    }

    fn copy_and_free(blob: CRYPT_INTEGER_BLOB) -> Result<Vec<u8>> {
        if blob.cbData > 0 && blob.pbData.is_null() {
            bail!("Windows DPAPI 返回了无效数据");
        }
        let bytes = if blob.cbData == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec() }
        };
        if !blob.pbData.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(blob.pbData.cast())));
            }
        }
        Ok(bytes)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn snapshot_serialization_preserves_missing_and_raw_values() {
            let mut values = BTreeMap::new();
            values.insert("missing".into(), None);
            values.insert(
                "binary".into(),
                Some(StoredValue {
                    value_type: REG_BINARY as u32,
                    bytes: vec![0, 1, 2, 255],
                }),
            );
            let snapshot = RegistrySnapshot {
                key_existed: true,
                values,
            };
            let encoded = serde_json::to_vec(&snapshot).unwrap();
            let decoded: RegistrySnapshot = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded, snapshot);
        }

        #[test]
        fn dpapi_round_trip_is_scoped_to_current_windows_user() {
            let secret = b"test registry secret that must not be stored as plaintext";
            let encrypted = dpapi_protect(secret).unwrap();
            assert_ne!(encrypted, secret);
            assert_eq!(dpapi_unprotect(&encrypted).unwrap(), secret);
        }

        #[test]
        fn managed_values_include_model_discovery_switch() {
            assert_eq!(MANAGED_VALUES.len(), 6);
            assert!(MANAGED_VALUES.contains(&"modelDiscoveryEnabled"));
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use anyhow::{Result, bail};
    use std::path::PathBuf;

    pub const MANAGED_VALUES: [&str; 6] = [
        "inferenceGatewayBaseUrl",
        "inferenceGatewayApiKey",
        "inferenceProvider",
        "inferenceCredentialKind",
        "inferenceModels",
        "modelDiscoveryEnabled",
    ];

    #[derive(Debug, Clone)]
    pub struct RegistryStatus {
        pub policy_key_exists: bool,
        pub managed_values_present: Vec<String>,
        pub pending_backup: bool,
        pub backup_path: PathBuf,
    }

    pub fn status() -> Result<RegistryStatus> {
        bail!("当前平台不支持 Claude Desktop Windows 注册表配置")
    }
    pub fn backup_path() -> Result<PathBuf> {
        bail!("当前平台不支持 Claude Desktop Windows 注册表配置")
    }
    pub fn restore_pending() -> Result<bool> {
        bail!("当前平台不支持 Claude Desktop Windows 注册表配置")
    }
    pub fn capture_and_store() -> Result<()> {
        bail!("当前平台不支持 Claude Desktop Windows 注册表配置")
    }
    pub fn write_temporary_gateway(
        _base_url: &str,
        _local_token: &str,
        _inference_models: &str,
        _model_discovery_enabled: Option<bool>,
    ) -> Result<()> {
        bail!("当前平台不支持 Claude Desktop Windows 注册表配置")
    }
}

pub use imp::*;
