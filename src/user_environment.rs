use anyhow::Result;
use std::path::{Path, PathBuf};

#[cfg(test)]
pub(crate) static TEST_ENVIRONMENT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexHomeUpdate {
    pub previous: Option<PathBuf>,
    pub current: Option<PathBuf>,
    pub environment_broadcast: bool,
}

pub fn read_codex_home() -> Result<Option<PathBuf>> {
    platform::read_codex_home()
}

pub fn set_codex_home(value: Option<&Path>) -> Result<CodexHomeUpdate> {
    let previous = read_codex_home()?;
    platform::write_codex_home(value)?;
    let current = read_codex_home()?;
    let expected = value.map(Path::to_path_buf);
    if current != expected {
        anyhow::bail!(
            "设置用户级 CODEX_HOME 后回读不一致：期望 {}，实际 {}",
            display_optional(expected.as_deref()),
            display_optional(current.as_deref())
        );
    }
    Ok(CodexHomeUpdate {
        previous,
        current,
        environment_broadcast: platform::broadcast_environment_change()?,
    })
}

pub fn restore_codex_home(value: Option<&Path>) -> Result<CodexHomeUpdate> {
    set_codex_home(value)
}

pub fn codex_homes_match(left: &Path, right: &Path) -> bool {
    platform::codex_homes_match(left, right)
}

fn display_optional(value: Option<&Path>) -> String {
    value
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<unset>".to_string())
}

#[cfg(all(windows, not(test)))]
mod platform {
    use super::*;
    use anyhow::Context;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
    };
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
    use winreg::RegKey;

    pub fn read_codex_home() -> Result<Option<PathBuf>> {
        let current_user = RegKey::predef(HKEY_CURRENT_USER);
        let environment = match current_user.open_subkey_with_flags("Environment", KEY_READ) {
            Ok(environment) => environment,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("打开 HKCU\\Environment 失败"),
        };
        match environment.get_value::<String, _>("CODEX_HOME") {
            Ok(value) if value.trim().is_empty() => Ok(None),
            Ok(value) => Ok(Some(PathBuf::from(value))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("读取用户级 CODEX_HOME 失败"),
        }
    }

    pub fn write_codex_home(value: Option<&Path>) -> Result<()> {
        let current_user = RegKey::predef(HKEY_CURRENT_USER);
        let (environment, _) = current_user
            .create_subkey_with_flags("Environment", KEY_READ | KEY_WRITE)
            .context("打开 HKCU\\Environment 失败")?;
        match value {
            Some(path) => environment
                .set_value(
                    "CODEX_HOME",
                    &path.as_os_str().to_string_lossy().into_owned(),
                )
                .context("设置用户级 CODEX_HOME 失败"),
            None => match environment.delete_value("CODEX_HOME") {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error).context("删除旧版 Connector 留下的用户级 CODEX_HOME 失败"),
            },
        }
    }

    pub fn broadcast_environment_change() -> Result<bool> {
        let environment = std::ffi::OsStr::new("Environment")
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut result = 0usize;
        let sent = unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                environment.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                5_000,
                &mut result,
            )
        };
        if sent == 0 {
            anyhow::bail!("用户级 CODEX_HOME 已写入，但广播 WM_SETTINGCHANGE 失败")
        }
        Ok(true)
    }

    pub fn codex_homes_match(left: &Path, right: &Path) -> bool {
        normalize_windows_path(left) == normalize_windows_path(right)
    }

    fn normalize_windows_path(path: &Path) -> String {
        path.as_os_str()
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    }
}

#[cfg(any(not(windows), test))]
mod platform {
    use super::*;

    pub fn read_codex_home() -> Result<Option<PathBuf>> {
        Ok(std::env::var_os("CODEX_HOME").map(PathBuf::from))
    }

    pub fn write_codex_home(value: Option<&Path>) -> Result<()> {
        match value {
            Some(path) => std::env::set_var("CODEX_HOME", path),
            None => std::env::remove_var("CODEX_HOME"),
        }
        Ok(())
    }

    pub fn broadcast_environment_change() -> Result<bool> {
        Ok(false)
    }

    pub fn codex_homes_match(left: &Path, right: &Path) -> bool {
        left == right
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn explicit_update_changes_and_reads_back_codex_home() {
        let _guard = TEST_ENVIRONMENT_LOCK.lock().unwrap();
        let previous = std::env::var_os("CODEX_HOME");
        let managed = std::env::temp_dir().join("baijimu-managed-codex-home");
        let original = std::env::temp_dir().join("user-original-codex-home");
        std::env::set_var("CODEX_HOME", &managed);

        let restored = set_codex_home(Some(&original)).unwrap();

        assert_eq!(restored.previous, Some(managed));
        assert_eq!(restored.current, Some(original.clone()));
        assert_eq!(
            std::env::var_os("CODEX_HOME"),
            Some(original.into_os_string())
        );
        assert!(!restored.environment_broadcast);
        restore("CODEX_HOME", previous);
    }

    fn restore(key: &str, value: Option<OsString>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}
