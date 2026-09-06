use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BACKUP_SETS: usize = 20;

pub fn backup_files(root: &Path, files: &[&Path]) -> Result<Option<PathBuf>> {
    let existing: Vec<&Path> = files.iter().copied().filter(|path| path.exists()).collect();
    if existing.is_empty() {
        return Ok(None);
    }

    let stamp = timestamp();
    let backup_root = root.join("znnz-backups").join(stamp);
    fs::create_dir_all(&backup_root)
        .with_context(|| format!("无法创建备份目录 {}", backup_root.display()))?;

    for source in existing {
        let relative = source
            .strip_prefix(root)
            .unwrap_or_else(|_| source.file_name().map(Path::new).unwrap_or(source));
        let target = backup_root.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, &target)
            .with_context(|| format!("备份失败: {} -> {}", source.display(), target.display()))?;
    }
    if let Err(error) = prune_old_backups(backup_root.parent().unwrap_or(root)) {
        tracing::warn!("清理旧配置备份失败: {error:#}");
    }
    Ok(Some(backup_root))
}

fn prune_old_backups(backups_root: &Path) -> Result<()> {
    let mut backups = Vec::new();
    for entry in fs::read_dir(backups_root)
        .with_context(|| format!("无法读取备份目录 {}", backups_root.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(stamp) = parse_backup_stamp(name) else {
            continue;
        };
        backups.push((stamp, entry.path()));
    }

    backups.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, path) in backups.into_iter().skip(MAX_BACKUP_SETS) {
        fs::remove_dir_all(&path)
            .with_context(|| format!("无法删除旧备份目录 {}", path.display()))?;
    }
    Ok(())
}

fn parse_backup_stamp(name: &str) -> Option<(u64, u16)> {
    let (seconds, millis) = name.split_once('-')?;
    if seconds.is_empty()
        || millis.len() != 3
        || !seconds.bytes().all(|byte| byte.is_ascii_digit())
        || !millis.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some((seconds.parse().ok()?, millis.parse().ok()?))
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("目标文件没有父目录: {}", path.display()))?;
    fs::create_dir_all(parent)?;

    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("znnz.tmp");
    let nonce: u64 = rand::random();
    let temp = parent.join(format!(
        ".{file_name}.znnz-tmp-{}-{nonce:016x}",
        std::process::id()
    ));
    let write_result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("无法创建临时文件 {}", temp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("无法写入临时文件 {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("无法同步临时文件 {}", temp.display()))?;
        replace_file(&temp, path)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result
}

#[cfg(windows)]
fn replace_file(temp: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let temp_wide: Vec<u16> = temp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let target_wide: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(temp_wide.as_ptr()),
            PCWSTR(target_wide.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .with_context(|| format!("无法原子替换 {} -> {}", temp.display(), target.display()))?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(temp: &Path, target: &Path) -> Result<()> {
    fs::rename(temp, target)
        .with_context(|| format!("无法原子替换 {} -> {}", temp.display(), target.display()))
}

pub fn remove_file_if_exists(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    if path.is_dir() {
        bail!("拒绝删除目录，预期应为缓存文件: {}", path.display());
    }
    fs::remove_file(path).with_context(|| format!("无法删除缓存 {}", path.display()))?;
    Ok(true)
}

fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}-{:03}", now.as_secs(), now.subsec_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "znnz-client-{label}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn atomic_write_replaces_contents_without_leaving_temp_files() {
        let root = test_root("atomic-write");
        let target = root.join("nested/config.toml");
        atomic_write(&target, b"old").unwrap();
        atomic_write(&target, b"new contents").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new contents");
        let leftovers = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains("znnz-tmp"))
            .count();
        assert_eq!(leftovers, 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn backup_preserves_relative_paths_and_original_files() {
        let root = test_root("backup");
        let first = root.join("config.toml");
        let second = root.join("model-catalogs/znnz-net.json");
        fs::create_dir_all(second.parent().unwrap()).unwrap();
        fs::write(&first, "config").unwrap();
        fs::write(&second, "catalog").unwrap();
        let backup = backup_files(&root, &[&first, &second]).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(backup.join("config.toml")).unwrap(),
            "config"
        );
        assert_eq!(
            fs::read_to_string(backup.join("model-catalogs/znnz-net.json")).unwrap(),
            "catalog"
        );
        assert_eq!(fs::read_to_string(&first).unwrap(), "config");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn backup_pruning_keeps_latest_twenty_and_unrelated_directories() {
        let root = test_root("backup-pruning");
        let backups_root = root.join("znnz-backups");
        fs::create_dir_all(&backups_root).unwrap();
        for index in 0..23 {
            fs::create_dir(backups_root.join(format!("1700000000-{index:03}"))).unwrap();
        }
        fs::create_dir(backups_root.join("keep-me")).unwrap();

        prune_old_backups(&backups_root).unwrap();

        for index in 0..3 {
            assert!(!backups_root.join(format!("1700000000-{index:03}")).exists());
        }
        for index in 3..23 {
            assert!(backups_root.join(format!("1700000000-{index:03}")).is_dir());
        }
        assert!(backups_root.join("keep-me").is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn backup_stamp_parser_rejects_unmanaged_names() {
        assert_eq!(parse_backup_stamp("1700000000-042"), Some((1700000000, 42)));
        assert_eq!(parse_backup_stamp("1700000000-42"), None);
        assert_eq!(parse_backup_stamp("notes-042"), None);
        assert_eq!(parse_backup_stamp("1700000000-042-extra"), None);
    }

    #[test]
    fn cache_removal_refuses_directories() {
        let root = test_root("remove-directory");
        let directory = root.join("models_cache.json");
        fs::create_dir_all(&directory).unwrap();
        assert!(remove_file_if_exists(&directory).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
