use crate::credential;
use anyhow::{Context, Result};
#[cfg(any(target_os = "windows", test))]
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "windows")]
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

mod contract;
#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
mod macos;
mod source;

#[cfg(any(target_os = "windows", test))]
use contract::InstallerResultEnvelope;
pub use contract::InstallerStatus;

const SETUP_STATUS_FILE: &str = "setup-status.json";
const SETUP_STATUS_SCHEMA_VERSION: u32 = 2;
const CONNECTOR_VERSION: &str = env!("CARGO_PKG_VERSION");
const ERROR_CODE_INTERRUPTED: &str = "SETUP_INTERRUPTED";
const ERROR_CODE_RETRY_AFTER_UPGRADE: &str = "SETUP_RETRY_REQUIRED_AFTER_UPGRADE";
const ERROR_CODE_FAILED: &str = "SETUP_FAILED";
#[cfg(target_os = "windows")]
const WINDOWS_INSTALL_SCRIPT_ENV: &str = "CODEX_CONNECTOR_INSTALL_SCRIPT_PATH";
#[cfg(any(target_os = "windows", test))]
const WINDOWS_INSTALL_WRAPPER: &str = r#"$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$OutputEncoding = [Console]::OutputEncoding
& $env:CODEX_CONNECTOR_INSTALL_SCRIPT_PATH
"#;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupStatus {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub attempt_id: Option<String>,
    #[serde(default)]
    pub connector_version: Option<String>,
    pub status: String,
    pub workspace_id: Option<u64>,
    pub message: String,
    pub error: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub automatic_retry_count: u32,
    pub started_at_epoch_seconds: Option<u64>,
    pub completed_at_epoch_seconds: Option<u64>,
    pub installer_status: Option<InstallerStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SetupCompletion {
    Verified,
    Warning(String),
    NotRequested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SetupFailureClassification {
    message: &'static str,
    error_code: &'static str,
    retryable: bool,
}

fn classify_setup_failure(error: &anyhow::Error) -> SetupFailureClassification {
    let unsupported_os = crate::system_compatibility::unsupported_os_version(error).is_some()
        || crate::system_compatibility::message_is_unsupported_os_version(&error.to_string());
    if unsupported_os {
        SetupFailureClassification {
            message: "当前系统版本不支持 ChatGPT/Codex 桌面应用",
            error_code: crate::system_compatibility::ERROR_CODE_UNSUPPORTED_OS_VERSION,
            retryable: false,
        }
    } else {
        SetupFailureClassification {
            message: "Codex 应用初始化失败",
            error_code: ERROR_CODE_FAILED,
            retryable: true,
        }
    }
}

impl SetupCompletion {
    fn completed_without_desktop_launch() -> Self {
        Self::NotRequested
    }

    fn from_desktop_launch(result: Result<()>) -> Self {
        match result {
            Ok(()) => Self::Verified,
            Err(error) => Self::Warning(format!(
                    "ChatGPT/Codex 已完成安装配置，但自动打开桌面窗口的校验未通过。请到系统应用列表中手动找到并打开 ChatGPT 应用（部分版本名称为 Codex）。自动打开校验错误：{}",
                    compact_error(&format!("{error:#}"))
                )),
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Verified => {
                "Codex 应用初始化已完成，并已确认当前工作区桌面窗口打开。".to_string()
            }
            Self::Warning(warning) => warning.clone(),
            Self::NotRequested => {
                "Codex 应用初始化已完成；检测到既有个人配置，未自动切换或打开工作区应用。"
                    .to_string()
            }
        }
    }

    #[cfg(any(target_os = "macos", test))]
    fn warning(&self) -> Option<&str> {
        match self {
            Self::Warning(warning) => Some(warning),
            _ => None,
        }
    }
}

impl Default for SetupStatus {
    fn default() -> Self {
        Self {
            schema_version: SETUP_STATUS_SCHEMA_VERSION,
            attempt_id: None,
            connector_version: Some(CONNECTOR_VERSION.to_string()),
            status: "pending".to_string(),
            workspace_id: None,
            message: "等待初始化".to_string(),
            error: None,
            last_error: None,
            error_code: None,
            retryable: false,
            automatic_retry_count: 0,
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
        let (status, should_persist) = match fs::read(status_path())
            .ok()
            .and_then(|content| crate::json_compat::from_slice::<SetupStatus>(&content).ok())
        {
            Some(status) => recover_persisted_status(status),
            None => (SetupStatus::default(), true),
        };
        let manager = Self {
            state: Arc::new(Mutex::new(status)),
        };
        if should_persist {
            let _ = manager.persist();
        }
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
        status.installer_status = match status.status.as_str() {
            "running" | "failed" | "interrupted" | "succeeded" => {
                read_json(installer_state_dir().join("status.json"))
            }
            _ => None,
        };
        status
    }

    pub fn start(&self, workspace_id: u64, force: bool) -> Result<SetupStatus> {
        if workspace_id == 0 {
            anyhow::bail!("workspaceId 必须是正整数");
        }
        let automatic_retry_count = {
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
                && current.status == "succeeded"
                && current.workspace_id == Some(workspace_id)
                && credential::codex_ready_for_workspace(workspace_id)
            {
                return Ok(current.clone());
            }
            if force {
                0
            } else {
                current.automatic_retry_count
            }
        };

        let started_at = now_epoch_seconds();
        let running = SetupStatus {
            schema_version: SETUP_STATUS_SCHEMA_VERSION,
            attempt_id: Some(format!(
                "{CONNECTOR_VERSION}-{}-{started_at}",
                std::process::id()
            )),
            connector_version: Some(CONNECTOR_VERSION.to_string()),
            status: "running".to_string(),
            workspace_id: Some(workspace_id),
            message: "正在初始化 Codex 应用".to_string(),
            error: None,
            last_error: None,
            error_code: None,
            retryable: false,
            automatic_retry_count,
            started_at_epoch_seconds: Some(started_at),
            completed_at_epoch_seconds: None,
            installer_status: None,
        };
        self.replace(running.clone())?;

        let manager = self.clone();
        let background = running.clone();
        thread::spawn(move || {
            let completed = match run_install(workspace_id) {
                Ok(outcome) => SetupStatus {
                    schema_version: SETUP_STATUS_SCHEMA_VERSION,
                    attempt_id: background.attempt_id.clone(),
                    connector_version: Some(CONNECTOR_VERSION.to_string()),
                    status: "succeeded".to_string(),
                    workspace_id: Some(workspace_id),
                    message: outcome.message(),
                    error: None,
                    last_error: None,
                    error_code: None,
                    retryable: false,
                    automatic_retry_count: background.automatic_retry_count,
                    started_at_epoch_seconds: background.started_at_epoch_seconds,
                    completed_at_epoch_seconds: Some(now_epoch_seconds()),
                    installer_status: None,
                },
                Err(error) => {
                    let classification = classify_setup_failure(&error);
                    let error = compact_error(&error.to_string());
                    SetupStatus {
                        schema_version: SETUP_STATUS_SCHEMA_VERSION,
                        attempt_id: background.attempt_id.clone(),
                        connector_version: Some(CONNECTOR_VERSION.to_string()),
                        status: "failed".to_string(),
                        workspace_id: Some(workspace_id),
                        message: classification.message.to_string(),
                        error: Some(error.clone()),
                        last_error: Some(error),
                        error_code: Some(classification.error_code.to_string()),
                        retryable: classification.retryable,
                        automatic_retry_count: background.automatic_retry_count,
                        started_at_epoch_seconds: background.started_at_epoch_seconds,
                        completed_at_epoch_seconds: Some(now_epoch_seconds()),
                        installer_status: None,
                    }
                }
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

fn recover_persisted_status(mut status: SetupStatus) -> (SetupStatus, bool) {
    if status.status == "running" {
        status.schema_version = SETUP_STATUS_SCHEMA_VERSION;
        status.status = "interrupted".to_string();
        status.last_error = status.error.take().or(status.last_error);
        status.error_code = Some(ERROR_CODE_INTERRUPTED.to_string());
        status.retryable = true;
        status.automatic_retry_count = status.automatic_retry_count.saturating_add(1);
        status.message = "上次初始化被中断，将自动重新验证；也可以手动重试".to_string();
        status.completed_at_epoch_seconds = Some(now_epoch_seconds());
        return (status, true);
    }

    if status.status == "failed" && status.connector_version.as_deref() != Some(CONNECTOR_VERSION) {
        let previous_version = status
            .connector_version
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("旧版本");
        status.schema_version = SETUP_STATUS_SCHEMA_VERSION;
        status.status = "needs_retry".to_string();
        status.last_error = status.error.take().or(status.last_error);
        status.error_code = Some(ERROR_CODE_RETRY_AFTER_UPGRADE.to_string());
        status.retryable = true;
        status.message =
            format!("检测到 {previous_version} 留下的初始化失败记录，当前版本将重新验证");
        return (status, true);
    }

    (status, false)
}

fn run_install(workspace_id: u64) -> Result<SetupCompletion> {
    #[cfg(target_os = "macos")]
    {
        let setup_dir = connector_home().join("setup");
        fs::create_dir_all(&setup_dir)
            .with_context(|| format!("创建安装目录失败: {}", setup_dir.display()))?;
        set_private_directory(&setup_dir)?;
        let unique = format!("{}-{}", std::process::id(), now_epoch_seconds());
        let script_path = setup_dir.join(format!("native-install-{unique}.sh"));
        atomic_write_private(
            &script_path,
            include_bytes!("../installers/macos-configure-terminal-and-login.sh"),
        )?;
        let install_result = macos::run_install(workspace_id, &script_path);
        let _ = fs::remove_file(&script_path);
        install_result
    }

    #[cfg(target_os = "windows")]
    {
        run_windows_install(workspace_id)
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = workspace_id;
        anyhow::bail!("Codex 一键安装目前只支持 macOS 和 Windows")
    }
}

#[cfg(target_os = "windows")]
fn run_windows_install(workspace_id: u64) -> Result<SetupCompletion> {
    let setup_dir = connector_home().join("setup");
    fs::create_dir_all(&setup_dir)
        .with_context(|| format!("创建安装目录失败: {}", setup_dir.display()))?;
    set_private_directory(&setup_dir)?;
    let unique = format!("{}-{}", std::process::id(), now_epoch_seconds());
    let secret_path = setup_dir.join(format!("credential-{unique}"));
    let script_path = setup_dir.join(format!("install-{unique}.ps1"));
    let auto_activate = credential::should_auto_activate_workspace_after_setup()?;

    let install_result = (|| -> Result<Option<PathBuf>> {
        let prepared = credential::prepare_workspace_profile(workspace_id)?;
        let profile_home = PathBuf::from(&prepared.profile.codex_home);
        atomic_write_private(&secret_path, prepared.credential.as_bytes())?;

        atomic_write_private(&script_path, &windows_install_script_bytes())?;
        let state_dir = installer_state_dir();
        fs::create_dir_all(&state_dir)?;
        set_private_directory(&state_dir)?;
        let _ = fs::remove_file(state_dir.join("status.json"));
        let _ = fs::remove_file(state_dir.join("result.json"));

        let mut command = install_command(&script_path)?;
        let product_config = crate::product_config::get();
        let trusted_publishers = product_config.windows_desktop_trusted_publishers.join("\n");
        command
            .env("CODEX_WORKSPACE_ID", workspace_id.to_string())
            .env("CODEX_ARTIFACT_MANIFEST_URL", source::manifest_url()?)
            .env("CODEX_LLM_CREDENTIAL_FILE", &secret_path)
            .env("CODEX_INSTALL_STATE_DIR", &state_dir)
            .env("CODEX_INSTALL_QUIET", "1")
            .env("CODEX_UI_LOCALE", &product_config.default_ui_locale)
            .env(
                "CODEX_DESKTOP_PROTOCOL",
                &product_config.windows_desktop_protocol,
            )
            .env("CODEX_DESKTOP_TRUSTED_PUBLISHERS", trusted_publishers)
            .env("CODEX_HOME", &profile_home)
            .env_remove("CODEX_PROJECT_ID")
            .env_remove("BAIJIMU_PROJECT_ID")
            .env_remove("PROJECT_ID");
        let output = command.output().context("启动 Codex 官方安装脚本失败")?;
        let installer_result_path = state_dir.join("result.json");
        let installer_result = read_json::<InstallerResultEnvelope>(&installer_result_path)
            .with_context(|| {
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
        if !output.status.success() || !installer_result.ok {
            let errors = (!installer_result.errors.is_empty())
                .then(|| installer_result.errors.join("；"))
                .unwrap_or_else(|| String::from_utf8_lossy(&output.stderr).to_string());
            anyhow::bail!("官方安装脚本执行失败: {}", compact_error(&errors));
        }
        credential::finalize_workspace_setup(&prepared.profile, auto_activate)?;
        let credential_state = credential::state()?;
        let workspace_profile_is_active = credential_state.active_mode
            == credential::AuthMode::Baijimu
            && credential_state.active_workspace_id == Some(workspace_id)
            && Path::new(&credential_state.active_codex_home) == profile_home;
        Ok(workspace_profile_is_active.then_some(profile_home))
    })();
    let _ = fs::remove_file(&secret_path);
    let _ = fs::remove_file(&script_path);
    let activated_profile_home = install_result?;

    if !credential::codex_ready_for_workspace(workspace_id) {
        anyhow::bail!("安装脚本执行成功，但独立工作区凭证归属回查失败");
    }
    Ok(match activated_profile_home {
        Some(profile_home) => launch_desktop_after_setup(&profile_home),
        None => SetupCompletion::completed_without_desktop_launch(),
    })
}

fn launch_desktop_after_setup(profile_home: &Path) -> SetupCompletion {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        SetupCompletion::from_desktop_launch(crate::desktop::launch_and_verify(profile_home))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = profile_home;
        SetupCompletion::completed_without_desktop_launch()
    }
}

#[cfg(any(target_os = "windows", test))]
fn windows_install_script_bytes() -> Vec<u8> {
    const UTF8_BOM: &[u8] = &[0xef, 0xbb, 0xbf];
    let source = include_bytes!("../installers/windows-configure-terminal-and-login.ps1");
    let mut script = Vec::with_capacity(UTF8_BOM.len() + source.len());
    script.extend_from_slice(UTF8_BOM);
    script.extend_from_slice(source);
    script
}

#[cfg(target_os = "windows")]
fn install_command(script_path: &Path) -> Result<Command> {
    let mut command = Command::new("powershell.exe");
    command.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-OutputFormat",
        "Text",
        "-EncodedCommand",
        &powershell_encoded_command(WINDOWS_INSTALL_WRAPPER),
    ]);
    command.env(WINDOWS_INSTALL_SCRIPT_ENV, script_path);
    Ok(command)
}

#[cfg(any(target_os = "windows", test))]
fn powershell_encoded_command(script: &str) -> String {
    let utf16le = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    BASE64_STANDARD.encode(utf16le)
}

fn connector_home() -> PathBuf {
    env::var_os("BAIJIMU_CONNECTOR_DATA_DIR")
        .or_else(|| env::var_os("CODEX_DESKTOP_HOME"))
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

fn read_json<T: DeserializeOwned>(path: impl AsRef<Path>) -> Option<T> {
    fs::read(path)
        .ok()
        .and_then(|content| crate::json_compat::from_slice(&content).ok())
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

    fn persisted_status(status: &str, connector_version: Option<&str>) -> SetupStatus {
        SetupStatus {
            schema_version: 1,
            connector_version: connector_version.map(str::to_string),
            status: status.to_string(),
            workspace_id: Some(1197),
            message: "历史初始化状态".to_string(),
            error: Some("历史安装错误".to_string()),
            started_at_epoch_seconds: Some(100),
            completed_at_epoch_seconds: Some(200),
            ..SetupStatus::default()
        }
    }

    #[test]
    fn interrupted_install_becomes_retryable_without_replaying_the_old_error() {
        let (recovered, changed) =
            recover_persisted_status(persisted_status("running", Some(CONNECTOR_VERSION)));

        assert!(changed);
        assert_eq!(recovered.status, "interrupted");
        assert_eq!(recovered.error, None);
        assert_eq!(recovered.last_error.as_deref(), Some("历史安装错误"));
        assert_eq!(
            recovered.error_code.as_deref(),
            Some(ERROR_CODE_INTERRUPTED)
        );
        assert!(recovered.retryable);
        assert_eq!(recovered.automatic_retry_count, 1);
        assert_eq!(recovered.schema_version, SETUP_STATUS_SCHEMA_VERSION);
        assert!(recovered.completed_at_epoch_seconds.unwrap_or_default() >= 200);
    }

    #[test]
    fn failed_status_from_an_older_connector_requires_fresh_verification() {
        let (recovered, changed) =
            recover_persisted_status(persisted_status("failed", Some("1.2.27")));

        assert!(changed);
        assert_eq!(recovered.status, "needs_retry");
        assert_eq!(recovered.error, None);
        assert_eq!(recovered.last_error.as_deref(), Some("历史安装错误"));
        assert_eq!(
            recovered.error_code.as_deref(),
            Some(ERROR_CODE_RETRY_AFTER_UPGRADE)
        );
        assert!(recovered.retryable);
        assert!(recovered.message.contains("1.2.27"));
    }

    #[test]
    fn current_connector_failure_is_not_rewritten_on_restart() {
        let original = persisted_status("failed", Some(CONNECTOR_VERSION));
        let (recovered, changed) = recover_persisted_status(original.clone());

        assert!(!changed);
        assert_eq!(recovered.status, original.status);
        assert_eq!(recovered.error, original.error);
        assert_eq!(recovered.completed_at_epoch_seconds, Some(200));
    }

    #[test]
    fn desktop_auto_launch_failure_keeps_setup_successful_and_requires_manual_open() {
        let outcome = SetupCompletion::from_desktop_launch(Err(anyhow::anyhow!(
            "operating system rejected automatic launch"
        )));

        assert!(matches!(&outcome, SetupCompletion::Warning(_)));
        assert!(outcome.message().contains("已完成安装配置"));
        assert!(outcome.message().contains("桌面窗口的校验未通过"));
        assert!(outcome.message().contains("手动找到并打开 ChatGPT"));
        assert!(outcome
            .warning()
            .is_some_and(|warning| warning.contains("operating system rejected automatic launch")));
    }

    #[test]
    fn unsupported_os_setup_failure_has_a_stable_non_retryable_code() {
        let error = crate::system_compatibility::ensure_supported(
            "macOS",
            "12.2.1",
            "14.0",
            "ChatGPT/Codex",
        )
        .unwrap_err();
        let classification = classify_setup_failure(&error);

        assert_eq!(
            classification.error_code,
            crate::system_compatibility::ERROR_CODE_UNSUPPORTED_OS_VERSION
        );
        assert!(!classification.retryable);
        assert!(classification.message.contains("系统版本不支持"));
    }

    #[test]
    fn windows_installer_marker_maps_to_the_same_unsupported_os_code() {
        let error = anyhow::anyhow!(
            "官方安装脚本执行失败: UNSUPPORTED_OS_VERSION: current Windows is too old"
        );
        let classification = classify_setup_failure(&error);

        assert_eq!(
            classification.error_code,
            crate::system_compatibility::ERROR_CODE_UNSUPPORTED_OS_VERSION
        );
        assert!(!classification.retryable);
    }

    #[test]
    fn successful_desktop_auto_launch_reports_the_opened_workspace() {
        let outcome = SetupCompletion::from_desktop_launch(Ok(()));

        assert_eq!(outcome, SetupCompletion::Verified);
        assert!(outcome.message().contains("已确认当前工作区桌面窗口打开"));
        assert_eq!(outcome.warning(), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_installer_is_compiled_into_the_connector() {
        let script = include_bytes!("../installers/macos-configure-terminal-and-login.sh");
        assert!(script.len() > 1_000);
        assert!(script.starts_with(b"#!/usr/bin/env bash"));
    }

    #[test]
    fn powershell_wrapper_is_encoded_as_utf16le_and_forces_utf8_output() {
        let encoded = powershell_encoded_command(WINDOWS_INSTALL_WRAPPER);
        let bytes = BASE64_STANDARD.decode(encoded).unwrap();
        let units = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        let decoded = String::from_utf16(&units).unwrap();

        assert_eq!(decoded, WINDOWS_INSTALL_WRAPPER);
        assert!(decoded.contains("[Console]::OutputEncoding"));
        assert!(decoded.contains("$OutputEncoding = [Console]::OutputEncoding"));
        assert!(decoded.contains("& $env:CODEX_CONNECTOR_INSTALL_SCRIPT_PATH"));
    }

    #[test]
    fn windows_installer_is_written_as_utf8_with_bom_for_powershell_5() {
        let script = windows_install_script_bytes();
        assert!(script.starts_with(&[0xef, 0xbb, 0xbf]));
        let source = std::str::from_utf8(&script[3..]).unwrap();
        assert!(source.starts_with("$ErrorActionPreference"));
        assert!(source.contains("检查 ChatGPT 桌面应用"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_installer_is_compiled_into_the_connector() {
        let script = include_bytes!("../installers/windows-configure-terminal-and-login.ps1");
        assert!(script.len() > 1_000);
        assert!(script.starts_with(b"$ErrorActionPreference"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_install_command_emits_chinese_errors_as_utf8() {
        let path = env::temp_dir().join(format!(
            "codex-setup-utf8-error-{}-{}.ps1",
            std::process::id(),
            now_epoch_seconds()
        ));
        fs::write(
            &path,
            r#"$message = -join @([char]0x8def, [char]0x5f84, [char]0x683c, [char]0x5f0f, [char]0x9519, [char]0x8bef)
throw $message
"#,
        )
        .unwrap();

        let output = install_command(&path).unwrap().output().unwrap();
        fs::remove_file(path).unwrap();
        assert!(!output.status.success());
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        assert!(stderr.contains("路径格式错误"), "stderr={stderr:?}");
    }

    #[test]
    fn reads_windows_powershell_json_with_utf8_bom() {
        let path = env::temp_dir().join(format!(
            "codex-setup-result-{}-{}.json",
            std::process::id(),
            now_epoch_seconds()
        ));
        fs::write(
            &path,
            "\u{feff}{\"ok\":true,\"projectId\":null,\"errors\":[]}",
        )
        .unwrap();
        let value = read_json::<InstallerResultEnvelope>(&path).unwrap();
        assert!(value.ok);
        assert!(value.errors.is_empty());
        fs::remove_file(path).unwrap();
    }
}
