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
mod windows_recovery;

const SETUP_STATUS_FILE: &str = "setup-status.json";
const SETUP_STATUS_SCHEMA_VERSION: u32 = 3;
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
    Activated,
    Warning(String),
    NotRequested,
}

#[derive(Clone, Debug)]
struct SetupInstallation {
    completion: SetupCompletion,
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
    if error.to_string().contains("MSIX_ELEVATION_REQUIRED") {
        SetupFailureClassification {
            message: "安装此应用需要管理员权限，安装包已下载完成，请授权后重试。",
            error_code: "MSIX_ELEVATION_REQUIRED",
            retryable: true,
        }
    } else if unsupported_os {
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

    fn from_desktop_activation(result: Result<()>) -> Self {
        match result {
            Ok(()) => Self::Activated,
            Err(error) => Self::Warning(format!(
                    "ChatGPT/Codex 已完成安装配置，但系统桌面应用启动请求失败。请到系统应用列表中手动找到并打开 ChatGPT 应用（部分版本名称为 Codex）。自动启动错误：{}",
                    compact_error(&format!("{error:#}"))
                )),
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Activated => {
                "Codex 应用初始化已完成，系统已接受当前授权档案的桌面应用启动请求。".to_string()
            }
            Self::Warning(warning) => warning.clone(),
            Self::NotRequested => {
                "Codex 应用初始化已完成；检测到既有个人配置，未自动切换或打开工作区应用。"
                    .to_string()
            }
        }
    }

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
        if matches!(status.status.as_str(), "failed" | "interrupted")
            && status
                .installer_status
                .as_ref()
                .is_some_and(|installer| installer.package_recovery.is_some())
        {
            if let Err(error) =
                windows_recovery::recovery_package(&status, &installer_state_dir(), false)
            {
                if let Some(installer) = status.installer_status.as_mut() {
                    installer.package_recovery = None;
                }
                status.error = Some(error.to_string());
                status.error_code = Some("MSIX_PACKAGE_UNAVAILABLE".to_string());
            }
        }
        status
    }

    pub fn start(&self, workspace_id: u64, force: bool) -> Result<SetupStatus> {
        self.start_with_elevation(workspace_id, force, false)
    }

    pub fn reveal_installer_package(&self) -> Result<()> {
        let status = self.state();
        let package = windows_recovery::recovery_package(&status, &installer_state_dir(), false)?;
        windows_recovery::reveal(&package)
    }

