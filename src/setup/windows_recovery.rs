use super::{contract::PackageRecovery, SetupStatus};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

// Resolve only the installer's recorded artifact, never a path supplied by the UI.
pub(super) fn recovery_package(
    status: &SetupStatus,
    state_dir: &Path,
    require_elevation: bool,
) -> Result<PathBuf> {
    if !matches!(status.status.as_str(), "failed" | "interrupted") {
        anyhow::bail!("当前没有可恢复的失败安装");
    }
    let installer = status
        .installer_status
        .as_ref()
        .filter(|installer| installer.platform == "windows")
        .context("当前没有 Windows 安装包记录")?;
    let recovery = installer
        .package_recovery
        .as_ref()
        .context("尚未下载并校验安装包")?;
    if require_elevation && !recovery.requires_elevation {
        anyhow::bail!("当前安装失败不需要管理员权限，请使用普通重试");
    }
    let receipt: PackageRecovery = super::read_json(state_dir.join("package-recovery.json"))
        .context("安装包记录已丢失，请重新安装并修复")?;
    if receipt.package_path != recovery.package_path || receipt.sha256 != recovery.sha256 {
        anyhow::bail!("安装包记录已改变，请刷新后重试");
    }
    validate_package_path(recovery, state_dir)
}

fn validate_package_path(recovery: &PackageRecovery, state_dir: &Path) -> Result<PathBuf> {
    if !Path::new(&recovery.package_path).is_absolute()
        || recovery.sha256.len() != 64
        || !recovery.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        anyhow::bail!("安装包校验记录无效，请重新安装并修复");
    }
    let root = state_dir
        .join("packages")
        .join(&recovery.sha256)
        .canonicalize()
        .context("安装包目录已丢失，请重新安装并修复")?;
    let path = Path::new(&recovery.package_path)
        .canonicalize()
        .context("安装包已丢失，请重新安装并修复")?;
    if path.parent() != Some(root.as_path())
        || !path.is_file()
        || path.extension().and_then(|ext| ext.to_str()) != Some("msix")
    {
        anyhow::bail!("安装包路径不属于当前安装记录");
    }
    // Explorer expects a normal absolute path, not canonicalize's Windows verbatim prefix.
    Ok(PathBuf::from(&recovery.package_path))
}

pub(super) fn reveal(path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        // One argument preserves spaces, commas and Unicode in the selected file path.
        let mut argument = std::ffi::OsString::from("/select,");
        argument.push(path);
        std::process::Command::new("explorer.exe")
            .arg(argument)
            .spawn()
            .context("打开安装包所在目录失败")?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        anyhow::bail!("此操作仅支持 Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recovery_rejects_paths_outside_the_recorded_download() {
        let root = std::env::temp_dir().join(format!(
            "msix-recovery-{}-{}",
            std::process::id(),
            super::super::now_epoch_seconds()
        ));
        let hash = "a".repeat(64);
        let packages = root.join("packages").join(&hash);
        std::fs::create_dir_all(&packages).unwrap();
        let file = packages.join("包含 空格's.msix");
        std::fs::write(&file, b"test").unwrap();
        let mut recovery = PackageRecovery {
            package_path: file.display().to_string(),
            sha256: hash,
            requires_elevation: true,
        };
        assert_eq!(validate_package_path(&recovery, &root).unwrap(), file);
        let mut status = SetupStatus {
            status: "failed".into(),
            installer_status: Some(
                serde_json::from_value(serde_json::json!({
                    "title": "installer", "locale": "zh-CN", "platform": "windows",
                    "startedAt": "", "updatedAt": "", "currentStep": 5,
                    "statusPath": "", "resultPath": "", "steps": [],
                    "packageRecovery": recovery,
                }))
                .unwrap(),
            ),
            ..SetupStatus::default()
        };
        std::fs::write(
            root.join("package-recovery.json"),
            serde_json::to_vec(&recovery).unwrap(),
        )
        .unwrap();
        assert!(recovery_package(&status, &root, true).is_ok());
        status.status = "running".into();
        assert!(recovery_package(&status, &root, true).is_err());
        status.status = "failed".into();
        status
            .installer_status
            .as_mut()
            .unwrap()
            .package_recovery
            .as_mut()
            .unwrap()
            .requires_elevation = false;
        assert!(recovery_package(&status, &root, true).is_err());
        assert!(recovery_package(&status, &root, false).is_ok());
        std::fs::remove_file(root.join("package-recovery.json")).unwrap();
        assert!(recovery_package(&status, &root, false).is_err());
        let outside = root.join("other.msix");
        std::fs::write(&outside, b"test").unwrap();
        recovery.package_path = outside.display().to_string();
        assert!(validate_package_path(&recovery, &root).is_err());
        recovery.sha256 = "../outside".into();
        assert!(validate_package_path(&recovery, &root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
