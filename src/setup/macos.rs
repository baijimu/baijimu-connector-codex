use super::contract::{InstallerStatus, InstallerStepState, MacosInstallerResult};
use super::{
    atomic_write_private, compact_error, installer_state_dir, launch_desktop_after_setup,
    set_private_directory, SetupCompletion, SetupInstallation,
};
use crate::credential;
use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MANIFEST_SCHEMA_VERSION: u32 = 4;
const MANIFEST_KIND: &str = "baijimu.codex.customer-install-artifacts";
const TARGET_APP_PATH: &str = "/Applications/ChatGPT.app";
const LEGACY_APP_PATH: &str = "/Applications/Codex.app";

pub(super) fn run_install(
    workspace_id: u64,
    native_script_path: &Path,
) -> Result<SetupInstallation> {
    let state_dir = installer_state_dir();
    fs::create_dir_all(&state_dir)
        .with_context(|| format!("创建安装状态目录失败: {}", state_dir.display()))?;
    set_private_directory(&state_dir)?;
    let _ = fs::remove_file(state_dir.join("status.json"));
    let _ = fs::remove_file(state_dir.join("result.json"));

    let work_dir = TemporaryDirectory::create("baijimu-codex-setup")?;
    let model = credential::default_model().to_string();
    let request = MacosInstallRequest {
        workspace_id,
        native_script_path,
        state_dir: &state_dir,
        work_dir: work_dir.path(),
        model,
    };
    let mut installer = MacosInstaller::new(request)?;
    let started = Instant::now();
    match installer.execute() {
        Ok(installed) => {
            installer.result.ok = true;
            installer.result.elapsed_ms = started.elapsed().as_millis();
            installer.write_result()?;
            Ok(installed)
        }
        Err(error) => {
            let message = compact_error(&format!("{error:#}"));
            installer.fail_progress(&message);
            installer.result.ok = false;
            installer.result.elapsed_ms = started.elapsed().as_millis();
            installer.result.errors = vec![message.clone()];
            if let Err(write_error) = installer.write_result() {
                return Err(error).context(format!("安装失败且结果文件写入失败：{write_error}"));
            }
            Err(error).context(message)
        }
    }
}

struct MacosInstaller<'a> {
    workspace_id: u64,
    native_script_path: &'a Path,
    work_dir: &'a Path,
    progress: ProgressStore,
    result_path: PathBuf,
    manifest: Option<UpstreamManifestV4>,
    result: MacosInstallerResult,
}

struct MacosInstallRequest<'a> {
    workspace_id: u64,
    native_script_path: &'a Path,
    state_dir: &'a Path,
    work_dir: &'a Path,
    model: String,
}

impl<'a> MacosInstaller<'a> {
    fn new(request: MacosInstallRequest<'a>) -> Result<Self> {
        let status_path = request.state_dir.join("status.json");
        let result_path = request.state_dir.join("result.json");
        let started_at = timestamp();
        let progress = ProgressStore::new(
            InstallerStatus::macos(
                started_at.clone(),
                status_path.display().to_string(),
                result_path.display().to_string(),
            ),
            status_path,
        )?;
        let result = MacosInstallerResult::pending(
            started_at,
            String::new(),
            request.workspace_id,
            request.model,
        );
        Ok(Self {
            workspace_id: request.workspace_id,
            native_script_path: request.native_script_path,
            work_dir: request.work_dir,
            progress,
            result_path,
            manifest: None,
            result,
        })
    }

