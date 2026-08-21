use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use toml_edit::{value, DocumentMut, Item, Table};

use crate::{baijimu_cli, user_environment};

mod contract;
pub use contract::*;
mod store;
use store::*;

const METADATA_VERSION: u32 = 9;
const METADATA_FILE: &str = "codex-credentials.json";
const OWNERSHIP_MARKER_FILE: &str = ".baijimu-owner.json";
const OWNERSHIP_RESERVATION_FILE: &str = ".baijimu-owner.pending.json";
const OWNERSHIP_SCHEMA_VERSION: u32 = 3;
const LEGACY_OWNERSHIP_SCHEMA_VERSION: u32 = 1;
const OWNERSHIP_OWNER: &str = "baijimu-codex-desktop";
const LEGACY_OWNERSHIP_OWNER: &str = "baijimu-connector-codex";
const OWNED_AUTH_FILE: &str = "auth.json";
const OWNED_CONFIG_FILE: &str = "config.toml";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialMetadata {
    version: u32,
    #[serde(default)]
    profiles: Vec<CredentialProfile>,
    #[serde(default)]
    active_mode: AuthMode,
    #[serde(default)]
    active_profile_id: Option<String>,
    // Kept only so v1 metadata can be read and migrated without losing its selection.
    #[serde(default)]
    active_workspace_id: Option<u64>,
    #[serde(default)]
    original_codex_home_state: OriginalCodexHomeState,
    #[serde(default)]
    legacy_global_codex_home_restored_at_epoch_seconds: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CodexHomeOwnership {
    schema_version: u32,
    owner: String,
    initialized_at_epoch_seconds: u64,
    managed_files: Vec<String>,
    #[serde(default)]
    profile_key: Option<String>,
}

impl Default for CredentialMetadata {
    fn default() -> Self {
        Self {
            version: METADATA_VERSION,
            profiles: Vec::new(),
            active_mode: AuthMode::Baijimu,
            active_profile_id: None,
            active_workspace_id: None,
            original_codex_home_state: OriginalCodexHomeState::default(),
            legacy_global_codex_home_restored_at_epoch_seconds: None,
        }
    }
}

pub fn state() -> Result<CredentialManagerState> {
    let mut metadata = load_metadata()?;
    let shared_home = default_original_codex_home();
    let chatgpt = ChatGptProfileState {
        available: false,
        configured: false,
        auth_mode: None,
        account_id: None,
        codex_home: shared_home.display().to_string(),
    };
    let auth_status = baijimu_cli::auth_status();
    let mut warning = auth_status.as_ref().err().map(ToString::to_string);
    let (current_workspace_id, authorized_workspace_ids) = match auth_status.as_ref() {
        Ok(status) => (
            status.current_workspace_id,
            status
                .workspace_ids
                .iter()
                .copied()
                .collect::<BTreeSet<_>>(),
        ),
        Err(_) => (None, BTreeSet::new()),
    };
    let discovered = if auth_status
        .as_ref()
        .is_ok_and(|status| status.authenticated)
    {
        match baijimu_cli::list_workspaces() {
            Ok(workspaces) => Some(workspaces),
            Err(error) => {
                warning = Some(format!("暂时无法通过 baijimu CLI 读取工作区：{error}"));
                None
            }
        }
    } else {
        None
    };
    let mut workspaces =
        merge_workspace_options(&authorized_workspace_ids, discovered.as_deref(), &metadata);

    for profile in &mut metadata.profiles {
        normalize_profile(profile);
        if let Some(workspace) = workspaces
            .iter()
            .find(|item| item.workspace_id == profile.workspace_id)
        {
            profile.workspace_name = workspace.name.clone();
        }
        profile.codex_home = shared_home.display().to_string();
        profile.credential_status = match read_codex_api_key(&profile_credential_path(profile)) {
            Ok(Some(_)) => "configured".to_string(),
            Ok(None) => "missing".to_string(),
            Err(_) => "invalid".to_string(),
        };
    }

    let mut active_profile = metadata
        .active_profile_id
        .as_deref()
        .and_then(|profile_id| {
            metadata
                .profiles
                .iter()
                .find(|profile| profile.profile_id == profile_id)
                .cloned()
        });
    let active_home = shared_home.clone();
    let auth_path = shared_home.join("auth.json");
    let config_path = shared_home.join("config.toml");
    let external_codex_home = user_environment::read_codex_home()?;
    let legacy_global_codex_home =
        legacy_global_codex_home_state(&metadata, external_codex_home.as_deref());
    let mut credential_status = "not_configured".to_string();
    let mut codex_configured = false;

    if metadata.active_mode == AuthMode::Baijimu {
        if let Some(profile) = active_profile.as_mut() {
            let has_credential = read_codex_api_key(&auth_path).ok().flatten().is_some();
            codex_configured = has_credential && managed_config_ready(&config_path);
            if codex_configured {
                credential_status = "verified".to_string();
                profile.credential_status = "verified".to_string();
            }
        }
    }

    workspaces.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(CredentialManagerState {
        active_mode: AuthMode::Baijimu,
        current_workspace_id,
        active_workspace_id: active_profile.as_ref().map(|profile| profile.workspace_id),
        codex_configured,
        credential_status,
        active_profile,
        profiles: metadata.profiles,
        workspaces,
        chatgpt,
        discovery_warning: warning,
        original_codex_home_state: metadata.original_codex_home_state.clone(),
        original_codex_home: shared_home.display().to_string(),
        active_codex_home: active_home.display().to_string(),
        external_codex_home: external_codex_home
            .as_ref()
            .map(|path| path.display().to_string()),
        legacy_global_codex_home,
        codex_auth_path: auth_path.display().to_string(),
        codex_config_path: config_path.display().to_string(),
    })
}

pub fn initialize_workspace_profile(workspace_id: u64) -> Result<PreparedWorkspaceProfile> {
    let product = crate::product_config::get();
    let workspace = authorized_workspace(workspace_id)?;
    let mut metadata = load_metadata()?;
    let existing_profile = select_workspace_profile(&metadata, workspace_id).cloned();
    let profile_preexisting = existing_profile.is_some();
    let mut profile = if let Some(mut profile) = existing_profile {
        profile.workspace_name = workspace.name;
        profile.codex_home = default_original_codex_home().display().to_string();
        profile
    } else {
        let auth_status = baijimu_cli::auth_status().context("读取 baijimu CLI 授权状态失败")?;
        let environment = auth_status.base_url;
        let profile_id = profile_id(&environment, None, None, workspace_id);
        CredentialProfile {
            profile_id,
            environment,
            user_id: None,
            client_id: None,
            workspace_id,
            workspace_name: workspace.name,
            model: product.default_model.clone(),
            activated_at_epoch_seconds: 0,
            codex_home: default_original_codex_home().display().to_string(),
            credential_status: "verified".to_string(),
        }
    };
    let credential = initialize_workspace_files(&profile, !profile_preexisting, || {
        baijimu_cli::create_llm_credential(workspace_id)
            .context("baijimu CLI 签发工作区 LLM credential 失败")
    })?;
    if metadata.active_profile_id.is_none() {
        sync_credential_to_shared_home(&credential)?;
    }
    profile.credential_status = "verified".to_string();
    let profile_id = profile.profile_id.clone();
    metadata
        .profiles
        .retain(|item| item.profile_id != profile_id);
    metadata.profiles.push(profile.clone());
    sort_profiles(&mut metadata.profiles);
    save_metadata(&metadata)?;
    Ok(PreparedWorkspaceProfile {
        profile,
        credential,
    })
}

pub fn prepare_workspace_activation(workspace_id: u64) -> Result<CredentialProfile> {
    authorized_workspace(workspace_id)?;
    let metadata = load_metadata()?;
    let profile = select_workspace_profile(&metadata, workspace_id)
        .cloned()
        .context("该工作区尚未初始化，请先初始化后再启动")?;
    if read_codex_api_key(&profile_credential_path(&profile))
        .context("该工作区 auth.json 无法读取，请先重新授权")?
        .is_none()
    {
        anyhow::bail!("该工作区授权已缺失，请先重新授权");
    }
    if !managed_config_ready(&default_original_codex_home().join(OWNED_CONFIG_FILE)) {
        anyhow::bail!("默认 .codex 缺少百积木配置，请先重新初始化");
    }
    Ok(profile)
}

fn initialize_workspace_files<F>(
    profile: &CredentialProfile,
    may_issue_initial_credential: bool,
    issue_credential: F,
) -> Result<String>
where
    F: FnOnce() -> Result<String>,
{
    let auth_path = profile_credential_path(profile);
    let credential = match read_codex_api_key(&auth_path) {
        Ok(Some(credential)) => credential,
        Ok(None) if may_issue_initial_credential => {
            let credential = issue_credential()?;
            write_workspace_auth(&auth_path, &credential)?;
            credential
        }
        Ok(None) => anyhow::bail!("该工作区授权已缺失，请使用重新授权，不得通过初始化覆盖"),
        Err(error) if !may_issue_initial_credential => {
            return Err(error).context("该工作区 auth.json 已损坏，请使用重新授权")
        }
        Err(error) => return Err(error),
    };
    ensure_workspace_config(&default_original_codex_home().join("config.toml"))?;
    commit_shared_home_ownership()?;
    Ok(credential)
}

pub fn prepare_workspace_reauthorization(
    workspace_id: u64,
) -> Result<PreparedWorkspaceReauthorization> {
    authorized_workspace(workspace_id)?;
    let metadata = load_metadata()?;
    let profile = select_workspace_profile(&metadata, workspace_id)
        .cloned()
        .context("该工作区尚未初始化，不能重新授权")?;
    let credential = baijimu_cli::create_llm_credential(workspace_id)
        .context("baijimu CLI 重新签发工作区 LLM credential 失败")?;
    Ok(PreparedWorkspaceReauthorization {
        profile,
        credential,
    })
}

pub fn commit_workspace_reauthorization(
    prepared: PreparedWorkspaceReauthorization,
) -> Result<CredentialProfile> {
    let mut metadata = load_metadata()?;
    let mut profile = prepared.profile;
    write_workspace_auth(&profile_credential_path(&profile), &prepared.credential)?;
    let is_active = metadata.active_profile_id.as_deref() == Some(profile.profile_id.as_str());
    if is_active {
        sync_credential_to_shared_home(&prepared.credential)?;
    }
    profile.credential_status = "verified".to_string();
    metadata
        .profiles
        .retain(|item| item.profile_id != profile.profile_id);
    metadata.profiles.push(profile.clone());
    sort_profiles(&mut metadata.profiles);
    save_metadata(&metadata)?;
    Ok(profile)
}

fn authorized_workspace(workspace_id: u64) -> Result<baijimu_cli::Workspace> {
    if workspace_id == 0 {
        anyhow::bail!("工作区 ID 必须大于 0");
    }
    let auth_status = baijimu_cli::auth_status().context("读取 baijimu CLI 授权状态失败")?;
    if !auth_status.authenticated || !auth_status.workspace_ids.contains(&workspace_id) {
        anyhow::bail!("baijimu CLI 授权不包含该工作区，请先重新完成设备授权");
    }
    baijimu_cli::get_workspace(workspace_id).context("baijimu CLI 无法确认当前工作区授权")
}

fn select_workspace_profile(
    metadata: &CredentialMetadata,
    workspace_id: u64,
) -> Option<&CredentialProfile> {
    metadata
        .profiles
        .iter()
        .filter(|profile| profile.workspace_id == workspace_id)
        .max_by_key(|profile| {
            (
                metadata.active_profile_id.as_deref() == Some(profile.profile_id.as_str()),
                profile.activated_at_epoch_seconds,
            )
        })
}

pub fn activate_prepared_workspace_profile(
    prepared: &CredentialProfile,
) -> Result<CredentialProfile> {
    let previous = load_metadata()?;
    let previous_credential = previous
        .active_profile_id
        .as_deref()
        .and_then(|id| {
            previous
                .profiles
                .iter()
                .find(|profile| profile.profile_id == id)
        })
        .and_then(|profile| {
            read_codex_api_key(&profile_credential_path(profile))
                .ok()
                .flatten()
        });
    let mut metadata = previous.clone();
    let credential = read_codex_api_key(&profile_credential_path(prepared))?
        .context("该工作区授权已缺失，请先重新授权")?;
    ensure_workspace_config(&default_original_codex_home().join("config.toml"))?;
    sync_credential_to_shared_home(&credential)?;
    commit_shared_home_ownership()?;
    let activated_at = now_epoch_seconds();
    for profile in &mut metadata.profiles {
        if profile.profile_id == prepared.profile_id {
            profile.activated_at_epoch_seconds = activated_at;
        }
    }
    metadata.active_mode = AuthMode::Baijimu;
    metadata.active_profile_id = Some(prepared.profile_id.clone());
    metadata.active_workspace_id = Some(prepared.workspace_id);
    if let Err(error) = save_metadata(&metadata) {
        if let Some(previous_credential) = previous_credential {
            sync_credential_to_shared_home(&previous_credential)
                .context("激活元数据保存失败，且恢复上一工作区凭证失败")?;
        }
        return Err(error).context("保存当前工作区失败");
    }
    metadata
        .profiles
        .into_iter()
        .find(|item| item.profile_id == prepared.profile_id)
        .context("激活后未找到工作区凭证档案")
}

pub fn restore_legacy_global_codex_home() -> Result<CredentialManagerState> {
    let mut metadata = load_metadata()?;
    let current = user_environment::read_codex_home()?;
    let migration = legacy_global_codex_home_state(&metadata, current.as_deref());
    if !migration.restore_required {
        return state();
    }
    if !migration.can_restore {
        anyhow::bail!("当前用户级 CODEX_HOME 无法证明由旧版 Connector 设置，未进行修改");
    }
    let restore_value = metadata
        .original_codex_home_state
        .value
        .as_deref()
        .map(Path::new);
    user_environment::restore_codex_home(restore_value)?;
    metadata.legacy_global_codex_home_restored_at_epoch_seconds = Some(now_epoch_seconds());
    save_metadata(&metadata).context("用户级 CODEX_HOME 已恢复，但迁移审计状态写入失败")?;
    state()
}

pub fn should_auto_activate_workspace_after_setup() -> Result<bool> {
    let metadata = load_metadata()?;
    Ok(metadata.active_profile_id.is_none())
}

pub fn finalize_workspace_setup(profile: &CredentialProfile, auto_activate: bool) -> Result<()> {
    if auto_activate {
        activate_prepared_workspace_profile(profile)?;
    }
    if !codex_ready_for_workspace(profile.workspace_id) {
        anyhow::bail!("工作区凭证未完成配置");
    }
    commit_shared_home_ownership()?;
    Ok(())
}

pub fn codex_ready_for_workspace(workspace_id: u64) -> bool {
    let metadata = load_metadata().ok();
    metadata.is_some_and(|metadata| {
        metadata.profiles.iter().any(|profile| {
            profile.workspace_id == workspace_id
                && read_codex_api_key(&profile_credential_path(profile))
                    .ok()
                    .flatten()
                    .is_some()
                && managed_config_ready(&default_original_codex_home().join("config.toml"))
        })
    })
}

pub(crate) fn router_credential_for_workspace(workspace_id: u64) -> Result<String> {
    let metadata = load_metadata()?;
    let profile = metadata
        .profiles
        .iter()
        .filter(|profile| profile.workspace_id == workspace_id)
        .max_by_key(|profile| {
            (
                metadata.active_profile_id.as_deref() == Some(profile.profile_id.as_str()),
                profile.activated_at_epoch_seconds,
            )
        })
        .context("未找到工作区 Codex 凭证档案")?;
    read_codex_api_key(&profile_credential_path(profile))?
        .context("工作区 Codex 凭证文件中缺少 OPENAI_API_KEY")
}

fn write_workspace_auth(path: &Path, credential: &str) -> Result<()> {
    atomic_write_private(
        path,
        &serde_json::to_vec_pretty(&json!({
            "OPENAI_API_KEY": credential,
            "auth_mode": "apikey"
        }))?,
    )?;
    verify_private_file(path)
}

fn ensure_workspace_config(path: &Path) -> Result<()> {
    let product = crate::product_config::get();
    let mut document = if path.exists() {
        let content = fs::read_to_string(path)
            .with_context(|| format!("读取 Codex 配置失败: {}", path.display()))?;
        crate::json_compat::strip_utf8_bom_str(&content)
            .parse::<DocumentMut>()
            .context("解析 Codex config.toml 失败")?
    } else {
        DocumentMut::new()
    };
    document["model"] = value(product.default_model.as_str());
    document["model_provider"] = value(product.router_provider.as_str());
    document["sandbox_mode"] = value("danger-full-access");
    document["approval_policy"] = value("on-request");
    document["cli_auth_credentials_store"] = value("file");
    document["forced_login_method"] = value("api");
    if document.get("desktop").and_then(Item::as_table).is_none() {
        document["desktop"] = Item::Table(Table::new());
    }
    document["desktop"]["localeOverride"] = value(product.default_ui_locale.as_str());
    if document
        .get("model_providers")
        .and_then(Item::as_table)
        .is_none()
    {
        document["model_providers"] = Item::Table(Table::new());
    }
    if document["model_providers"]
        .as_table()
        .and_then(|table| table.get(product.router_provider.as_str()))
        .and_then(Item::as_table)
        .is_none()
    {
        document["model_providers"][product.router_provider.as_str()] = Item::Table(Table::new());
    }
    let provider = &mut document["model_providers"][product.router_provider.as_str()];
    provider["name"] = value(product.router_provider.as_str());
    provider["base_url"] = value(product.router_base_url.as_str());
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(true);
    let rendered = document.to_string();
    if fs::read(path).ok().as_deref() != Some(rendered.as_bytes()) {
        atomic_write_private(path, rendered.as_bytes())?;
    }
    verify_private_file(path)
}

fn managed_config_ready(path: &Path) -> bool {
    let product = crate::product_config::get();
    fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            crate::json_compat::strip_utf8_bom_str(&text)
                .parse::<DocumentMut>()
                .ok()
        })
        .is_some_and(|doc| {
            doc.get("model_provider").and_then(Item::as_str)
                == Some(product.router_provider.as_str())
                && doc
                    .get("model_providers")
                    .and_then(Item::as_table)
                    .and_then(|table| table.get(product.router_provider.as_str()))
                    .and_then(Item::as_table)
                    .and_then(|table| table.get("base_url"))
                    .and_then(Item::as_str)
                    == Some(product.router_base_url.as_str())
        })
}

