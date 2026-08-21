use super::*;

pub(super) fn load_metadata() -> Result<CredentialMetadata> {
    let path = metadata_path();
    let (source, remove_after_import) = if path.exists() {
        (Some(path.clone()), false)
    } else if legacy_metadata_path().exists() {
        (Some(legacy_metadata_path()), true)
    } else {
        (None, false)
    };
    let mut metadata = if let Some(source) = source.as_ref() {
        let content = fs::read(source)
            .with_context(|| format!("读取 Codex 凭证元数据失败: {}", source.display()))?;
        crate::json_compat::from_slice::<CredentialMetadata>(&content)
            .with_context(|| format!("解析 Codex 凭证元数据失败: {}", source.display()))?
    } else {
        CredentialMetadata::default()
    };
    let previous_version = source.as_ref().map(|_| metadata.version).unwrap_or(0);
    let needs_version_migration = previous_version < METADATA_VERSION;
    for profile in &mut metadata.profiles {
        normalize_profile(profile);
    }
    let profile_default_models_migrated =
        migrate_profile_default_models(&mut metadata, previous_version);
    let legacy_profile_homes_migrated = migrate_profiles_to_shared_home(&mut metadata)?;
    if previous_version < 2 && metadata.active_profile_id.is_none() {
        metadata.active_profile_id = metadata.active_workspace_id.and_then(|id| {
            metadata
                .profiles
                .iter()
                .find(|p| p.workspace_id == id)
                .map(|p| p.profile_id.clone())
        });
        if metadata.active_profile_id.is_some() {
            metadata.active_mode = AuthMode::Baijimu;
        }
    }
    let baseline_captured = capture_original_codex_home(&mut metadata)?;
    let original_auth_captured = capture_original_auth_profile(&mut metadata)?;
    let chatgpt_profile_created = ensure_chatgpt_profile(&mut metadata)?;
    // Migration and status reads must never reactivate archived credentials into
    // the shared Codex home. They only reconcile connector metadata to live files.
    let active_profile_reconciled = reconcile_active_profile_from_shared_home(&mut metadata)?;
    metadata.version = METADATA_VERSION;
    if source.as_ref() != Some(&path)
        || needs_version_migration
        || profile_default_models_migrated
        || baseline_captured
        || legacy_profile_homes_migrated
        || original_auth_captured
        || chatgpt_profile_created
        || active_profile_reconciled
    {
        save_metadata(&metadata)?;
    }
    if remove_after_import {
        let source = source.expect("legacy source exists when cleanup is requested");
        fs::remove_file(&source)
            .with_context(|| format!("清理旧版元数据失败: {}", source.display()))?;
    }
    Ok(metadata)
}
pub(super) fn save_metadata(metadata: &CredentialMetadata) -> Result<()> {
    atomic_write_private(&metadata_path(), &serde_json::to_vec_pretty(metadata)?)?;
    verify_private_file(&metadata_path())
}
pub(super) fn legacy_config_dir() -> PathBuf {
    if let Some(config_home) = std::env::var_os("BAIJIMU_CONFIG_HOME") {
        return PathBuf::from(config_home).join("baijimu");
    }
    home_dir().join(".config").join("baijimu")
}

pub(super) fn metadata_path() -> PathBuf {
    connector_data_dir().join(METADATA_FILE)
}
pub(super) fn legacy_metadata_path() -> PathBuf {
    legacy_config_dir().join(METADATA_FILE)
}
pub(super) fn connector_data_dir() -> PathBuf {
    std::env::var_os("BAIJIMU_CONNECTOR_DATA_DIR")
        .or_else(|| std::env::var_os("CODEX_DESKTOP_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".baijimu-connector-codex"))
}
pub(super) fn home_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
pub(super) fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

pub(super) fn atomic_write_private(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
        set_private_directory(parent)?;
    }
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let temp = path.with_extension(format!("tmp-{}-{unique}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .with_context(|| format!("创建临时文件失败: {}", temp.display()))?;
    use std::io::Write;
    file.write_all(content)
        .with_context(|| format!("写入临时文件失败: {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("同步临时文件失败: {}", temp.display()))?;
    drop(file);
    set_private_file(&temp)?;
    if let Err(error) = replace_file(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    set_private_file(path)?;
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(temp: &Path, path: &Path) -> Result<()> {
    fs::rename(temp, path).with_context(|| format!("替换文件失败: {}", path.display()))
}

#[cfg(windows)]
fn replace_file(temp: &Path, path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = temp
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("原子替换文件失败: {}", path.display()));
    }
    Ok(())
}
pub(super) fn verify_private_file(path: &Path) -> Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("回读文件失败: {}", path.display()))?;
    if !metadata.is_file() || metadata.len() == 0 {
        anyhow::bail!("文件为空或不是普通文件: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            anyhow::bail!("文件权限不是 600: {}", path.display());
        }
    }
    Ok(())
}
pub(super) fn set_private_directory(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub(super) fn set_private_file(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
