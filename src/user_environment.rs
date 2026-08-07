use anyhow::Result;
use std::path::{Path, PathBuf};

#[cfg(test)]
pub(crate) static TEST_ENVIRONMENT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
static FAIL_NEXT_ACTIVATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexHomeActivation {
    pub previous: Option<PathBuf>,
    pub current: Option<PathBuf>,
    pub persisted_for_desktop: bool,
    pub environment_broadcast: bool,
}

pub fn read_codex_home() -> Result<Option<PathBuf>> {
    platform::read_codex_home()
}

pub fn persisted_for_desktop() -> bool {
    platform::PERSISTED_FOR_DESKTOP
}

pub fn activate_codex_home(value: Option<&Path>) -> Result<CodexHomeActivation> {
    let previous = read_codex_home()?;
    platform::write_codex_home(value)?;
    match value {
        Some(path) => std::env::set_var("CODEX_HOME", path),
        None => std::env::remove_var("CODEX_HOME"),
    }
    let current = read_codex_home()?;
    let expected = value.map(Path::to_path_buf);
    if current != expected {
        anyhow::bail!(
            "用户级 CODEX_HOME 回读不一致：期望 {}，实际 {}",
            display_optional(expected.as_deref()),
            display_optional(current.as_deref())
        );
    }
    #[cfg(test)]
    if FAIL_NEXT_ACTIVATION.swap(false, std::sync::atomic::Ordering::SeqCst) {
        anyhow::bail!("injected CODEX_HOME activation failure");
    }
    Ok(CodexHomeActivation {
        previous,
        current,
        persisted_for_desktop: platform::PERSISTED_FOR_DESKTOP,
        environment_broadcast: platform::broadcast_environment_change()?,
    })
}

#[cfg(test)]
pub(crate) fn fail_next_activation() {
    FAIL_NEXT_ACTIVATION.store(true, std::sync::atomic::Ordering::SeqCst);
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

    pub const PERSISTED_FOR_DESKTOP: bool = true;

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
            Some(path) => {
                let value = path.as_os_str().to_string_lossy().into_owned();
                environment
                    .set_value("CODEX_HOME", &value)
                    .context("写入用户级 CODEX_HOME 失败")
            }
            None => match environment.delete_value("CODEX_HOME") {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error).context("删除用户级 CODEX_HOME 失败"),
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
            anyhow::bail!("用户级 CODEX_HOME 已写入，但广播 WM_SETTINGCHANGE 失败");
        }
        Ok(true)
    }
}

#[cfg(any(not(windows), test))]
mod platform {
    use super::*;

    pub const PERSISTED_FOR_DESKTOP: bool = false;

    pub fn read_codex_home() -> Result<Option<PathBuf>> {
        Ok(std::env::var_os("CODEX_HOME").map(PathBuf::from))
    }

    pub fn write_codex_home(_value: Option<&Path>) -> Result<()> {
        Ok(())
    }

    pub fn broadcast_environment_change() -> Result<bool> {
        Ok(false)
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[cfg(not(windows))]
    #[test]
    fn non_windows_activation_updates_process_environment() {
        let _guard = TEST_ENVIRONMENT_LOCK.lock().unwrap();
        let previous = std::env::var_os("CODEX_HOME");
        let target = std::env::temp_dir().join("baijimu-user-environment-test");
        let activation = activate_codex_home(Some(&target)).unwrap();
        assert_eq!(activation.current, Some(target.clone()));
        assert_eq!(
            std::env::var_os("CODEX_HOME"),
            Some(target.into_os_string())
        );
        restore("CODEX_HOME", previous);
    }

    fn restore(key: &str, value: Option<OsString>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
}