fn merge_workspace_options(
    authorized_workspace_ids: &BTreeSet<u64>,
    discovered: Option<&[baijimu_cli::Workspace]>,
    metadata: &CredentialMetadata,
) -> Vec<WorkspaceOption> {
    let mut ids = discovered
        .into_iter()
        .flatten()
        .map(|item| item.id)
        .collect::<BTreeSet<_>>();
    ids.extend(authorized_workspace_ids.iter().copied());
    ids.extend(metadata.profiles.iter().map(|item| item.workspace_id));
    ids.into_iter()
        .map(|workspace_id| {
            let name = discovered
                .into_iter()
                .flatten()
                .find(|item| item.id == workspace_id)
                .map(|item| item.name.clone())
                .or_else(|| {
                    metadata
                        .profiles
                        .iter()
                        .find(|item| item.workspace_id == workspace_id)
                        .map(|item| item.workspace_name.clone())
                })
                .unwrap_or_else(|| format!("工作区 {workspace_id}"));
            let user_ids = metadata
                .profiles
                .iter()
                .filter(|item| item.workspace_id == workspace_id)
                .filter_map(|profile| profile.user_id)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            WorkspaceOption {
                workspace_id,
                name,
                authorized: discovered.is_some()
                    && authorized_workspace_ids.contains(&workspace_id),
                configured: metadata.profiles.iter().any(|item| {
                    item.workspace_id == workspace_id && workspace_profile_initialized(item)
                }),
                user_ids,
            }
        })
        .collect()
}