    pub fn start_with_elevation(
        &self,
        workspace_id: u64,
        force: bool,
        elevate: bool,
    ) -> Result<SetupStatus> {
        let operation = crate::state_access::Operation::begin("安装 Codex")?;
        if elevate {
            let status = self.state();
            if !cfg!(target_os = "windows") || status.workspace_id != Some(workspace_id) {
                anyhow::bail!("只能为当前 Windows 工作区重试管理员安装");
            }
            windows_recovery::recovery_package(&status, &installer_state_dir(), true)?;
        }
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

        crate::state_access::initialize_local_data()?;
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
            let install_result = run_install(workspace_id, elevate);
            drop(operation);
            match install_result {
                Ok(installed) => {
                    let completed = SetupStatus {
                        schema_version: SETUP_STATUS_SCHEMA_VERSION,
                        attempt_id: background.attempt_id.clone(),
                        connector_version: Some(CONNECTOR_VERSION.to_string()),
                        status: "succeeded".to_string(),
                        workspace_id: Some(workspace_id),
                        message: installed.completion.message(),
                        error: None,
                        last_error: None,
                        error_code: None,
                        retryable: false,
                        automatic_retry_count: background.automatic_retry_count,
                        started_at_epoch_seconds: background.started_at_epoch_seconds,
                        completed_at_epoch_seconds: Some(now_epoch_seconds()),
                        installer_status: None,
                    };
                    let _ = manager.replace(completed);
                }
                Err(error) => {
                    let classification = classify_setup_failure(&error);
                    let error = setup_error_detail(&error);
                    let completed = SetupStatus {
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
                    };
                    let _ = manager.replace(completed);
                }
            }
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

    // Schema 2 recorded a model probe after installation had already succeeded.
    // Retire only that probe's state; real installation errors and launch warnings survive.
    if status.schema_version < SETUP_STATUS_SCHEMA_VERSION && status.status == "succeeded" {
        status.schema_version = SETUP_STATUS_SCHEMA_VERSION;
        if status.message.contains("百积木路由") {
            const LAUNCH_WARNING: &str =
                "ChatGPT/Codex 已完成安装配置，但系统桌面应用启动请求失败。";
            status.message = match status.message.find(LAUNCH_WARNING) {
                Some(offset) => status.message[offset..].trim().to_string(),
                None => "Codex 应用安装和工作区配置已完成，可以打开使用。".to_string(),
            };
        }
        if status
            .last_error
            .as_deref()
            .is_some_and(|error| error.starts_with("百积木路由验证"))
        {
            status.last_error = None;
        }
        status.retryable = false;
        status
            .completed_at_epoch_seconds
            .get_or_insert_with(now_epoch_seconds);
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

fn run_install(workspace_id: u64, elevate: bool) -> Result<SetupInstallation> {
    #[cfg(not(target_os = "windows"))]
    let _ = elevate;
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
        run_windows_install(workspace_id, elevate)
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = workspace_id;
        anyhow::bail!("Codex 一键安装目前只支持 macOS 和 Windows")
    }
}

#[cfg(target_os = "windows")]
fn run_windows_install(workspace_id: u64, elevate: bool) -> Result<SetupInstallation> {
    let setup_dir = connector_home().join("setup");
    fs::create_dir_all(&setup_dir)
        .with_context(|| format!("创建安装目录失败: {}", setup_dir.display()))?;
    set_private_directory(&setup_dir)?;
    let unique = format!("{}-{}", std::process::id(), now_epoch_seconds());
    let script_path = setup_dir.join(format!("install-{unique}.ps1"));
    let auto_activate = credential::should_auto_activate_workspace_after_setup()?;
    let desktop_guard = SetupDesktopGuard::stop_for_activation(auto_activate)?;

    let install_result = (|| -> Result<Option<PathBuf>> {
        let prepared = credential::initialize_workspace_profile(workspace_id)?;
        let profile_home = PathBuf::from(&prepared.profile.codex_home);
        credential::finalize_workspace_setup(&prepared.profile, auto_activate)?;
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
            .env(
                "CODEX_AUTH_PROFILE_ACTIVATED",
                if auto_activate { "1" } else { "0" },
            )
            .env("CODEX_ARTIFACT_MANIFEST_URL", source::manifest_url()?)
            .env("CODEX_INSTALL_STATE_DIR", &state_dir)
            .env("CODEX_INSTALL_QUIET", "1")
            .env("CODEX_INSTALL_ELEVATE", if elevate { "1" } else { "0" })
            .env("CODEX_UI_LOCALE", &product_config.default_ui_locale)
            .env("CODEX_HOME", &profile_home)
            .env(
                "CODEX_DESKTOP_PROTOCOL",
                &product_config.windows_desktop_protocol,
            )
            .env("CODEX_DESKTOP_TRUSTED_PUBLISHERS", trusted_publishers)
            .env_remove("CODEX_PROJECT_ID")
            .env_remove("BAIJIMU_PROJECT_ID")
            .env_remove("PROJECT_ID");
        let output =
            crate::child_process::output(&mut command, std::time::Duration::from_secs(30 * 60))
                .context("执行 Codex 官方安装脚本失败")?;
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
        let credential_state = credential::state()?;
        let workspace_profile_is_active = credential_state.active_mode
            == credential::AuthMode::Baijimu
            && credential_state.active_workspace_id == Some(workspace_id)
            && Path::new(&credential_state.active_codex_home) == profile_home;
        Ok(workspace_profile_is_active.then_some(profile_home))
    })();
    let _ = fs::remove_file(&script_path);
    let activated_profile_home = install_result?;

    if !credential::codex_ready_for_workspace(workspace_id) {
        anyhow::bail!("安装脚本执行成功，但工作区凭证归属回查失败");
    }
    let completion = match activated_profile_home {
        Some(_) => desktop_guard.launch(),
        None => SetupCompletion::completed_without_desktop_launch(),
    };
    Ok(SetupInstallation { completion })
}

struct SetupDesktopGuard {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    stopped: Option<crate::desktop::DesktopSwitch>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    codex_home: PathBuf,
    launch_after_setup: bool,
}

impl SetupDesktopGuard {
    fn stop_for_activation(activate: bool) -> Result<Self> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let stopped = if activate {
            Some(crate::desktop::stop_for_workspace_switch()?)
        } else {
            None
        };
        Ok(Self {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            stopped,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            codex_home: crate::codex_workspace::active()
                .map(|workspace| PathBuf::from(workspace.codex_home))
                .unwrap_or_else(|_| crate::credential::default_codex_home()),
            launch_after_setup: activate,
        })
    }

    fn launch(mut self) -> SetupCompletion {
        self.launch_after_setup = false;
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            self.stopped.take();
            SetupCompletion::from_desktop_activation(crate::desktop::launch_workspace(
                &self.codex_home,
            ))
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            SetupCompletion::completed_without_desktop_launch()
        }
    }
}

