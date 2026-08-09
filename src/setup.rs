#[cfg(target_os = "windows")]
use crate::desktop;
use crate::{codex_binary, credential};
use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
const MACOS_SCRIPT_URL: &str =
    "https://download.baijimu.com/docs/scripts/codex-device-install/macos-configure-terminal-and-login.sh?versionId=CAEQogIYgYCAmezf6f4ZIiBmMDQ3MWU4ZDVhYTY0ZjQxYmEzOTA3MTU0NDlmNmE5Nw--";
#[cfg(target_os = "windows")]
const WINDOWS_SCRIPT_URL: &str =
    "https://download.baijimu.com/docs/scripts/codex-device-install/windows-configure-terminal-and-login.ps1?versionId=CAEQogIYgYCAuuvr_f4ZIiAyZjRkYmYyYTdmOGI0YmQ2OGIxZDU3NThkYzcwOGExMQ--";
const SETUP_STATUS_FILE: &str = "setup-status.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupStatus {
    pub status: String,
    pub workspace_id: Option<u64>,
    pub message: String,
    pub error: Option<String>,
    pub started_at_epoch_seconds: Option<u64>,
    pub completed_at_epoch_seconds: Option<u64>,
    pub installer_status: Option<Value>,
}

impl Default for SetupStatus {
    fn default() -> Self {
        Self {
            status: "pending".to_string(),
            workspace_id: None,
            message: "等待初始化".to_string(),
            error: None,
            started_at_epoch_seconds: None,
            completed_at_epoch_seconds: None,
            installer_status: None,
        }
    }
}

#[derive(Clone)]
pub struct SetupManager {
    state: Arc<Mutex<SetupStatus>>,
}

impl SetupManager {
    pub fn load() -> Self {
        let status = fs::read_to_string(status_path())
            .ok()
            .and_then(|content| serde_json::from_str::<SetupStatus>(&content).ok())
            .map(|mut status| {
                if status.status == "running" {
                    status.status = "failed".to_string();
                    status.error = Some("连接器在初始化过程中退出，请重试".to_string());
                    status.message = "上次初始化未完成".to_string();
                    status.completed_at_epoch_seconds = Some(now_epoch_seconds());
                }
                status
            })
            .unwrap_or_default();
        let manager = Self {
            state: Arc::new(Mutex::new(status)),
        };
        let _ = manager.persist();
        manager
    }

    pub fn state(&self) -> SetupStatus {
        let mut status = self
            .state
            .lock()
            .map(|value| value.clone())
            .unwrap_or_else(|_| SetupStatus {
                status: "failed".to_string(),
                error: Some("初始化状态锁异常".to_string()),
                ..SetupStatus::default()
            });
        status.installer_status = read_json(installer_state_dir().join("status.json"));
        status
    }

    pub fn start(
        &self,
        workspace_id: u64,
        codex_cli: Option<PathBuf>,
        force: bool,
        verify_app_server_capability: bool,
    ) -> Result<SetupStatus> {
        if workspace_id == 0 {
            anyhow::bail!("workspaceId must be a positive integer");
        }
        {
            let current = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("初始化状态锁异常"))?;
            if current.status == "running" {
                if current.workspace_id == Some(workspace_id) {
                    return Ok(current.clone());
                }
                anyhow::bail!("另一个工作区的初始化正在进行");
            }
            if !force
                && codex_cli.is_some()
                && current.status == "succeeded"
                && current.workspace_id == Some(workspace_id)
                && credential::codex_ready_for_workspace(workspace_id)
            {
                return Ok(current.clone());
            }
        }

        let running = SetupStatus {
            status: "running".to_string(),
            workspace_id: Some(workspace_id),
            message: "正在初始化 Codex 应用".to_string(),
            error: None,
            started_at_epoch_seconds: Some(now_epoch_seconds()),
            completed_at_epoch_seconds: None,
            installer_status: None,
        };
        self.replace(running.clone())?;