fn workspace_profile_initialized(profile: &CredentialProfile) -> bool {
    read_codex_api_key(&profile_credential_path(profile))
        .ok()
        .flatten()
        .is_some()
        && managed_config_ready(&default_original_codex_home().join("config.toml"))
}

fn capture_original_codex_home(metadata: &mut CredentialMetadata) -> Result<bool> {
    if metadata.original_codex_home_state.captured {
        return Ok(false);
    }
    let current = user_environment::read_codex_home()?;
    let managed_pointer = current.as_ref().is_some_and(|path| {
        path.starts_with(legacy_managed_profile_root())
            || path.starts_with(managed_profile_root())
            || metadata
                .profiles
                .iter()
                .any(|profile| Path::new(&profile.codex_home) == path)
    });
    metadata.original_codex_home_state = if managed_pointer {
        OriginalCodexHomeState {
            captured: true,
            value: None,
            capture_source: "inferred-default-before-managed-pointer".to_string(),
        }
    } else {
        OriginalCodexHomeState {
            captured: true,
            value: current.map(|path| path.display().to_string()),
            capture_source: "user-environment".to_string(),
        }
    };
    Ok(true)
}

fn default_original_codex_home() -> PathBuf {
    home_dir().join(".codex")
}

fn is_managed_codex_home(metadata: &CredentialMetadata, path: &Path) -> bool {
    path.starts_with(legacy_managed_profile_root())
        || path.starts_with(managed_profile_root())
        || metadata
            .profiles
            .iter()
            .any(|profile| Path::new(&profile.codex_home) == path)
}

fn legacy_global_codex_home_state(
    metadata: &CredentialMetadata,
    current: Option<&Path>,
) -> LegacyGlobalCodexHomeState {
    let restore_required = current.is_some_and(|path| is_managed_codex_home(metadata, path));
    LegacyGlobalCodexHomeState {
        restore_required,
        can_restore: restore_required && metadata.original_codex_home_state.captured,
        current_value: current.map(|path| path.display().to_string()),
        restore_value: metadata.original_codex_home_state.value.clone(),
        restored_at_epoch_seconds: metadata.legacy_global_codex_home_restored_at_epoch_seconds,
    }
}

fn normalize_profile(profile: &mut CredentialProfile) {
    if profile.environment.is_empty() {
        profile.environment = default_environment();
    }
    if profile.profile_id.is_empty() {
        profile.profile_id = profile_id(
            &profile.environment,
            profile.user_id,
            profile.client_id.as_deref(),
            profile.workspace_id,
        );
    }
    if profile.codex_home.is_empty() {
        profile.codex_home = default_original_codex_home().display().to_string();
    }
}

fn profile_credential_path(profile: &CredentialProfile) -> PathBuf {
    connector_data_dir()
        .join("workspace-credentials")
        .join(profile_short_key(&profile.profile_id))
        .join(OWNED_AUTH_FILE)
}

fn migrate_profiles_to_shared_home(metadata: &mut CredentialMetadata) -> Result<bool> {
    let shared_home = default_original_codex_home();
    let mut changed = false;
    for profile in &mut metadata.profiles {
        let previous_home = PathBuf::from(&profile.codex_home);
        let credential_path = profile_credential_path(profile);
        if !credential_path.exists() {
            if let Some(credential) = read_codex_api_key(&previous_home.join(OWNED_AUTH_FILE))? {
                write_workspace_auth(&credential_path, &credential)?;
            }
        }
        if previous_home != shared_home {
            profile.codex_home = shared_home.display().to_string();
            changed = true;
        }
    }
    Ok(changed)
}

fn sync_profile_to_shared_home(profile: &CredentialProfile) -> Result<()> {
    let credential = read_codex_api_key(&profile_credential_path(profile))?
        .context("活动工作区的凭证库中缺少 OPENAI_API_KEY")?;
    ensure_workspace_config(&default_original_codex_home().join(OWNED_CONFIG_FILE))?;
    sync_credential_to_shared_home(&credential)?;
    commit_shared_home_ownership()
}

fn sync_credential_to_shared_home(credential: &str) -> Result<()> {
    let path = default_original_codex_home().join(OWNED_AUTH_FILE);
    if read_codex_api_key(&path)?.as_deref() == Some(credential) {
        return Ok(());
    }
    write_workspace_auth(&path, credential)
}

fn commit_shared_home_ownership() -> Result<()> {
    let home = default_original_codex_home();
    fs::create_dir_all(&home)
        .with_context(|| format!("创建默认 Codex 状态目录失败: {}", home.display()))?;
    set_private_directory(&home)?;
    if !managed_config_ready(&home.join(OWNED_CONFIG_FILE)) {
        anyhow::bail!("默认 .codex 尚未完成百积木配置");
    }
    let marker = CodexHomeOwnership {
        schema_version: OWNERSHIP_SCHEMA_VERSION,
        owner: OWNERSHIP_OWNER.to_string(),
        initialized_at_epoch_seconds: now_epoch_seconds(),
        managed_files: vec![OWNED_AUTH_FILE.to_string(), OWNED_CONFIG_FILE.to_string()],
        profile_key: None,
    };
    let path = home.join(OWNERSHIP_MARKER_FILE);
    if read_valid_ownership(&home)
        .ok()
        .flatten()
        .as_ref()
        .is_some_and(|current| {
            current.schema_version == marker.schema_version
                && current.owner == marker.owner
                && current.managed_files == marker.managed_files
                && current.profile_key.is_none()
        })
    {
        return Ok(());
    }
    atomic_write_private(&path, &serde_json::to_vec_pretty(&marker)?)?;
    read_valid_ownership(&home)?.context("百积木 Codex 所有权标记写入后无法回读")?;
    let reservation = home.join(OWNERSHIP_RESERVATION_FILE);
    if reservation.exists() {
        fs::remove_file(&reservation)?;
    }
    Ok(())
}