    fn execute(&mut self) -> Result<SetupInstallation> {
        self.ensure_desktop_app()?;

        self.progress.set_step(
            5,
            InstallerStepState::Running,
            "正在创建百积木 LLM 凭证并写入 Codex 配置",
            None,
            None,
        )?;
        let auto_activate = credential::should_auto_activate_workspace_after_setup()?;
        let prepared = credential::initialize_workspace_profile(self.workspace_id)?;
        let profile_home = PathBuf::from(&prepared.profile.codex_home);
        self.result.codex_home = profile_home.display().to_string();
        self.result.llm_credential_created = true;
        self.result.config_written = true;
        self.result.auth_written = true;
        self.progress.set_step(
            5,
            InstallerStepState::Completed,
            "已使用百积木 LLM 凭证写入 Codex 配置",
            None,
            None,
        )?;

        self.progress.set_step(
            6,
            InstallerStepState::Skipped,
            "安装完成后由桌面管理器在后台验证百积木路由",
            None,
            None,
        )?;

        self.progress.set_step(
            7,
            InstallerStepState::Running,
            "正在提交工作区档案并启动桌面应用",
            None,
            None,
        )?;
        credential::finalize_workspace_setup(&prepared.profile, auto_activate)?;
        if !credential::codex_ready_for_workspace(self.workspace_id) {
            anyhow::bail!("安装配置完成，但独立工作区凭证归属回查失败");
        }
        let credential_state = credential::state()?;
        let workspace_profile_is_active = credential_state.active_mode
            == credential::AuthMode::Baijimu
            && credential_state.active_workspace_id == Some(self.workspace_id)
            && Path::new(&credential_state.active_codex_home) == profile_home;
        let outcome = if workspace_profile_is_active {
            launch_desktop_after_setup(&profile_home)
        } else {
            SetupCompletion::completed_without_desktop_launch()
        };
        if let Some(warning) = outcome.warning() {
            self.result.warnings.push(warning.to_string());
        }
        self.progress.set_step(
            7,
            InstallerStepState::Completed,
            outcome.message(),
            None,
            None,
        )?;
        self.progress
            .complete_pending(InstallerStepState::Skipped, "安装已完成")?;
        Ok(SetupInstallation {
            completion: outcome,
            router_credential: prepared.credential,
        })
    }

    fn ensure_desktop_app(&mut self) -> Result<()> {
        self.progress.set_step(
            1,
            InstallerStepState::Running,
            "正在检查 ChatGPT 桌面应用",
            None,
            None,
        )?;
        if let Some(path) = installed_app_path() {
            #[cfg(target_os = "macos")]
            crate::desktop::verify_system_compatibility()?;
            self.capture_app(&path, "already-installed")?;
            self.progress.set_step(
                1,
                InstallerStepState::Completed,
                "ChatGPT 桌面应用已安装",
                None,
                None,
            )?;
            self.progress.set_step(
                2,
                InstallerStepState::Skipped,
                "无需读取应用安装包清单",
                None,
                None,
            )?;
            self.progress.set_step(
                3,
                InstallerStepState::Skipped,
                "无需下载应用安装包",
                None,
                None,
            )?;
            self.progress.set_step(
                4,
                InstallerStepState::Skipped,
                "无需重新安装应用",
                None,
                None,
            )?;
            return Ok(());
        }

        self.progress.set_step(
            1,
            InstallerStepState::Completed,
            "未安装 ChatGPT 桌面应用，正在准备安装",
            None,
            None,
        )?;
        self.ensure_manifest(2)?;
        let asset = self.select_asset("codex_desktop_app")?.clone();
        #[cfg(target_os = "macos")]
        asset.ensure_current_macos_supported()?;
        self.progress.set_step(
            2,
            InstallerStepState::Completed,
            format!("已找到制品 {}", asset.name),
            None,
            None,
        )?;
        let archive = self.work_dir.join(&asset.name);
        self.download_to_path(
            &asset.mirror_url,
            &archive,
            3,
            "正在下载官方 ChatGPT 桌面应用安装包",
            Some(asset.size_bytes),
        )?;

        self.progress.set_step(
            4,
            InstallerStepState::Running,
            "正在校验并安装 ChatGPT 桌面应用",
            None,
            None,
        )?;
        let app_path =
            self.run_native_action("install-app", &[&archive, Path::new(&asset.sha256)])?;
        let app_path = PathBuf::from(app_path.trim());
        if !app_path.is_absolute()
            || app_path.extension().and_then(|value| value.to_str()) != Some("app")
        {
            anyhow::bail!("macOS 原生安装适配器返回了无效应用路径");
        }
        #[cfg(target_os = "macos")]
        crate::desktop::verify_system_compatibility()?;
        self.capture_app(&app_path, "baijimu-cache-dmg")?;
        self.progress.set_step(
            4,
            InstallerStepState::Completed,
            "ChatGPT 桌面应用已安装",
            None,
            None,
        )?;
        Ok(())
    }

    fn ensure_manifest(&mut self, step: usize) -> Result<()> {
        if self.manifest.is_some() {
            return Ok(());
        }
        let path = self.work_dir.join("latest.json");
        let manifest_url = super::source::manifest_url()?;
        self.download_to_path(&manifest_url, &path, step, "正在读取百积木安装包清单", None)?;
        let bytes = fs::read(&path).context("读取百积木安装包清单失败")?;
        let manifest = crate::json_compat::from_slice::<UpstreamManifestV4>(&bytes)
            .context("解析百积木安装包清单失败")?;
        manifest.validate()?;
        self.manifest = Some(manifest);
        Ok(())
    }

