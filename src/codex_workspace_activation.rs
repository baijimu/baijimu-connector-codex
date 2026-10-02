use anyhow::Result;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

#[cfg(windows)]
use crate::user_environment;
use crate::{codex_workspace, desktop};

pub fn switch(workspace_id: &str, manage_desktop: bool) -> Result<()> {
    let previous = codex_workspace::active()?;
    let _target = codex_workspace::workspace(workspace_id)?;

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let desktop_switch = if manage_desktop {
        Some(desktop::stop_for_workspace_switch()?)
    } else {
        None
    };

    #[cfg(windows)]
    let previous_environment = user_environment::read_codex_home()?;
    let commit_result = (|| -> Result<()> {
        let _write = crate::state_access::write()?;
        #[cfg(windows)]
        let previous_user_codex_home = if manage_desktop {
            match project_windows_codex_home(Path::new(&_target.codex_home)) {
                Ok(previous_home) => Some(previous_home),
                Err(error) => {
                    anyhow::bail!("切换 CODEX_HOME 失败：{error}");
                }
            }
        } else {
            None
        };

        if let Err(error) = codex_workspace::activate(workspace_id) {
            #[allow(unused_mut)]
            let mut message = format!("切换 Codex 工作区目录失败：{error}");
            #[cfg(windows)]
            if let Some(previous_user_codex_home) = previous_user_codex_home.as_ref() {
                if let Err(rollback_error) =
                    user_environment::set_codex_home(previous_user_codex_home.as_deref())
                {
                    message.push_str(&format!("；恢复原 CODEX_HOME 失败：{rollback_error}"));
                }
            }
            anyhow::bail!(message);
        }

        Ok(())
    })();
    #[cfg(windows)]
    let commit_result = commit_result.and_then(|_| {
        if !manage_desktop {
            return Ok(());
        }
        if let Err(error) = user_environment::notify_environment_change() {
            let mut message = format!("通知桌面进程更新 CODEX_HOME 失败：{error}");
            {
                let _write = crate::state_access::write()?;
                if let Err(rollback) =
                    user_environment::set_codex_home(previous_environment.as_deref())
                {
                    message.push_str(&format!("；恢复原环境失败：{rollback}"));
                }
                if let Err(rollback) = codex_workspace::activate(&previous.workspace_id) {
                    message.push_str(&format!("；恢复原工作区失败：{rollback}"));
                }
            }
            if let Err(rollback) = user_environment::notify_environment_change() {
                message.push_str(&format!("；通知恢复后的环境失败：{rollback}"));
            }
            anyhow::bail!(message);
        }
        Ok(())
    });
    if let Err(error) = commit_result {
        #[allow(unused_mut)]
        let mut message = error.to_string();
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(desktop_switch) = desktop_switch.as_ref() {
            if let Err(restart_error) =
                desktop_switch.restart_workspace_if_needed(Path::new(&previous.codex_home))
            {
                message.push_str(&format!("；恢复原工作区桌面进程失败：{restart_error}"));
            }
        }
        anyhow::bail!(message);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = (previous, _target, manage_desktop);

    Ok(())
}

#[cfg(windows)]
fn project_windows_codex_home(target: &Path) -> Result<Option<PathBuf>> {
    let previous = user_environment::read_codex_home()?;
    if let Err(error) = user_environment::set_codex_home(Some(target)) {
        if let Err(rollback_error) = user_environment::set_codex_home(previous.as_deref()) {
            anyhow::bail!("{error}；恢复原 CODEX_HOME 失败：{rollback_error}");
        }
        return Err(error);
    }
    Ok(previous)
}