fn sort_profiles(profiles: &mut [CredentialProfile]) {
    profiles.sort_by(|left, right| {
        (&left.workspace_name, &left.profile_id).cmp(&(&right.workspace_name, &right.profile_id))
    });
}

fn read_codex_api_key(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read(path).with_context(|| format!("读取 Codex 认证文件失败: {}", path.display()))?;
    let value: Value = crate::json_compat::from_slice(&content)
        .with_context(|| format!("解析 Codex 认证文件失败: {}", path.display()))?;
    Ok(value
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned))
}

fn profile_id(
    environment: &str,
    user_id: Option<u64>,
    client_id: Option<&str>,
    workspace_id: u64,
) -> String {
    format!(
        "{}:user-{}:client-{}:workspace-{workspace_id}",
        sanitize_path_segment(environment),
        user_id.unwrap_or_default(),
        sanitize_path_segment(client_id.unwrap_or("local"))
    )
}

fn profile_short_key(profile_id: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(profile_id.as_bytes()));
    digest[..24].to_string()
}

fn managed_profile_root() -> PathBuf {
    home_dir().join(".baijimu").join("codex").join("p")
}

fn legacy_managed_profile_root() -> PathBuf {
    connector_data_dir().join("codex-profiles")
}

fn read_valid_ownership(home: &Path) -> Result<Option<CodexHomeOwnership>> {
    let path = home.join(OWNERSHIP_MARKER_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read(&path)
        .with_context(|| format!("读取百积木 Codex 所有权标记失败: {}", path.display()))?;
    let marker: CodexHomeOwnership = crate::json_compat::from_slice(&content)
        .with_context(|| format!("解析百积木 Codex 所有权标记失败: {}", path.display()))?;
    let expected_files = vec![OWNED_AUTH_FILE.to_string(), OWNED_CONFIG_FILE.to_string()];
    let supported_schema = matches!(
        marker.schema_version,
        OWNERSHIP_SCHEMA_VERSION | 2 | LEGACY_OWNERSHIP_SCHEMA_VERSION
    );
    let valid_profile_key = match marker.schema_version {
        OWNERSHIP_SCHEMA_VERSION => marker.profile_key.is_none(),
        2 => marker
            .profile_key
            .as_ref()
            .is_some_and(|key| key.len() == 24 && key.bytes().all(|byte| byte.is_ascii_hexdigit())),
        LEGACY_OWNERSHIP_SCHEMA_VERSION => marker.profile_key.is_none(),
        _ => false,
    };
    if !supported_schema
        || !matches!(
            marker.owner.as_str(),
            OWNERSHIP_OWNER | LEGACY_OWNERSHIP_OWNER
        )
        || marker.managed_files != expected_files
        || !valid_profile_key
    {
        anyhow::bail!(
            "百积木 Codex 所有权标记不受当前版本支持: {}",
            path.display()
        );
    }
    Ok(Some(marker))
}

fn sanitize_path_segment(value: &str) -> String {
    let value = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if value.is_empty() {
        "default".to_string()
    } else {
        value
    }
}

fn default_environment() -> String {
    "prod".to_string()
}

#[cfg(all(test, any()))]
mod legacy_profile_home_tests {
    use super::*;
    use crate::user_environment::TEST_ENVIRONMENT_LOCK as ENVIRONMENT_LOCK;
    use std::ffi::OsString;
    struct EnvironmentRestore {
        key: &'static str,
        previous: Option<OsString>,
    }
    impl EnvironmentRestore {
        fn set(key: &'static str, value: &Path) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }
    impl Drop for EnvironmentRestore {
        fn drop(&mut self) {
            match self.previous.as_ref() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn reads_windows_chatgpt_auth_with_utf8_bom() {
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-chatgpt-auth-bom-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("auth.json"),
            "\u{feff}{\"auth_mode\":\"chatgpt\",\"tokens\":{\"access_token\":\"personal-token\",\"account_id\":\"account-1\"}}",
        )
        .unwrap();

        let state = read_chatgpt_state(&root).unwrap();

        assert!(state.configured);
        assert_eq!(state.auth_mode.as_deref(), Some("chatgpt"));
        assert_eq!(state.account_id.as_deref(), Some("account-1"));
        assert_eq!(state.codex_home, root.display().to_string());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_malformed_chatgpt_auth_after_bom_normalization() {
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-chatgpt-auth-invalid-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("auth.json"), "\u{feff}{invalid-json").unwrap();

        let error = read_chatgpt_state(&root).unwrap_err();

        assert!(error.to_string().contains("解析 ChatGPT 登录状态失败"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_workspace_api_key_with_utf8_bom() {
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-workspace-auth-bom-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        fs::create_dir_all(&root).unwrap();
        let auth_path = root.join("auth.json");
        fs::write(
            &auth_path,
            "\u{feff}{\"OPENAI_API_KEY\":\"workspace-key\",\"auth_mode\":\"apikey\"}",
        )
        .unwrap();

        assert_eq!(
            read_codex_api_key(&auth_path).unwrap().as_deref(),
            Some("workspace-key")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_metadata_is_migrated_into_connector_data_directory() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-metadata-test-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let config_home = root.join("config");
        let data_dir = root.join("connector-data");
        fs::create_dir_all(config_home.join("baijimu")).unwrap();
        let _config = EnvironmentRestore::set("BAIJIMU_CONFIG_HOME", &config_home);
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);
        let mut legacy = vec![0xef, 0xbb, 0xbf];
        legacy.extend(serde_json::to_vec_pretty(&json!({"version":1,"profiles":[{"workspaceId":12,"workspaceName":"测试工作区","model":crate::product_config::get().default_model,"activatedAtEpochSeconds":56}],"activeWorkspaceId":12})).unwrap());
        fs::write(legacy_metadata_path(), legacy).unwrap();
        let metadata = load_metadata().unwrap();
        assert_eq!(metadata.active_mode, AuthMode::Baijimu);
        assert!(metadata.active_profile_id.is_some());
        assert!(metadata_path().exists());
        assert!(!legacy_metadata_path().exists());
        verify_private_file(&metadata_path()).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_identity_is_scoped_by_environment_user_and_workspace() {
        assert_eq!(
            profile_id("prod", Some(25), Some("device-a"), 1390),
            "prod:user-25:client-device-a:workspace-1390"
        );
        assert_ne!(
            profile_id("prod", Some(25), Some("device-a"), 1390),
            profile_id("test", Some(25), Some("device-a"), 1390)
        );
        assert_ne!(
            profile_id("prod", Some(25), Some("device-a"), 1390),
            profile_id("prod", Some(26), Some("device-a"), 1390)
        );
        assert_ne!(
            profile_id("prod", Some(25), Some("device-a"), 1390),
            profile_id("prod", Some(25), Some("device-b"), 1390)
        );
    }

    #[test]
    fn workspace_profile_home_is_short_stable_and_contains_no_business_identifiers() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-short-profile-home-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let user_home = root.join("user");
        fs::create_dir_all(&user_home).unwrap();
        let _home = EnvironmentRestore::set("HOME", &user_home);
        let _user_profile = EnvironmentRestore::set("USERPROFILE", &user_home);

        let first = workspace_profile_home("prod", Some(25), Some("device-a"), 1390);
        let second = workspace_profile_home("prod", Some(25), Some("device-a"), 1390);
        let different = workspace_profile_home("prod", Some(25), Some("device-b"), 1390);

        assert_eq!(first, second);
        assert_ne!(first, different);
        assert_eq!(first.parent().unwrap(), managed_profile_root());
        let key = first.file_name().unwrap().to_string_lossy();
        assert_eq!(key.len(), 24);
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let rendered = first.display().to_string();
        assert!(!rendered.contains("workspace-1390"));
        assert!(!rendered.contains("device-a"));
        assert!(!rendered.contains("user-25"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_new_user_binds_the_first_workspace_to_the_default_codex_home() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-default-profile-home-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let user_home = root.join("user");
        fs::create_dir_all(&user_home).unwrap();
        let _home = EnvironmentRestore::set("HOME", &user_home);
        let _user_profile = EnvironmentRestore::set("USERPROFILE", &user_home);
        let _codex = EnvironmentRestore::unset("CODEX_HOME");

        let selected = select_new_profile_home(
            &CredentialMetadata::default(),
            "prod",
            Some(25),
            Some("device-a"),
            1390,
        )
        .unwrap();

        assert_eq!(selected, user_home.join(".codex"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_existing_unowned_default_home_keeps_new_workspace_isolated() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-unowned-default-home-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let user_home = root.join("user");
        fs::create_dir_all(user_home.join(".codex")).unwrap();
        fs::write(user_home.join(".codex/user-state"), b"keep").unwrap();
        let _home = EnvironmentRestore::set("HOME", &user_home);
        let _user_profile = EnvironmentRestore::set("USERPROFILE", &user_home);
        let _codex = EnvironmentRestore::unset("CODEX_HOME");

        let selected = select_new_profile_home(
            &CredentialMetadata::default(),
            "prod",
            Some(25),
            Some("device-a"),
            1390,
        )
        .unwrap();

        assert_eq!(selected.parent().unwrap(), managed_profile_root());
        assert_ne!(selected, user_home.join(".codex"));
        assert_eq!(
            fs::read(user_home.join(".codex/user-state")).unwrap(),
            b"keep"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_second_workspace_cannot_take_the_default_home_binding() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-single-default-binding-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let user_home = root.join("user");
        let data_dir = root.join("connector-data");
        fs::create_dir_all(&user_home).unwrap();
        let _home = EnvironmentRestore::set("HOME", &user_home);
        let _user_profile = EnvironmentRestore::set("USERPROFILE", &user_home);
        let _codex = EnvironmentRestore::unset("CODEX_HOME");
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);
        let mut first = test_workspace_profile(&data_dir, 642);
        first.codex_home = user_home.join(".codex").display().to_string();
        let metadata = CredentialMetadata {
            profiles: vec![first],
            original_codex_home_state: OriginalCodexHomeState {
                captured: true,
                value: None,
                capture_source: "test".to_string(),
            },
            ..CredentialMetadata::default()
        };

        let selected =
            select_new_profile_home(&metadata, "prod", Some(25), Some("device-a"), 1390).unwrap();

        assert_eq!(selected.parent().unwrap(), managed_profile_root());
        assert_ne!(selected, user_home.join(".codex"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn v4_active_legacy_profile_is_atomically_migrated_to_the_default_home() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-profile-migration-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let user_home = root.join("user");
        let data_dir = root.join("connector-data");
        let legacy_home = data_dir
            .join("codex-profiles")
            .join("baijimu")
            .join("prod")
            .join("user-25")
            .join("client-device-a")
            .join("workspace-1390");
        fs::create_dir_all(legacy_home.join("sessions/2026/08/11")).unwrap();
        fs::write(legacy_home.join("state_5.sqlite"), b"sqlite-state").unwrap();
        fs::write(
            legacy_home.join("sessions/2026/08/11/thread.jsonl"),
            b"thread-state",
        )
        .unwrap();
        fs::create_dir_all(&user_home).unwrap();
        let _home = EnvironmentRestore::set("HOME", &user_home);
        let _user_profile = EnvironmentRestore::set("USERPROFILE", &user_home);
        let _codex = EnvironmentRestore::unset("CODEX_HOME");
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);
        let profile_id = "prod:user-25:client-device-a:workspace-1390";
        let profile = CredentialProfile {
            profile_id: profile_id.to_string(),
            environment: "prod".to_string(),
            user_id: Some(25),
            client_id: Some("device-a".to_string()),
            workspace_id: 1390,
            workspace_name: "迁移工作区".to_string(),
            model: crate::product_config::get().default_model.clone(),
            activated_at_epoch_seconds: 1,
            codex_home: legacy_home.display().to_string(),
            credential_status: "verified".to_string(),
        };
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(
            metadata_path(),
            serde_json::to_vec_pretty(&CredentialMetadata {
                version: 4,
                profiles: vec![profile],
                active_mode: AuthMode::Baijimu,
                active_profile_id: Some(profile_id.to_string()),
                active_workspace_id: Some(1390),
                original_codex_home_state: OriginalCodexHomeState {
                    captured: true,
                    value: None,
                    capture_source: "test".to_string(),
                },
                ..CredentialMetadata::default()
            })
            .unwrap(),
        )
        .unwrap();

        let migrated_home = user_home.join(".codex");
        let metadata = load_metadata().unwrap();
        assert_eq!(metadata.version, METADATA_VERSION);
        assert_eq!(
            metadata.profiles[0].codex_home,
            migrated_home.display().to_string()
        );
        assert!(!legacy_home.exists());
        assert_eq!(
            fs::read(migrated_home.join("state_5.sqlite")).unwrap(),
            b"sqlite-state"
        );
        assert_eq!(
            fs::read(migrated_home.join("sessions/2026/08/11/thread.jsonl")).unwrap(),
            b"thread-state"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_profile_migration_recovers_after_rename_before_metadata_save() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-profile-migration-recovery-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let user_home = root.join("user");
        let data_dir = root.join("connector-data");
        let legacy_home = data_dir.join("codex-profiles/workspace-1390");
        let profile_id = "prod:user-25:client-device-a:workspace-1390";
        fs::create_dir_all(&user_home).unwrap();
        let _home = EnvironmentRestore::set("HOME", &user_home);
        let _user_profile = EnvironmentRestore::set("USERPROFILE", &user_home);
        let _codex = EnvironmentRestore::unset("CODEX_HOME");
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);
        let short_home = profile_home_for_id(profile_id);
        fs::create_dir_all(&short_home).unwrap();
        fs::write(short_home.join("state_5.sqlite"), b"recovered").unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        let mut profile = test_workspace_profile(&data_dir, 1390);
        profile.codex_home = legacy_home.display().to_string();
        fs::write(
            metadata_path(),
            serde_json::to_vec_pretty(&CredentialMetadata {
                version: 4,
                profiles: vec![profile],
                active_mode: AuthMode::Baijimu,
                active_profile_id: Some(profile_id.to_string()),
                active_workspace_id: Some(1390),
                original_codex_home_state: OriginalCodexHomeState {
                    captured: true,
                    value: None,
                    capture_source: "test".to_string(),
                },
                ..CredentialMetadata::default()
            })
            .unwrap(),
        )
        .unwrap();

        let metadata = load_metadata().unwrap();
        let migrated_home = user_home.join(".codex");
        assert_eq!(
            metadata.profiles[0].codex_home,
            migrated_home.display().to_string()
        );
        assert_eq!(
            fs::read(migrated_home.join("state_5.sqlite")).unwrap(),
            b"recovered"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn v5_active_private_profile_migrates_to_default_home_and_commits_binding() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-v5-default-home-migration-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let user_home = root.join("user");
        let data_dir = root.join("connector-data");
        fs::create_dir_all(&user_home).unwrap();
        let _home = EnvironmentRestore::set("HOME", &user_home);
        let _user_profile = EnvironmentRestore::set("USERPROFILE", &user_home);
        let _codex = EnvironmentRestore::unset("CODEX_HOME");
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);
        let mut profile = test_workspace_profile(&data_dir, 642);
        profile.codex_home = profile_home_for_id(&profile.profile_id)
            .display()
            .to_string();
        let private_home = PathBuf::from(&profile.codex_home);
        write_workspace_auth(&private_home.join(OWNED_AUTH_FILE), "workspace-token").unwrap();
        write_workspace_config(&private_home.join(OWNED_CONFIG_FILE)).unwrap();
        fs::write(private_home.join("state_5.sqlite"), b"workspace-state").unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(
            metadata_path(),
            serde_json::to_vec_pretty(&CredentialMetadata {
                version: 5,
                profiles: vec![profile.clone()],
                active_mode: AuthMode::Baijimu,
                active_profile_id: Some(profile.profile_id.clone()),
                active_workspace_id: Some(profile.workspace_id),
                original_codex_home_state: OriginalCodexHomeState {
                    captured: true,
                    value: None,
                    capture_source: "test".to_string(),
                },
                ..CredentialMetadata::default()
            })
            .unwrap(),
        )
        .unwrap();

        let default_home = user_home.join(".codex");
        let metadata = load_metadata().unwrap();
        assert_eq!(
            metadata.profiles[0].codex_home,
            default_home.display().to_string()
        );
        assert!(!private_home.exists());
        assert_eq!(
            fs::read(default_home.join("state_5.sqlite")).unwrap(),
            b"workspace-state"
        );
        let ownership = read_valid_ownership(&default_home).unwrap().unwrap();
        assert_eq!(
            ownership.profile_key,
            Some(profile_short_key(&profile.profile_id))
        );
        assert!(!default_home.join(OWNERSHIP_RESERVATION_FILE).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn switching_back_to_bound_workspace_restores_default_codex_home() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-bound-workspace-switch-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let user_home = root.join("user");
        let data_dir = root.join("connector-data");
        fs::create_dir_all(user_home.join(".codex")).unwrap();
        let _home = EnvironmentRestore::set("HOME", &user_home);
        let _user_profile = EnvironmentRestore::set("USERPROFILE", &user_home);
        let _codex = EnvironmentRestore::unset("CODEX_HOME");
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);
        let mut bound = test_workspace_profile(&data_dir, 642);
        bound.codex_home = user_home.join(".codex").display().to_string();
        let isolated = test_workspace_profile(&data_dir, 1390);
        save_metadata(&CredentialMetadata {
            profiles: vec![bound.clone(), isolated.clone()],
            active_mode: AuthMode::Baijimu,
            active_profile_id: Some(isolated.profile_id.clone()),
            active_workspace_id: Some(isolated.workspace_id),
            original_codex_home_state: OriginalCodexHomeState {
                captured: true,
                value: None,
                capture_source: "test".to_string(),
            },
            ..CredentialMetadata::default()
        })
        .unwrap();

        activate_prepared_workspace_profile(&bound).unwrap();
        assert_eq!(active_codex_home(), user_home.join(".codex"));
        activate_prepared_workspace_profile(&isolated).unwrap();
        assert_eq!(active_codex_home(), PathBuf::from(&isolated.codex_home));
        activate_prepared_workspace_profile(&bound).unwrap();
        assert_eq!(active_codex_home(), user_home.join(".codex"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_profile_migration_preserves_both_directories_on_collision() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-profile-migration-collision-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let user_home = root.join("user");
        let data_dir = root.join("connector-data");
        let legacy_home = data_dir.join("codex-profiles/workspace-1390");
        fs::create_dir_all(&legacy_home).unwrap();
        fs::write(legacy_home.join("source"), b"source").unwrap();
        fs::create_dir_all(&user_home).unwrap();
        let _home = EnvironmentRestore::set("HOME", &user_home);
        let _user_profile = EnvironmentRestore::set("USERPROFILE", &user_home);
        let _codex = EnvironmentRestore::unset("CODEX_HOME");
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);
        let mut profile = test_workspace_profile(&data_dir, 1390);
        profile.codex_home = legacy_home.display().to_string();
        let migrated_home = profile_home_for_id(&profile.profile_id);
        fs::create_dir_all(&migrated_home).unwrap();
        fs::write(migrated_home.join("target"), b"target").unwrap();
        fs::write(
            metadata_path(),
            serde_json::to_vec_pretty(&CredentialMetadata {
                version: 4,
                profiles: vec![profile],
                original_codex_home_state: OriginalCodexHomeState {
                    captured: true,
                    value: None,
                    capture_source: "test".to_string(),
                },
                ..CredentialMetadata::default()
            })
            .unwrap(),
        )
        .unwrap();

        let error = load_metadata().unwrap_err();
        assert!(error.to_string().contains("源目录和目标目录同时存在"));
        assert_eq!(fs::read(legacy_home.join("source")).unwrap(), b"source");
        assert_eq!(fs::read(migrated_home.join("target")).unwrap(), b"target");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn v2_managed_pointer_migrates_to_an_unset_original_value() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-managed-pointer-migration-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let user_home = root.join("user");
        let data_dir = root.join("connector-data");
        let managed_home = data_dir
            .join("codex-profiles")
            .join("baijimu")
            .join("prod")
            .join("user-25")
            .join("client-device-a")
            .join("workspace-1203");
        fs::create_dir_all(&managed_home).unwrap();
        fs::create_dir_all(&user_home).unwrap();
        let _home = EnvironmentRestore::set("HOME", &user_home);
        let _user_profile = EnvironmentRestore::set("USERPROFILE", &user_home);
        let _codex = EnvironmentRestore::set("CODEX_HOME", &managed_home);
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);
        let profile = CredentialProfile {
            profile_id: "prod:user-25:client-device-a:workspace-1203".to_string(),
            environment: "prod".to_string(),
            user_id: Some(25),
            client_id: Some("device-a".to_string()),
            workspace_id: 1203,
            workspace_name: "工作区 1203".to_string(),
            model: crate::product_config::get().default_model.clone(),
            activated_at_epoch_seconds: 1,
            codex_home: managed_home.display().to_string(),
            credential_status: "verified".to_string(),
        };
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(
            metadata_path(),
            serde_json::to_vec_pretty(&json!({
                "version": 2,
                "profiles": [profile],
                "activeMode": "baijimu",
                "activeProfileId": "prod:user-25:client-device-a:workspace-1203",
                "activeWorkspaceId": 1203
            }))
            .unwrap(),
        )
        .unwrap();

        let metadata = load_metadata().unwrap();
        assert!(metadata.original_codex_home_state.captured);
        assert_eq!(metadata.original_codex_home_state.value, None);
        assert_eq!(
            metadata.original_codex_home_state.capture_source,
            "inferred-default-before-managed-pointer"
        );
        assert_eq!(original_codex_home(), user_home.join(".codex"));

        assert_eq!(
            std::env::var_os("CODEX_HOME"),
            Some(managed_home.into_os_string())
        );
        let migration = state().unwrap().legacy_global_codex_home;
        assert!(migration.restore_required);
        assert!(migration.can_restore);
        restore_legacy_global_codex_home().unwrap();
        assert_eq!(std::env::var_os("CODEX_HOME"), None);
        assert_eq!(load_metadata().unwrap().active_mode, AuthMode::Baijimu);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn internal_profile_switch_does_not_change_the_external_codex_home() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-personal-baseline-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let personal_home = root.join("personal");
        let data_dir = root.join("connector-data");
        fs::create_dir_all(&personal_home).unwrap();
        let _codex = EnvironmentRestore::set("CODEX_HOME", &personal_home);
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);
        let _config = EnvironmentRestore::unset("BAIJIMU_CONFIG_HOME");
        let profile = test_workspace_profile(&data_dir, 1203);
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(
            metadata_path(),
            serde_json::to_vec_pretty(&json!({
                "version": 2,
                "profiles": [profile],
                "activeMode": "chatgpt",
                "activeProfileId": null,
                "activeWorkspaceId": null
            }))
            .unwrap(),
        )
        .unwrap();

        let metadata = load_metadata().unwrap();
        assert_eq!(
            metadata.original_codex_home_state.value.as_deref(),
            Some(personal_home.to_string_lossy().as_ref())
        );
        let profile = metadata.profiles[0].clone();
        activate_prepared_workspace_profile(&profile).unwrap();
        assert_eq!(
            std::env::var_os("CODEX_HOME"),
            Some(personal_home.clone().into_os_string())
        );
        assert_eq!(original_codex_home(), personal_home);

        activate_chatgpt_profile().unwrap();
        assert_eq!(
            std::env::var_os("CODEX_HOME"),
            Some(personal_home.into_os_string())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_activation_only_updates_connector_metadata() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-pointer-rollback-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let original_home = root.join("original");
        let data_dir = root.join("connector-data");
        fs::create_dir_all(&original_home).unwrap();
        let _codex = EnvironmentRestore::set("CODEX_HOME", &original_home);
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);
        let profile = test_workspace_profile(&data_dir, 1203);
        save_metadata(&CredentialMetadata {
            profiles: vec![profile.clone()],
            original_codex_home_state: OriginalCodexHomeState {
                captured: true,
                value: Some(original_home.display().to_string()),
                capture_source: "user-environment".to_string(),
            },
            ..CredentialMetadata::default()
        })
        .unwrap();

        activate_prepared_workspace_profile(&profile).unwrap();
        assert_eq!(load_metadata().unwrap().active_mode, AuthMode::Baijimu);
        assert_eq!(
            std::env::var_os("CODEX_HOME"),
            Some(original_home.into_os_string())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_activation_preserves_original_login_and_existing_selection() {
        let original = CredentialMetadata::default();
        assert!(!should_auto_activate_workspace(&original, true));
        assert!(should_auto_activate_workspace(&original, false));

        let profile = CredentialProfile {
            profile_id: "prod:user-25:client-device-a:workspace-1390".to_string(),
            environment: "prod".to_string(),
            user_id: Some(25),
            client_id: Some("device-a".to_string()),
            workspace_id: 1390,
            workspace_name: "既有工作区".to_string(),
            model: crate::product_config::get().default_model.clone(),
            activated_at_epoch_seconds: 1,
            codex_home: "/isolated/workspace-1390".to_string(),
            credential_status: "verified".to_string(),
        };
        let selected = CredentialMetadata {
            version: METADATA_VERSION,
            profiles: vec![profile.clone()],
            active_mode: AuthMode::Baijimu,
            active_profile_id: Some(profile.profile_id),
            active_workspace_id: Some(profile.workspace_id),
            ..CredentialMetadata::default()
        };
        assert!(!should_auto_activate_workspace(&selected, false));
    }

    #[test]
    fn existing_chatgpt_files_remain_byte_identical_after_workspace_setup() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-existing-user-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let personal_home = root.join("personal-codex");
        let data_dir = root.join("connector-data");
        fs::create_dir_all(&personal_home).unwrap();
        let _codex = EnvironmentRestore::set("CODEX_HOME", &personal_home);
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);

        let personal_auth = br#"{"auth_mode":"chatgpt","tokens":{"access_token":"personal-token","account_id":"account-1"}}"#;
        let personal_config = b"model = \"personal-model\"\n";
        fs::write(personal_home.join("auth.json"), personal_auth).unwrap();
        fs::write(personal_home.join("config.toml"), personal_config).unwrap();
        let profile = test_workspace_profile(&data_dir, 642);
        write_workspace_auth(
            &Path::new(&profile.codex_home).join("auth.json"),
            "workspace-token",
        )
        .unwrap();
        write_workspace_config(&Path::new(&profile.codex_home).join("config.toml")).unwrap();
        save_metadata(&CredentialMetadata {
            profiles: vec![profile.clone()],
            ..CredentialMetadata::default()
        })
        .unwrap();

        assert!(!should_auto_activate_workspace_after_setup().unwrap());
        finalize_workspace_setup(&profile, false).unwrap();
        assert_eq!(
            fs::read(personal_home.join("auth.json")).unwrap(),
            personal_auth
        );
        assert_eq!(
            fs::read(personal_home.join("config.toml")).unwrap(),
            personal_config
        );
        assert_eq!(load_metadata().unwrap().active_mode, AuthMode::Chatgpt);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn clean_install_automatically_activates_isolated_workspace_profile() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-new-user-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let personal_home = root.join("personal-codex");
        let data_dir = root.join("connector-data");
        fs::create_dir_all(&personal_home).unwrap();
        let _codex = EnvironmentRestore::set("CODEX_HOME", &personal_home);
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);

        let profile = test_workspace_profile(&data_dir, 642);
        write_workspace_auth(
            &Path::new(&profile.codex_home).join("auth.json"),
            "workspace-token",
        )
        .unwrap();
        write_workspace_config(&Path::new(&profile.codex_home).join("config.toml")).unwrap();
        save_metadata(&CredentialMetadata {
            profiles: vec![profile.clone()],
            ..CredentialMetadata::default()
        })
        .unwrap();

        assert!(should_auto_activate_workspace_after_setup().unwrap());
        finalize_workspace_setup(&profile, true).unwrap();
        let metadata = load_metadata().unwrap();
        assert_eq!(metadata.active_mode, AuthMode::Baijimu);
        assert_eq!(
            metadata.active_profile_id.as_deref(),
            Some(profile.profile_id.as_str())
        );
        assert_eq!(metadata.active_workspace_id, Some(642));
        assert!(!personal_home.join("auth.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_home_ownership_marker_binds_a_profile_without_business_identifiers() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-default-home-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let user_home = root.join("user");
        let data_dir = root.join("connector-data");
        fs::create_dir_all(&user_home).unwrap();
        let _home = EnvironmentRestore::set("HOME", &user_home);
        let _profile = EnvironmentRestore::set("USERPROFILE", &user_home);
        let _codex = EnvironmentRestore::unset("CODEX_HOME");
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);
        let default_home = user_home.join(".codex");

        write_workspace_auth(&default_home.join("auth.json"), "workspace-token").unwrap();
        write_workspace_config(&default_home.join("config.toml")).unwrap();
        let mut profile = test_workspace_profile(&data_dir, 642);
        profile.codex_home = default_home.display().to_string();
        commit_default_home_ownership(&profile).unwrap();

        let marker_content = fs::read_to_string(default_home.join(OWNERSHIP_MARKER_FILE)).unwrap();
        let marker_json: Value = serde_json::from_str(&marker_content).unwrap();
        let marker = read_valid_ownership(&default_home).unwrap().unwrap();
        assert_eq!(marker.owner, OWNERSHIP_OWNER);
        assert_eq!(
            marker.managed_files,
            vec![OWNED_AUTH_FILE.to_string(), OWNED_CONFIG_FILE.to_string()]
        );
        assert_eq!(
            marker.profile_key,
            Some(profile_short_key(&profile.profile_id))
        );
        for forbidden_field in [
            "workspaceId",
            "workspaceName",
            "userId",
            "clientId",
            "environment",
            "profileId",
        ] {
            assert!(marker_json.get(forbidden_field).is_none());
        }
        assert!(!marker_content.contains(&profile.profile_id));
        assert!(!marker_content.contains("workspace-token"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repeated_initialization_reuses_existing_credential_and_preserves_user_config_bytes() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-idempotent-initialization-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        fs::create_dir_all(&root).unwrap();
        let auth = br#"{"OPENAI_API_KEY":"existing-key","auth_mode":"apikey","userField":"keep"}"#;
        let config = b"model = \"user-model\"\ncustom_setting = \"keep\"\n";
        fs::write(root.join("auth.json"), auth).unwrap();
        fs::write(root.join("config.toml"), config).unwrap();
        let issued = std::cell::Cell::new(false);

        let credential = initialize_workspace_files(&root, false, || {
            issued.set(true);
            Ok("new-key".to_string())
        })
        .unwrap();

        assert_eq!(credential, "existing-key");
        assert!(!issued.get());
        assert_eq!(fs::read(root.join("auth.json")).unwrap(), auth);
        assert_eq!(fs::read(root.join("config.toml")).unwrap(), config);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn initialized_workspace_with_missing_auth_requires_explicit_reauthorization() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-missing-auth-initialization-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        fs::create_dir_all(&root).unwrap();
        let config = b"model = \"user-model\"\ncustom_setting = \"keep\"\n";
        fs::write(root.join("config.toml"), config).unwrap();

        let issued = std::cell::Cell::new(false);
        let error = initialize_workspace_files(&root, false, || {
            issued.set(true);
            Ok("new-key".to_string())
        })
        .unwrap_err();

        assert!(error.to_string().contains("重新授权"));
        assert!(!issued.get());
        assert_eq!(fs::read(root.join("config.toml")).unwrap(), config);
        assert!(!root.join("auth.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incomplete_initialization_creates_missing_auth_and_default_config_once() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-new-profile-files-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let personal_home = root.join("personal");
        let profile_home = root.join("profile");
        fs::create_dir_all(&personal_home).unwrap();
        let _codex = EnvironmentRestore::set("CODEX_HOME", &personal_home);

        let credential =
            initialize_workspace_files(&profile_home, true, || Ok("initial-key".to_string()))
                .unwrap();

        assert_eq!(credential, "initial-key");
        assert_eq!(
            read_codex_api_key(&profile_home.join("auth.json"))
                .unwrap()
                .as_deref(),
            Some("initial-key")
        );
        assert!(managed_config_ready(&profile_home.join("config.toml")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn activating_existing_workspace_does_not_touch_auth_or_config() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-pure-launch-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let data_dir = root.join("connector-data");
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);
        let profile = test_workspace_profile(&data_dir, 642);
        let home = PathBuf::from(&profile.codex_home);
        fs::create_dir_all(&home).unwrap();
        let auth = br#"{"OPENAI_API_KEY":"existing-key","auth_mode":"apikey","userField":"keep"}"#;
        let config = b"model = \"user-model\"\ncustom_setting = \"keep\"\n";
        fs::write(home.join("auth.json"), auth).unwrap();
        fs::write(home.join("config.toml"), config).unwrap();
        save_metadata(&CredentialMetadata {
            profiles: vec![profile],
            ..CredentialMetadata::default()
        })
        .unwrap();

        activate_existing_workspace_profile(642).unwrap();

        assert_eq!(fs::read(home.join("auth.json")).unwrap(), auth);
        assert_eq!(fs::read(home.join("config.toml")).unwrap(), config);
        let metadata = load_metadata().unwrap();
        assert_eq!(metadata.active_mode, AuthMode::Baijimu);
        assert_eq!(metadata.active_workspace_id, Some(642));
        fs::remove_dir_all(root).unwrap();
    }

    fn test_workspace_profile(data_dir: &Path, workspace_id: u64) -> CredentialProfile {
        let profile_id = format!("prod:user-25:client-device-a:workspace-{workspace_id}");
        let profile_key = profile_short_key(&profile_id);
        CredentialProfile {
            profile_id,
            environment: "prod".to_string(),
            user_id: Some(25),
            client_id: Some("device-a".to_string()),
            workspace_id,
            workspace_name: format!("工作区 {workspace_id}"),
            model: crate::product_config::get().default_model.clone(),
            activated_at_epoch_seconds: 0,
            codex_home: data_dir
                .join("profile-homes")
                .join(profile_key)
                .display()
                .to_string(),
            credential_status: "verified".to_string(),
        }
    }
}

#[cfg(test)]
mod shared_home_tests {
    use super::*;
    use crate::user_environment::TEST_ENVIRONMENT_LOCK;
    use std::ffi::OsString;

    struct EnvRestore {
        values: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvRestore {
        fn set(values: &[(&'static str, &Path)]) -> Self {
            let previous = values
                .iter()
                .map(|(key, value)| {
                    let previous = std::env::var_os(key);
                    std::env::set_var(key, value);
                    (*key, previous)
                })
                .collect();
            Self { values: previous }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, value) in self.values.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn profile(workspace_id: u64, home: &Path) -> CredentialProfile {
        CredentialProfile {
            profile_id: profile_id("prod", None, None, workspace_id),
            environment: "prod".to_string(),
            user_id: None,
            client_id: None,
            workspace_id,
            workspace_name: format!("workspace-{workspace_id}"),
            model: crate::product_config::get().default_model.clone(),
            activated_at_epoch_seconds: 0,
            codex_home: home.display().to_string(),
            credential_status: "verified".to_string(),
        }
    }

    #[test]
    fn migration_moves_only_credentials_and_preserves_legacy_state() {
        let _guard = TEST_ENVIRONMENT_LOCK.lock().unwrap();
        let root =
            std::env::temp_dir().join(format!("codex-shared-migration-{}", std::process::id()));
        let user_home = root.join("user");
        let data_home = root.join("data");
        let old_one = root.join("old-one");
        let old_two = root.join("old-two");
        fs::create_dir_all(&old_one).unwrap();
        fs::create_dir_all(&old_two).unwrap();
        fs::write(
            old_one.join("auth.json"),
            br#"{"OPENAI_API_KEY":"key-one"}"#,
        )
        .unwrap();
        fs::write(
            old_two.join("auth.json"),
            br#"{"OPENAI_API_KEY":"key-two"}"#,
        )
        .unwrap();
        fs::write(old_one.join("state_5.sqlite"), b"workspace-one-state").unwrap();
        fs::write(old_two.join("sessions.jsonl"), b"workspace-two-session").unwrap();
        let _env = EnvRestore::set(&[
            ("HOME", &user_home),
            ("USERPROFILE", &user_home),
            ("BAIJIMU_CONNECTOR_DATA_DIR", &data_home),
        ]);
        let mut metadata = CredentialMetadata {
            version: 8,
            profiles: vec![profile(1, &old_one), profile(2, &old_two)],
            active_mode: AuthMode::Baijimu,
            active_profile_id: None,
            active_workspace_id: None,
            original_codex_home_state: OriginalCodexHomeState::default(),
            legacy_global_codex_home_restored_at_epoch_seconds: None,
        };

        assert!(migrate_profiles_to_shared_home(&mut metadata).unwrap());
        let shared = user_home.join(".codex").display().to_string();
        assert!(metadata
            .profiles
            .iter()
            .all(|item| item.codex_home == shared));
        assert_eq!(
            read_codex_api_key(&profile_credential_path(&metadata.profiles[0]))
                .unwrap()
                .as_deref(),
            Some("key-one")
        );
        assert_eq!(
            read_codex_api_key(&profile_credential_path(&metadata.profiles[1]))
                .unwrap()
                .as_deref(),
            Some("key-two")
        );
        assert_eq!(
            fs::read(old_one.join("state_5.sqlite")).unwrap(),
            b"workspace-one-state"
        );
        assert_eq!(
            fs::read(old_two.join("sessions.jsonl")).unwrap(),
            b"workspace-two-session"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn credential_switch_preserves_sessions_and_unmanaged_config() {
        let _guard = TEST_ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("codex-shared-switch-{}", std::process::id()));
        let user_home = root.join("user");
        let data_home = root.join("data");
        let _env = EnvRestore::set(&[
            ("HOME", &user_home),
            ("USERPROFILE", &user_home),
            ("BAIJIMU_CONNECTOR_DATA_DIR", &data_home),
        ]);
        let shared = user_home.join(".codex");
        fs::create_dir_all(shared.join("sessions")).unwrap();
        fs::write(shared.join("sessions/thread.jsonl"), b"keep-session").unwrap();
        fs::write(shared.join("config.toml"), b"custom_setting = \"keep\"\n").unwrap();
        let profile = profile(642, &shared);
        write_workspace_auth(&profile_credential_path(&profile), "workspace-key").unwrap();

        sync_profile_to_shared_home(&profile).unwrap();

        assert_eq!(
            read_codex_api_key(&shared.join("auth.json"))
                .unwrap()
                .as_deref(),
            Some("workspace-key")
        );
        assert_eq!(
            fs::read(shared.join("sessions/thread.jsonl")).unwrap(),
            b"keep-session"
        );
        let config = fs::read_to_string(shared.join("config.toml")).unwrap();
        assert!(config.contains("custom_setting = \"keep\""));
        assert!(managed_config_ready(&shared.join("config.toml")));
        let marker = read_valid_ownership(&shared).unwrap().unwrap();
        assert_eq!(marker.schema_version, OWNERSHIP_SCHEMA_VERSION);
        assert_eq!(marker.profile_key, None);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reauthorization_updates_shared_auth_only_for_the_active_workspace() {
        let _guard = TEST_ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("codex-shared-reauth-{}", std::process::id()));
        let user_home = root.join("user");
        let data_home = root.join("data");
        let _env = EnvRestore::set(&[
            ("HOME", &user_home),
            ("USERPROFILE", &user_home),
            ("BAIJIMU_CONNECTOR_DATA_DIR", &data_home),
        ]);
        let shared = user_home.join(".codex");
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join("session-index.json"), b"keep-index").unwrap();
        ensure_workspace_config(&shared.join("config.toml")).unwrap();
        let first = profile(1, &shared);
        let second = profile(2, &shared);
        write_workspace_auth(&profile_credential_path(&first), "first-key").unwrap();
        write_workspace_auth(&profile_credential_path(&second), "second-key").unwrap();
        save_metadata(&CredentialMetadata {
            version: METADATA_VERSION,
            profiles: vec![first.clone(), second.clone()],
            active_mode: AuthMode::Baijimu,
            active_profile_id: Some(first.profile_id.clone()),
            active_workspace_id: Some(first.workspace_id),
            original_codex_home_state: OriginalCodexHomeState::default(),
            legacy_global_codex_home_restored_at_epoch_seconds: None,
        })
        .unwrap();
        sync_profile_to_shared_home(&first).unwrap();

        commit_workspace_reauthorization(PreparedWorkspaceReauthorization {
            profile: second.clone(),
            credential: "second-key-new".to_string(),
        })
        .unwrap();
        assert_eq!(
            read_codex_api_key(&shared.join("auth.json"))
                .unwrap()
                .as_deref(),
            Some("first-key")
        );

        commit_workspace_reauthorization(PreparedWorkspaceReauthorization {
            profile: first,
            credential: "first-key-new".to_string(),
        })
        .unwrap();
        assert_eq!(
            read_codex_api_key(&shared.join("auth.json"))
                .unwrap()
                .as_deref(),
            Some("first-key-new")
        );
        assert_eq!(
            fs::read(shared.join("session-index.json")).unwrap(),
            b"keep-index"
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