    fn select_asset(&self, component: &str) -> Result<&UpstreamAssetV4> {
        let arch = env::consts::ARCH;
        let matches = self
            .manifest
            .as_ref()
            .context("安装包清单尚未加载")?
            .assets
            .iter()
            .filter(|asset| {
                asset.component == component
                    && asset.platform == "macos"
                    && asset.arch == arch
                    && !asset.deprecated
            })
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [asset] => {
                validate_asset_file_name(&asset.name)?;
                Ok(asset)
            }
            [] => anyhow::bail!("百积木缓存不包含 macOS {arch} 的 {component} 制品"),
            _ => anyhow::bail!("百积木缓存包含多个 macOS {arch} 的 {component} 活动制品"),
        }
    }

    fn download_to_path(
        &mut self,
        url: &str,
        output: &Path,
        step: usize,
        detail: &str,
        total_hint: Option<u64>,
    ) -> Result<()> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(900))
            .build()
            .context("创建安装下载客户端失败")?;
        let mut response = client
            .get(url)
            .send()
            .with_context(|| format!("{detail}失败，下载地址：{url}"))?
            .error_for_status()
            .with_context(|| format!("{detail}失败，下载地址：{url}"))?;
        let total = total_hint.or_else(|| response.content_length());
        self.progress
            .set_step(step, InstallerStepState::Running, detail, Some(0), total)?;
        let mut file = File::create(output)
            .with_context(|| format!("创建下载文件失败: {}", output.display()))?;
        let mut buffer = [0_u8; 64 * 1024];
        let mut downloaded = 0_u64;
        let mut last_report = Instant::now();
        loop {
            let count = response.read(&mut buffer).context("读取安装下载响应失败")?;
            if count == 0 {
                break;
            }
            file.write_all(&buffer[..count])
                .context("写入安装下载文件失败")?;
            downloaded = downloaded.saturating_add(count as u64);
            if last_report.elapsed() >= Duration::from_secs(1) {
                self.progress.set_step(
                    step,
                    InstallerStepState::Running,
                    detail,
                    Some(downloaded),
                    total,
                )?;
                last_report = Instant::now();
            }
        }
        file.sync_all().context("同步安装下载文件失败")?;
        self.progress.set_step(
            step,
            InstallerStepState::Completed,
            detail,
            Some(downloaded),
            total,
        )?;
        Ok(())
    }

    fn run_native_action(&self, action: &str, arguments: &[&Path]) -> Result<String> {
        let output = Command::new("/bin/bash")
            .arg(self.native_script_path)
            .arg(action)
            .args(arguments)
            .output()
            .with_context(|| format!("启动 macOS 原生安装动作失败：{action}"))?;
        if !output.status.success() {
            let exit = output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let stderr = compact_error(&String::from_utf8_lossy(&output.stderr));
            anyhow::bail!(
                "macOS 原生安装动作失败：{action}（exit={exit}，stderr={}）",
                if stderr.is_empty() {
                    "<empty>"
                } else {
                    &stderr
                }
            );
        }
        String::from_utf8(output.stdout).context("macOS 原生安装动作返回了非 UTF-8 输出")
    }

    fn capture_app(&mut self, path: &Path, method: &str) -> Result<()> {
        if !path.is_dir() {
            anyhow::bail!("ChatGPT 桌面应用路径不存在：{}", path.display());
        }
        self.result.app_installed = true;
        self.result.app_install_method = method.to_string();
        self.result.app_path = path.display().to_string();
        self.result.version =
            read_app_metadata(path, "kMDItemVersion", "CFBundleShortVersionString");
        self.result.bundle_id =
            read_app_metadata(path, "kMDItemCFBundleIdentifier", "CFBundleIdentifier");
        Ok(())
    }

    fn fail_progress(&mut self, message: &str) {
        let _ = self.progress.fail_current(message);
        let _ = self
            .progress
            .complete_pending(InstallerStepState::Skipped, "安装已停止");
    }

    fn write_result(&self) -> Result<()> {
        atomic_write_private(&self.result_path, &serde_json::to_vec_pretty(&self.result)?)
    }
}

struct ProgressStore {
    status: InstallerStatus,
    path: PathBuf,
}

impl ProgressStore {
    fn new(status: InstallerStatus, path: PathBuf) -> Result<Self> {
        let store = Self { status, path };
        store.persist()?;
        Ok(store)
    }