        let manager = self.clone();
        thread::spawn(move || {
            let completed = match run_install(
                workspace_id,
                codex_cli.as_deref(),
                verify_app_server_capability,
            ) {
                Ok(()) => SetupStatus {
                    status: "succeeded".to_string(),
                    workspace_id: Some(workspace_id),
                    message: "Codex 应用初始化已完成".to_string(),
                    error: None,
                    started_at_epoch_seconds: running.started_at_epoch_seconds,
                    completed_at_epoch_seconds: Some(now_epoch_seconds()),
                    installer_status: None,
                },
                Err(error) => SetupStatus {
                    status: "failed".to_string(),
                    workspace_id: Some(workspace_id),
                    message: "Codex 应用初始化失败".to_string(),
                    error: Some(compact_error(&error.to_string())),
                    started_at_epoch_seconds: running.started_at_epoch_seconds,
                    completed_at_epoch_seconds: Some(now_epoch_seconds()),
                    installer_status: None,
                },
            };
            let _ = manager.replace(completed);
        });
        Ok(running)
    }

    fn replace(&self, status: SetupStatus) -> Result<()> {
        *self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("初始化状态锁异常"))? = status;
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let status = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("初始化状态锁异常"))?
            .clone();
        atomic_write_private(&status_path(), &serde_json::to_vec_pretty(&status)?)
    }
}

fn run_install(
    workspace_id: u64,
    codex_cli: Option<&Path>,
    verify_app_server_capability: bool,
) -> Result<()> {
    let setup_dir = connector_home().join("setup");
    fs::create_dir_all(&setup_dir)
        .with_context(|| format!("创建安装目录失败: {}", setup_dir.display()))?;
    set_private_directory(&setup_dir)?;
    let unique = format!("{}-{}", std::process::id(), now_epoch_seconds());
    let secret_path = setup_dir.join(format!("credential-{unique}"));
    let script_path = install_script_path(&setup_dir, &unique)?;
    let auto_activate = credential::should_auto_activate_workspace_after_setup()?;

    let install_result = (|| -> Result<()> {
        let prepared = credential::prepare_workspace_profile(workspace_id)?;
        let profile_home = PathBuf::from(&prepared.profile.codex_home);
        #[cfg(target_os = "windows")]
        let active_home_snapshot = credential::active_home_snapshot()
            .context("保存 ChatGPT/Codex 桌面应用启动前的环境状态失败")?;
        atomic_write_private(&secret_path, prepared.credential.as_bytes())?;

        let script_url = env::var("CODEX_CONNECTOR_INSTALL_SCRIPT_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(default_script_url);
        download_script(&script_url, &script_path)?;
        let state_dir = installer_state_dir();
        fs::create_dir_all(&state_dir)?;
        set_private_directory(&state_dir)?;
        let _ = fs::remove_file(state_dir.join("status.json"));
        let _ = fs::remove_file(state_dir.join("result.json"));

        let mut command = install_command(&script_path)?;
        command
            .env("CODEX_WORKSPACE_ID", workspace_id.to_string())
            .env("CODEX_LLM_CREDENTIAL_FILE", &secret_path)
            .env("CODEX_INSTALL_STATE_DIR", &state_dir)
            .env("CODEX_INSTALL_QUIET", "1")
            .env("CODEX_HOME", &profile_home)
            .env("CODEX_INSTALL_SKIP_DESKTOP_RESTART", "1")
            .env_remove("CODEX_PROJECT_ID")
            .env_remove("BAIJIMU_PROJECT_ID")
            .env_remove("PROJECT_ID");
        if let Some(codex_cli) = codex_cli {
            command.env("CODEX_CLI_BIN", codex_cli);
        } else {
            command.env_remove("CODEX_CLI_BIN");
        }
        let output = command.output().context("启动 Codex 官方安装脚本失败")?;
        let installer_result_path = state_dir.join("result.json");
        let installer_result = read_json(&installer_result_path).with_context(|| {
            let exit = output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let stderr = compact_error(&String::from_utf8_lossy(&output.stderr));
            format!(
                "安装脚本没有生成结果文件: {}（exit={exit}，stderr={}）",
                installer_result_path.display(),
                if stderr.is_empty() {
                    "<empty>"
                } else {
                    &stderr
                }
            )
        })?;
        if !output.status.success()
            || installer_result.get("ok").and_then(Value::as_bool) != Some(true)
        {
            let errors = installer_result
                .get("errors")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("；")
                })
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| String::from_utf8_lossy(&output.stderr).to_string());
            anyhow::bail!("官方安装脚本执行失败: {}", compact_error(&errors));
        }
        credential::finalize_workspace_setup(&prepared.profile, auto_activate)?;
        #[cfg(target_os = "windows")]
        if let Err(error) = desktop::launch_and_verify() {
            let rollback = credential::restore_active_home(active_home_snapshot);
            let mut message =
                format!("工作区配置已完成，但自动打开 ChatGPT/Codex 桌面应用失败：{error}");
            if let Err(rollback) = rollback {
                message.push_str(&format!("；用户级 CODEX_HOME 回滚失败：{rollback}"));
            }
            anyhow::bail!(message);
        }
        Ok(())
    })();
    let _ = fs::remove_file(&secret_path);
    let _ = fs::remove_file(&script_path);
    install_result?;

    if !credential::codex_ready_for_workspace(workspace_id) {
        anyhow::bail!("安装脚本执行成功，但独立工作区凭证归属回查失败");
    }
    let requested = codex_cli.map(|path| path.to_string_lossy().into_owned());
    let resolution = codex_binary::resolve(requested.as_deref())
        .map_err(|error| anyhow::anyhow!("安装脚本执行成功，但 Codex CLI 回查失败：{error}"))?;
    if verify_app_server_capability {
        let inspection = codex_binary::inspect(&resolution);
        if !inspection.app_server_supported {
            anyhow::bail!(
                "安装脚本执行成功，但 Codex CLI 不支持 app-server：{}",
                inspection
                    .error
                    .unwrap_or_else(|| "能力检查失败".to_string())
            );
        }
    }
    Ok(())
}