impl Drop for SetupDesktopGuard {
    fn drop(&mut self) {
        if !self.launch_after_setup {
            return;
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(stopped) = self.stopped.take() {
            if let Err(error) = stopped.restart_workspace_if_needed(&self.codex_home) {
                eprintln!("安装失败后恢复 ChatGPT/Codex 桌面应用失败: {error}");
            }
        }
    }
}

#[cfg(any(target_os = "windows", test))]
fn windows_install_script_bytes() -> Vec<u8> {
    const UTF8_BOM: &[u8] = &[0xef, 0xbb, 0xbf];
    let source = include_bytes!("../installers/windows-configure-terminal-and-login.ps1");
    let helper = include_bytes!("../installers/windows-msix-install.ps1");
    let mut script = Vec::with_capacity(UTF8_BOM.len() + helper.len() + source.len() + 1);
    script.extend_from_slice(UTF8_BOM);
    script.extend_from_slice(helper);
    script.push(b'\n');
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
    env::var_os("BAIJIMU_LOCAL_APP_DATA_DIR")
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

fn setup_error_detail(error: &anyhow::Error) -> String {
    compact_error(&format!("{error:#}"))
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
    #[test]
    fn windows_permission_failure_has_a_specific_actionable_code() {
        let result = super::classify_setup_failure(&anyhow::anyhow!(
            "MSIX_ELEVATION_REQUIRED: permission denied"
        ));
        assert_eq!(result.error_code, "MSIX_ELEVATION_REQUIRED");
        assert!(result.retryable);
        assert!(result.message.contains("请授权后重试"));
    }

    #[test]
    fn setup_failure_preserves_the_cli_cause_and_error_code() {
        let error = anyhow::anyhow!("LLM_CREDENTIAL_CREDENTIAL_DENIED: 无权使用该模型凭证")
            .context("baijimu CLI llm-credential create 失败")
            .context("baijimu CLI 签发工作区 LLM credential 失败");
        let detail = super::setup_error_detail(&error);
        assert!(detail.contains("签发工作区"));
        assert!(detail.contains("llm-credential create"));
        assert!(detail.contains("LLM_CREDENTIAL_CREDENTIAL_DENIED"));
        assert!(detail.contains("无权使用该模型凭证"));
    }

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
    fn legacy_probe_states_are_retired_once_without_changing_install_identity() {
        for completed_at in [None, Some(200)] {
            let mut persisted = persisted_status("succeeded", Some("2.1.5"));
            persisted.schema_version = 2;
            persisted.error = None;
            persisted.message = "Codex 已安装；百积木路由正在后台验证".to_string();
            persisted.last_error =
                Some("百积木路由验证已尝试 1 次但暂未通过：HTTP 400".to_string());
            persisted.retryable = true;
            persisted.completed_at_epoch_seconds = completed_at;
            let (recovered, changed) = recover_persisted_status(persisted);
            assert!(changed);
            assert_eq!(recovered.status, "succeeded");
            assert_eq!(recovered.workspace_id, Some(1197));
            assert_eq!(recovered.connector_version.as_deref(), Some("2.1.5"));
            assert_eq!(recovered.started_at_epoch_seconds, Some(100));
            assert_eq!(recovered.error, None);
            assert_eq!(recovered.last_error, None);
            assert!(!recovered.retryable);
            assert!(!recovered.message.contains("路由"));
            assert!(recovered.completed_at_epoch_seconds.is_some());
            if let Some(expected) = completed_at {
                assert_eq!(recovered.completed_at_epoch_seconds, Some(expected));
            }
            assert!(!recover_persisted_status(recovered).1);
        }
    }

    #[test]
    fn retiring_probe_preserves_desktop_launch_warning_and_unrelated_errors() {
        let warning =
            SetupCompletion::from_desktop_activation(Err(anyhow::anyhow!("launch denied")))
                .message();
        for message in [
            warning.clone(),
            format!("百积木路由验证暂未通过。 {warning}"),
        ] {
            let mut persisted = persisted_status("succeeded", Some("2.1.5"));
            persisted.message = message;
            persisted.last_error = Some("unrelated diagnostic".to_string());
            let (recovered, _) = recover_persisted_status(persisted);
            assert_eq!(recovered.message, warning);
            assert_eq!(recovered.error.as_deref(), Some("历史安装错误"));
            assert_eq!(
                recovered.last_error.as_deref(),
                Some("unrelated diagnostic")
            );
        }
    }

    #[test]
    fn retiring_probe_does_not_erase_failed_or_interrupted_installations() {
        for state in ["failed", "interrupted"] {
            let mut persisted = persisted_status(state, Some(CONNECTOR_VERSION));
            persisted.schema_version = 2;
            persisted.last_error = Some("MSIX_ELEVATION_REQUIRED".to_string());
            persisted.error_code = Some("MSIX_ELEVATION_REQUIRED".to_string());
            persisted.retryable = true;
            let (recovered, changed) = recover_persisted_status(persisted);
            assert!(!changed);
            assert_eq!(recovered.status, state);
            assert_eq!(recovered.error.as_deref(), Some("历史安装错误"));
            assert_eq!(
                recovered.last_error.as_deref(),
                Some("MSIX_ELEVATION_REQUIRED")
            );
            assert_eq!(
                recovered.error_code.as_deref(),
                Some("MSIX_ELEVATION_REQUIRED")
            );
            assert!(recovered.retryable);
        }
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
        let outcome = SetupCompletion::from_desktop_activation(Err(anyhow::anyhow!(
            "operating system rejected automatic launch"
        )));

        assert!(matches!(&outcome, SetupCompletion::Warning(_)));
        assert!(outcome.message().contains("已完成安装配置"));
        assert!(outcome.message().contains("桌面应用启动请求失败"));
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
    fn successful_desktop_auto_launch_reports_the_accepted_activation() {
        let outcome = SetupCompletion::from_desktop_activation(Ok(()));

        assert_eq!(outcome, SetupCompletion::Activated);
        assert!(outcome
            .message()
            .contains("已接受当前授权档案的桌面应用启动请求"));
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