    fn set_step(
        &mut self,
        index: usize,
        state: InstallerStepState,
        detail: impl Into<String>,
        downloaded_bytes: Option<u64>,
        total_bytes: Option<u64>,
    ) -> Result<()> {
        self.status.update_step(
            index,
            state,
            detail,
            downloaded_bytes,
            total_bytes,
            timestamp(),
        )?;
        self.persist()
    }

    fn fail_current(&mut self, message: &str) -> Result<()> {
        let index = self.status.current_step;
        if index == 0 {
            return Ok(());
        }
        let (downloaded, total) = self
            .status
            .steps
            .get(index - 1)
            .map(|step| (step.downloaded_bytes, step.total_bytes))
            .unwrap_or((None, None));
        self.set_step(
            index,
            InstallerStepState::Failed,
            message,
            downloaded,
            total,
        )
    }

    fn complete_pending(
        &mut self,
        state: InstallerStepState,
        detail: impl Into<String>,
    ) -> Result<()> {
        self.status.complete_pending(state, detail, timestamp());
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        atomic_write_private(&self.path, &serde_json::to_vec_pretty(&self.status)?)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpstreamManifestV4 {
    schema_version: u32,
    manifest_kind: String,
    source: String,
    snapshot_id: String,
    fetched_at: String,
    components: UpstreamComponentsV4,
    upstream_release: UpstreamReleaseV4,
    required_assets: Vec<String>,
    assets: Vec<UpstreamAssetV4>,
}

impl UpstreamManifestV4 {
    fn validate(&self) -> Result<()> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            anyhow::bail!("不支持的百积木安装包清单版本：{}", self.schema_version);
        }
        if self.manifest_kind != MANIFEST_KIND {
            anyhow::bail!("百积木安装包清单类型无效：{}", self.manifest_kind);
        }
        let required_identity = [
            self.source.as_str(),
            self.snapshot_id.as_str(),
            self.fetched_at.as_str(),
            self.components.codex_desktop_app.source.as_str(),
            self.components.codex_desktop_app.version_identity.as_str(),
            self.upstream_release.tag_name.as_str(),
            self.upstream_release.name.as_str(),
            self.upstream_release.published_at.as_str(),
            self.upstream_release.html_url.as_str(),
        ];
        if required_identity
            .iter()
            .any(|value| value.trim().is_empty())
            || self.required_assets.is_empty()
            || self.assets.is_empty()
        {
            anyhow::bail!("百积木安装包清单缺少必需的版本身份信息");
        }
        let mut names = HashSet::new();
        for asset in &self.assets {
            asset.validate()?;
            if !names.insert(asset.name.as_str()) {
                anyhow::bail!("百积木安装包清单包含重复制品：{}", asset.name);
            }
        }
        if self
            .assets
            .iter()
            .any(|asset| asset.component != "codex_desktop_app")
        {
            anyhow::bail!("桌面安装包清单不得包含 Codex CLI 制品");
        }
        for required in &self.required_assets {
            if required.trim().is_empty() || !names.contains(required.as_str()) {
                anyhow::bail!("百积木安装包清单缺少必需制品：{required}");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpstreamComponentsV4 {
    codex_desktop_app: CodexDesktopComponentV4,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodexDesktopComponentV4 {
    source: String,
    version_identity: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpstreamReleaseV4 {
    tag_name: String,
    name: String,
    published_at: String,
    html_url: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpstreamAssetV4 {
    name: String,
    component: String,
    platform: String,
    arch: String,
    source_kind: String,
    upstream_url: String,
    effective_upstream_url: String,
    upstream_sha256: String,
    signature_verification: Option<String>,
    install_layout: Option<String>,
    host_requirements: Option<ArtifactHostRequirementsV4>,
    deprecated: bool,
    mirror_url: String,
    object_key: String,
    sha256: String,
    size: u64,
    size_bytes: u64,
    content_type: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactHostRequirementsV4 {
    minimum_os_version: String,
}

impl UpstreamAssetV4 {
    fn validate(&self) -> Result<()> {
        validate_asset_file_name(&self.name)?;
        let required = [
            self.component.as_str(),
            self.platform.as_str(),
            self.arch.as_str(),
            self.source_kind.as_str(),
            self.upstream_url.as_str(),
            self.effective_upstream_url.as_str(),
            self.upstream_sha256.as_str(),
            self.mirror_url.as_str(),
            self.object_key.as_str(),
            self.sha256.as_str(),
            self.content_type.as_str(),
        ];
        let host_requirements_valid = match self.component.as_str() {
            "codex_desktop_app" => self.host_requirements.as_ref().is_some_and(|requirements| {
                crate::system_compatibility::validate_version(&requirements.minimum_os_version)
                    .is_ok()
            }),
            _ => false,
        };
        if required.iter().any(|value| value.trim().is_empty())
            || self.size == 0
            || self.size != self.size_bytes
            || !host_requirements_valid
            || self
                .signature_verification
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            || self
                .install_layout
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
        {
            anyhow::bail!("百积木安装包清单包含无效制品记录：{}", self.name);
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn ensure_current_macos_supported(&self) -> Result<()> {
        let minimum = self
            .host_requirements
            .as_ref()
            .context("ChatGPT/Codex macOS 制品缺少最低系统版本要求")?
            .minimum_os_version
            .as_str();
        let current = crate::system_compatibility::current_macos_version()?;
        crate::system_compatibility::ensure_supported("macOS", &current, minimum, "ChatGPT/Codex")
    }
}

fn validate_asset_file_name(name: &str) -> Result<()> {
    let path = Path::new(name);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => anyhow::bail!("安装包清单包含无效文件名：{name}"),
    }
}

fn installed_app_path() -> Option<PathBuf> {
    [TARGET_APP_PATH, LEGACY_APP_PATH]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_dir())
}

fn read_app_metadata(app_path: &Path, metadata_key: &str, plist_key: &str) -> String {
    let metadata = Command::new("mdls")
        .args(["-raw", "-name", metadata_key])
        .arg(app_path)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty() && value != "(null)");
    metadata.unwrap_or_else(|| {
        Command::new("defaults")
            .arg("read")
            .arg(app_path.join("Contents/Info"))
            .arg(plist_key)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_default()
    })
}

fn timestamp() -> String {
    Command::new("/bin/date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .to_string()
        })
}

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn create(prefix: &str) -> Result<Self> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = env::temp_dir().join(format!("{prefix}-{}-{unique}", std::process::id()));
        fs::create_dir(&path)
            .with_context(|| format!("创建临时安装目录失败: {}", path.display()))?;
        set_private_directory(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_manifest_is_deserialized_into_a_closed_contract() {
        let manifest = crate::json_compat::from_slice::<UpstreamManifestV4>(include_bytes!(
            "../../test/fixtures/codex-desktop-artifacts-manifest-v4.json"
        ))
        .unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.assets.len(), 1);
        assert_eq!(manifest.assets[0].component, "codex_desktop_app");
    }

    #[test]
    fn desktop_manifest_rejects_cli_assets() {
        let result = crate::json_compat::from_slice::<UpstreamManifestV4>(include_bytes!(
            "../../test/fixtures/codex-artifacts-manifest-v4.json"
        ));

        assert!(result.is_err());
    }

    #[test]
    fn customer_manifest_rejects_unknown_fields() {
        let mut payload = serde_json::from_slice::<serde_json::Value>(include_bytes!(
            "../../test/fixtures/codex-artifacts-manifest-v4.json"
        ))
        .unwrap();
        payload
            .as_object_mut()
            .unwrap()
            .insert("unexpected".to_string(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<UpstreamManifestV4>(payload).is_err());
    }

    #[test]
    fn desktop_assets_require_a_numeric_minimum_os_version() {
        let mut manifest = crate::json_compat::from_slice::<UpstreamManifestV4>(include_bytes!(
            "../../test/fixtures/codex-desktop-artifacts-manifest-v4.json"
        ))
        .unwrap();
        manifest.assets.truncate(1);
        manifest.required_assets = vec![manifest.assets[0].name.clone()];
        manifest.assets[0].host_requirements = None;
        assert!(manifest.validate().is_err());

        manifest.assets[0].host_requirements = Some(ArtifactHostRequirementsV4 {
            minimum_os_version: "14 beta".to_string(),
        });
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn desktop_manifest_rejects_unknown_asset_components() {
        let mut manifest = crate::json_compat::from_slice::<UpstreamManifestV4>(include_bytes!(
            "../../test/fixtures/codex-desktop-artifacts-manifest-v4.json"
        ))
        .unwrap();
        manifest.assets[0].component = "unknown".to_string();

        assert!(manifest.validate().is_err());
    }

    #[test]
    fn asset_file_names_cannot_escape_the_installer_workspace() {
        assert!(validate_asset_file_name("codex-app.dmg").is_ok());
        assert!(validate_asset_file_name("../codex-app.dmg").is_err());
        assert!(validate_asset_file_name("nested/codex-app.dmg").is_err());
    }
}