fn install_script_path(setup_dir: &Path, unique: &str) -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Ok(setup_dir.join(format!("install-{unique}.sh")))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(setup_dir.join(format!("install-{unique}.ps1")))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (setup_dir, unique);
        anyhow::bail!("Codex 一键安装目前只支持 macOS 和 Windows")
    }
}

fn download_script(url: &str, path: &Path) -> Result<()> {
    let response = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(90))
        .build()?
        .get(url)
        .send()
        .with_context(|| format!("下载安装脚本失败: {url}"))?;
    if !response.status().is_success() {
        anyhow::bail!("下载安装脚本失败: HTTP {}", response.status());
    }
    let bytes = response.bytes().context("读取安装脚本失败")?;
    if bytes.len() < 1_000 {
        anyhow::bail!("下载的安装脚本内容异常");
    }
    atomic_write_private(path, &bytes)
}

fn install_command(script_path: &Path) -> Result<Command> {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("/bin/bash");
        command.arg(script_path);
        Ok(command)
    }
    #[cfg(target_os = "windows")]
    {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
        command.arg(script_path);
        Ok(command)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = script_path;
        anyhow::bail!("unsupported platform")
    }
}

fn default_script_url() -> String {
    #[cfg(target_os = "macos")]
    {
        MACOS_SCRIPT_URL.to_string()
    }
    #[cfg(target_os = "windows")]
    {
        WINDOWS_SCRIPT_URL.to_string()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        String::new()
    }
}

fn connector_home() -> PathBuf {
    env::var_os("BAIJIMU_CONNECTOR_DATA_DIR")
        .or_else(|| env::var_os("CODEX_CONNECTOR_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".baijimu-connector-codex"))
}

fn installer_state_dir() -> PathBuf {
    connector_home().join("codex-install-state")
}

fn status_path() -> PathBuf {
    connector_home().join(SETUP_STATUS_FILE)
}

fn home_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(profile) = env::var_os("USERPROFILE") {
        return PathBuf::from(profile);
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_json(path: impl AsRef<Path>) -> Option<Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str(content.trim_start_matches('\u{feff}')).ok())
}

fn atomic_write_private(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_private_directory(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, content)?;
    set_private_file(&temporary)?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&temporary, path)?;
    set_private_file(path)
}

fn set_private_directory(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(_path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn set_private_file(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(_path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn compact_error(error: &str) -> String {
    error
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(1_000)
        .collect()
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn default_macos_installer_is_pinned_to_an_immutable_oss_version() {
        assert!(MACOS_SCRIPT_URL.starts_with("https://download.baijimu.com/"));
        assert!(MACOS_SCRIPT_URL.contains("?versionId="));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn default_windows_installer_is_pinned_to_an_immutable_oss_version() {
        assert!(WINDOWS_SCRIPT_URL.starts_with("https://download.baijimu.com/"));
        assert!(WINDOWS_SCRIPT_URL.contains("?versionId="));
    }

    #[test]
    fn reads_windows_powershell_json_with_utf8_bom() {
        let path = env::temp_dir().join(format!(
            "codex-setup-result-{}-{}.json",
            std::process::id(),
            now_epoch_seconds()
        ));
        fs::write(&path, "\u{feff}{\"ok\":true,\"projectId\":null}").unwrap();
        let value = read_json(&path).unwrap();
        assert_eq!(value.get("ok").and_then(Value::as_bool), Some(true));
        assert!(value.get("projectId").is_some_and(Value::is_null));
        fs::remove_file(path).unwrap();
    }
}
