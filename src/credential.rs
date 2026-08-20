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

const METADATA_VERSION: u32 = 7;
const METADATA_FILE: &str = "codex-credentials.json";
const CODEX_GLOBAL_STATE_FILE: &str = ".codex-global-state.json";
const DESKTOP_DEFAULTS_VERSION: u32 = 1;
const PERSISTED_ATOM_STATE_KEY: &str = "electron-persisted-atom-state";
const PERMISSION_MODE_VISIBILITY_KEY: &str = "composer-permission-mode-visibility";
const ONBOARDING_COMPLETED_KEY: &str = "electron:onboarding-projectless-completed";
const LAST_COMPLETED_ONBOARDING_KEY: &str = "last_completed_onboarding";
const OWNERSHIP_MARKER_FILE: &str = ".baijimu-owner.json";
const OWNERSHIP_RESERVATION_FILE: &str = ".baijimu-owner.pending.json";
const OWNERSHIP_SCHEMA_VERSION: u32 = 2;
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CodexHomeReservation {
    schema_version: u32,
    owner: String,
    reserved_at_epoch_seconds: u64,
    profile_key: String,
}

impl Default for CredentialMetadata {
    fn default() -> Self {
        Self {
            version: METADATA_VERSION,
            profiles: Vec::new(),
            active_mode: AuthMode::Chatgpt,
            active_profile_id: None,
            active_workspace_id: None,
            original_codex_home_state: OriginalCodexHomeState::default(),
            legacy_global_codex_home_restored_at_epoch_seconds: None,
        }
    }
}

pub fn state() -> Result<CredentialManagerState> {
    let mut metadata = load_metadata()?;
    let original_home = original_home_from_metadata(&metadata);
    let mut chatgpt = read_chatgpt_state(&original_home)?;
    chatgpt.available = !metadata
        .profiles
        .iter()
        .any(|profile| Path::new(&profile.codex_home) == original_home);
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
        profile.credential_status = if Path::new(&profile.codex_home).join("auth.json").is_file() {
            "configured".to_string()
        } else {
            "missing".to_string()
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
    let active_home = match metadata.active_mode {
        AuthMode::Chatgpt => original_home.clone(),
        AuthMode::Baijimu => active_profile
            .as_ref()
            .map(|profile| PathBuf::from(&profile.codex_home))
            .unwrap_or_else(|| original_home.clone()),
    };
    let auth_path = active_home.join("auth.json");
    let config_path = active_home.join("config.toml");
    let external_codex_home = user_environment::read_codex_home()?;
    let legacy_global_codex_home =
        legacy_global_codex_home_state(&metadata, external_codex_home.as_deref());
    let mut credential_status = match metadata.active_mode {
        AuthMode::Chatgpt if chatgpt.configured => "verified".to_string(),
        AuthMode::Chatgpt => "not_configured".to_string(),
        AuthMode::Baijimu => "not_configured".to_string(),
    };
    let mut codex_configured = match metadata.active_mode {
        AuthMode::Chatgpt => chatgpt.configured,
        AuthMode::Baijimu => false,
    };

    if metadata.active_mode == AuthMode::Baijimu {
        if let Some(profile) = active_profile.as_mut() {
            let has_credential = read_codex_api_key(&auth_path)?.is_some();
            codex_configured = has_credential && managed_config_ready(&config_path);
            if codex_configured {
                credential_status = "verified".to_string();
                profile.credential_status = "verified".to_string();
            }
        }
    }

    workspaces.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(CredentialManagerState {
        active_mode: metadata.active_mode.clone(),
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
        original_codex_home: original_home.display().to_string(),
        active_codex_home: active_home.display().to_string(),
        external_codex_home: external_codex_home
            .as_ref()
            .map(|path| path.display().to_string()),
        legacy_global_codex_home,
        codex_auth_path: auth_path.display().to_string(),
        codex_config_path: config_path.display().to_string(),
    })
}

pub fn prepare_workspace_profile(workspace_id: u64) -> Result<PreparedWorkspaceProfile> {
    let product = crate::product_config::get();
    if workspace_id == 0 {
        anyhow::bail!("工作区 ID 必须大于 0");
    }
    let auth_status = baijimu_cli::auth_status().context("读取 baijimu CLI 授权状态失败")?;
    if !auth_status.authenticated || !auth_status.workspace_ids.contains(&workspace_id) {
        anyhow::bail!("baijimu CLI 授权不包含该工作区，请先重新完成设备授权");
    }
    let workspace =
        baijimu_cli::get_workspace(workspace_id).context("baijimu CLI 无法确认当前工作区授权")?;
    let mut metadata = load_metadata()?;
    let existing_profile = metadata
        .profiles
        .iter()
        .filter(|profile| profile.workspace_id == workspace_id)
        .max_by_key(|profile| {
            (
                metadata.active_profile_id.as_deref() == Some(profile.profile_id.as_str()),
                profile.activated_at_epoch_seconds,
            )
        })
        .cloned();
    let (profile_id, environment, user_id, client_id, profile_home) =
        if let Some(profile) = existing_profile {
            (
                profile.profile_id,
                profile.environment,
                profile.user_id,
                profile.client_id,
                PathBuf::from(profile.codex_home),
            )
        } else {
            let environment = auth_status.base_url.clone();
            let profile_id = profile_id(&environment, None, None, workspace_id);
            let profile_home =
                select_new_profile_home(&metadata, &environment, None, None, workspace_id)?;
            if profile_home == default_original_codex_home() {
                reserve_default_home(&profile_home, &profile_id)?;
            }
            (profile_id, environment, None, None, profile_home)
        };
    let auth_path = profile_home.join("auth.json");
    let credential = baijimu_cli::create_llm_credential(workspace_id)
        .context("baijimu CLI 签发工作区 LLM credential 失败")?;
    write_workspace_auth(&auth_path, &credential)?;
    write_workspace_config(&profile_home.join("config.toml"))?;

    let previous_activation = metadata
        .profiles
        .iter()
        .find(|item| item.profile_id == profile_id)
        .map(|item| item.activated_at_epoch_seconds)
        .unwrap_or_default();
    let profile = CredentialProfile {
        profile_id: profile_id.clone(),
        environment,
        user_id,
        client_id,
        workspace_id,
        workspace_name: workspace.name,
        model: product.default_model.clone(),
        activated_at_epoch_seconds: previous_activation,
        codex_home: profile_home.display().to_string(),
        credential_status: "verified".to_string(),
        desktop_defaults_version: metadata
            .profiles
            .iter()
            .find(|item| item.profile_id == profile_id)
            .map(|item| item.desktop_defaults_version)
            .unwrap_or_default(),
    };
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

pub fn activate_prepared_workspace_profile(
    prepared: &CredentialProfile,
) -> Result<CredentialProfile> {
    let previous = load_metadata()?;
    let mut metadata = previous.clone();
    let activated_at = now_epoch_seconds();
    for profile in &mut metadata.profiles {
        if profile.profile_id == prepared.profile_id {
            profile.activated_at_epoch_seconds = activated_at;
        }
    }
    metadata.active_mode = AuthMode::Baijimu;
    metadata.active_profile_id = Some(prepared.profile_id.clone());
    metadata.active_workspace_id = Some(prepared.workspace_id);
    save_metadata(&metadata)?;
    metadata
        .profiles
        .into_iter()
        .find(|item| item.profile_id == prepared.profile_id)
        .context("激活后未找到工作区凭证档案")
}

/// Applies Baijimu's initial desktop preference to one managed workspace profile.
///
/// The migration only exposes the Full access choice. It deliberately does not touch
/// `permission-selection-by-host-id:*`, so Ask for approval remains the selected mode.
/// Profiles that have not completed onboarding remain pending because Codex writes its
/// work-mode defaults at the end of onboarding; the next managed launch reapplies this
/// initial preference once and then records completion so later user changes are kept.
pub fn apply_workspace_desktop_defaults(codex_home: &Path) -> Result<()> {
    let mut metadata = load_metadata()?;
    let profile = metadata
        .profiles
        .iter_mut()
        .find(|profile| Path::new(&profile.codex_home) == codex_home)
        .context("当前 CODEX_HOME 不属于百积木工作区档案")?;
    if profile.desktop_defaults_version >= DESKTOP_DEFAULTS_VERSION {
        return Ok(());
    }

    let onboarding_completed = ensure_full_access_choice_visible(codex_home)?;
    if onboarding_completed {
        profile.desktop_defaults_version = DESKTOP_DEFAULTS_VERSION;
        save_metadata(&metadata)?;
    }
    Ok(())
}

fn ensure_full_access_choice_visible(codex_home: &Path) -> Result<bool> {
    let path = codex_home.join(CODEX_GLOBAL_STATE_FILE);
    let mut state = if path.exists() {
        let content = fs::read(&path)
            .with_context(|| format!("读取 Codex 桌面状态失败: {}", path.display()))?;
        crate::json_compat::from_slice::<Value>(&content)
            .with_context(|| format!("解析 Codex 桌面状态失败: {}", path.display()))?
    } else {
        json!({})
    };
    let root = state
        .as_object_mut()
        .context("Codex 桌面状态根节点必须是 JSON 对象")?;
    let persisted = root
        .entry(PERSISTED_ATOM_STATE_KEY)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Codex 桌面持久状态节点必须是 JSON 对象")?;
    let onboarding_completed = persisted
        .get(ONBOARDING_COMPLETED_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || persisted
            .get(LAST_COMPLETED_ONBOARDING_KEY)
            .and_then(Value::as_u64)
            .is_some_and(|value| value > 0);

    let changed = match persisted.get_mut(PERMISSION_MODE_VISIBILITY_KEY) {
        Some(Value::Bool(visible)) => {
            let changed = !*visible;
            *visible = true;
            changed
        }
        Some(Value::Object(visibility)) => {
            let changed = visibility.get("full-access").and_then(Value::as_bool) != Some(true);
            visibility.insert("full-access".to_string(), Value::Bool(true));
            changed
        }
        Some(_) => anyhow::bail!(
            "Codex 桌面权限菜单可见性状态格式不受支持: {}",
            path.display()
        ),
        None => {
            persisted.insert(
                PERMISSION_MODE_VISIBILITY_KEY.to_string(),
                json!({"guardian-approvals": true, "full-access": true}),
            );
            true
        }
    };
    if changed || !path.exists() {
        atomic_write_private(&path, &serde_json::to_vec_pretty(&state)?)?;
        verify_private_file(&path)?;
    }
    Ok(onboarding_completed)
}

pub fn activate_chatgpt_profile() -> Result<PathBuf> {
    let previous = load_metadata()?;
    let home = original_home_from_metadata(&previous);
    if previous
        .profiles
        .iter()
        .any(|profile| Path::new(&profile.codex_home) == home)
    {
        anyhow::bail!("默认 .codex 已绑定百积木工作区，不能同时作为个人 Codex 环境");
    }
    let mut metadata = previous.clone();
    metadata.active_mode = AuthMode::Chatgpt;
    metadata.active_profile_id = None;
    metadata.active_workspace_id = None;
    save_metadata(&metadata)?;
    Ok(home)
}

pub fn active_codex_home() -> PathBuf {
    load_metadata()
        .ok()
        .map(|metadata| active_home_from_metadata(&metadata))
        .unwrap_or_else(default_original_codex_home)
}

pub fn original_codex_home() -> PathBuf {
    load_metadata()
        .map(|metadata| original_home_from_metadata(&metadata))
        .unwrap_or_else(|_| default_original_codex_home())
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
    let chatgpt_configured = read_chatgpt_state(&original_codex_home())?.configured;
    Ok(should_auto_activate_workspace(
        &metadata,
        chatgpt_configured,
    ))
}

fn should_auto_activate_workspace(metadata: &CredentialMetadata, chatgpt_configured: bool) -> bool {
    let has_active_workspace = metadata.active_mode == AuthMode::Baijimu
        && metadata
            .active_profile_id
            .as_deref()
            .is_some_and(|profile_id| {
                metadata
                    .profiles
                    .iter()
                    .any(|profile| profile.profile_id == profile_id)
            });
    if has_active_workspace {
        return false;
    }
    !chatgpt_configured
}

pub fn finalize_workspace_setup(profile: &CredentialProfile, auto_activate: bool) -> Result<()> {
    if auto_activate {
        activate_prepared_workspace_profile(profile)?;
    }
    if !codex_ready_for_workspace(profile.workspace_id) {
        anyhow::bail!("独立工作区凭证档案未完成配置");
    }
    commit_default_home_ownership(profile)?;
    Ok(())
}

pub fn codex_ready_for_workspace(workspace_id: u64) -> bool {
    let metadata = load_metadata().ok();
    metadata.is_some_and(|metadata| {
        metadata.profiles.iter().any(|profile| {
            profile.workspace_id == workspace_id
                && read_codex_api_key(&Path::new(&profile.codex_home).join("auth.json"))
                    .ok()
                    .flatten()
                    .is_some()
                && managed_config_ready(&Path::new(&profile.codex_home).join("config.toml"))
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
    read_codex_api_key(&Path::new(&profile.codex_home).join("auth.json"))?
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

fn write_workspace_config(path: &Path) -> Result<()> {
    let product = crate::product_config::get();
    let original_path = original_codex_home().join("config.toml");
    let mut document = if original_path.exists() {
        let content = fs::read_to_string(&original_path)
            .with_context(|| format!("读取原有 Codex 配置失败: {}", original_path.display()))?;
        crate::json_compat::strip_utf8_bom_str(&content)
            .parse::<DocumentMut>()
            .context("解析原有 Codex config.toml 失败")?
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
    atomic_write_private(path, document.to_string().as_bytes())?;
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

fn read_chatgpt_state(home: &Path) -> Result<ChatGptProfileState> {
    let path = home.join("auth.json");
    if !path.exists() {
        return Ok(ChatGptProfileState {
            available: true,
            configured: false,
            auth_mode: None,
            account_id: None,
            codex_home: home.display().to_string(),
        });
    }
    let content = fs::read(&path)
        .with_context(|| format!("读取 ChatGPT 登录状态失败: {}", path.display()))?;
    let value: Value = crate::json_compat::from_slice(&content)
        .with_context(|| format!("解析 ChatGPT 登录状态失败: {}", path.display()))?;
    let auth_mode = value
        .get("auth_mode")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let account_id = value
        .get("tokens")
        .and_then(|v| v.get("account_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let configured = auth_mode.as_deref() == Some("chatgpt")
        && value
            .get("tokens")
            .and_then(|v| v.get("access_token"))
            .and_then(Value::as_str)
            .is_some_and(|v| !v.is_empty());
    Ok(ChatGptProfileState {
        available: true,
        configured,
        auth_mode,
        account_id,
        codex_home: home.display().to_string(),
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
                configured: metadata
                    .profiles
                    .iter()
                    .any(|item| item.workspace_id == workspace_id),
                user_ids,
            }
        })
        .collect()
}

pub fn pending_profile_home_migration() -> Result<Option<PendingProfileHomeMigration>> {
    let path = metadata_path();
    let source = if path.exists() {
        Some(path)
    } else if legacy_metadata_path().exists() {
        Some(legacy_metadata_path())
    } else {
        None
    };
    let Some(source) = source else {
        return Ok(None);
    };
    let content = fs::read(&source)
        .with_context(|| format!("读取 Codex 凭证元数据失败: {}", source.display()))?;
    let mut metadata = crate::json_compat::from_slice::<CredentialMetadata>(&content)
        .with_context(|| format!("解析 Codex 凭证元数据失败: {}", source.display()))?;
    for profile in &mut metadata.profiles {
        normalize_profile(profile);
    }
    let has_legacy_migration = metadata
        .profiles
        .iter()
        .any(|profile| Path::new(&profile.codex_home).starts_with(legacy_managed_profile_root()));

    let legacy_workspace_selection = metadata.version < 2
        && metadata.active_profile_id.is_none()
        && metadata.active_workspace_id.is_some();
    let active_profile = if metadata.active_mode == AuthMode::Baijimu || legacy_workspace_selection
    {
        metadata
            .active_profile_id
            .as_deref()
            .and_then(|profile_id| {
                metadata
                    .profiles
                    .iter()
                    .find(|profile| profile.profile_id == profile_id)
            })
            .or_else(|| {
                metadata.active_workspace_id.and_then(|workspace_id| {
                    metadata
                        .profiles
                        .iter()
                        .find(|profile| profile.workspace_id == workspace_id)
                })
            })
    } else {
        None
    };
    if has_legacy_migration {
        let active_profile = active_profile.filter(|profile| {
            Path::new(&profile.codex_home).starts_with(legacy_managed_profile_root())
        });
        let active_home_after = active_profile.map(|profile| {
            if default_home_can_bind_profile(&metadata, &profile.profile_id).unwrap_or(false) {
                default_original_codex_home()
            } else {
                profile_home_for_id(&profile.profile_id)
            }
        });
        return Ok(Some(PendingProfileHomeMigration {
            active_home_before: active_profile.map(|profile| PathBuf::from(&profile.codex_home)),
            active_home_after,
        }));
    }

    let Some(profile) = active_profile
        .filter(|profile| Path::new(&profile.codex_home).starts_with(managed_profile_root()))
    else {
        return Ok(None);
    };
    if !default_home_can_bind_profile(&metadata, &profile.profile_id)? {
        return Ok(None);
    }
    Ok(Some(PendingProfileHomeMigration {
        active_home_before: Some(PathBuf::from(&profile.codex_home)),
        active_home_after: Some(default_original_codex_home()),
    }))
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

fn original_home_from_metadata(metadata: &CredentialMetadata) -> PathBuf {
    metadata
        .original_codex_home_state
        .value
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(default_original_codex_home)
}

fn active_home_from_metadata(metadata: &CredentialMetadata) -> PathBuf {
    if metadata.active_mode == AuthMode::Chatgpt {
        return original_home_from_metadata(metadata);
    }
    metadata
        .active_profile_id
        .as_deref()
        .and_then(|id| {
            metadata
                .profiles
                .iter()
                .find(|profile| profile.profile_id == id)
                .map(|profile| PathBuf::from(&profile.codex_home))
        })
        .unwrap_or_else(|| original_home_from_metadata(metadata))
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
        profile.codex_home = workspace_profile_home(
            &profile.environment,
            profile.user_id,
            profile.client_id.as_deref(),
            profile.workspace_id,
        )
        .display()
        .to_string();
    }
}

fn migrate_legacy_profile_homes(metadata: &mut CredentialMetadata) -> Result<bool> {
    let legacy_root = legacy_managed_profile_root();
    let mut changed = false;
    for profile in &mut metadata.profiles {
        let source = PathBuf::from(&profile.codex_home);
        if !source.starts_with(&legacy_root) {
            continue;
        }
        let target = profile_home_for_id(&profile.profile_id);
        if source == target {
            continue;
        }

        match (source.exists(), target.exists()) {
            (true, true) => anyhow::bail!(
                "Codex 档案迁移发现源目录和目标目录同时存在；为避免覆盖状态，已保留两者，请先确认数据归属：source={}, target={}",
                source.display(),
                target.display()
            ),
            (true, false) => {
                if !source.is_dir() {
                    anyhow::bail!("旧版 Codex 档案路径不是目录: {}", source.display());
                }
                let parent = target.parent().context("新的 Codex 档案路径没有父目录")?;
                fs::create_dir_all(parent)
                    .with_context(|| format!("创建新的 Codex 档案根目录失败: {}", parent.display()))?;
                set_private_directory(parent)?;
                fs::rename(&source, &target).with_context(|| {
                    format!(
                        "迁移 Codex 档案目录失败；迁移要求旧目录和新目录位于同一文件系统，并且没有进程阻止目录重命名: source={}, target={}",
                        source.display(),
                        target.display()
                    )
                })?;
                set_private_directory(&target)?;
            }
            // Recovery after the directory rename succeeded but metadata persistence was interrupted.
            (false, true) => {}
            // The profile has not created any state yet; future writes should use the short path.
            (false, false) => {}
        }
        profile.codex_home = target.display().to_string();
        changed = true;
    }
    Ok(changed)
}

fn migrate_active_profile_to_default_home(
    metadata: &mut CredentialMetadata,
) -> Result<Option<CredentialProfile>> {
    let Some(profile_index) = active_managed_profile_index(metadata) else {
        return Ok(None);
    };
    let profile_id = metadata.profiles[profile_index].profile_id.clone();
    if !default_home_can_bind_profile(metadata, &profile_id)? {
        return Ok(None);
    }

    let source = PathBuf::from(&metadata.profiles[profile_index].codex_home);
    let target = default_original_codex_home();
    if source == target {
        return Ok(None);
    }
    let target_has_matching_reservation = read_default_home_reservation(&target)?
        .is_some_and(|reservation| reservation.profile_key == profile_short_key(&profile_id));
    let target_has_matching_ownership = read_valid_ownership(&target)?
        .and_then(|ownership| ownership.profile_key)
        .is_some_and(|key| key == profile_short_key(&profile_id));

    match (source.exists(), target.exists()) {
        (true, false) => {
            if !source.is_dir() {
                anyhow::bail!("百积木 Codex 档案路径不是目录: {}", source.display());
            }
            write_default_home_reservation(&source, &profile_id)?;
            fs::rename(&source, &target).with_context(|| {
                format!(
                    "将活动百积木 Codex 档案绑定到默认目录失败；迁移要求两个目录位于同一文件系统，并且没有进程占用源目录: source={}, target={}",
                    source.display(),
                    target.display()
                )
            })?;
            set_private_directory(&target)?;
        }
        // Recovery after the directory rename succeeded but metadata persistence was interrupted.
        (false, true) if target_has_matching_reservation || target_has_matching_ownership => {}
        // A user-owned or differently bound default directory must never be overwritten.
        (_, true) => return Ok(None),
        // The profile has no state yet. Reserve the absent default directory for this profile.
        (false, false) => reserve_default_home(&target, &profile_id)?,
    }

    metadata.profiles[profile_index].codex_home = target.display().to_string();
    Ok(Some(metadata.profiles[profile_index].clone()))
}

fn active_managed_profile_index(metadata: &CredentialMetadata) -> Option<usize> {
    if metadata.active_mode != AuthMode::Baijimu {
        return None;
    }
    let active_profile_id = metadata.active_profile_id.as_deref().or_else(|| {
        metadata.active_workspace_id.and_then(|workspace_id| {
            metadata
                .profiles
                .iter()
                .find(|profile| profile.workspace_id == workspace_id)
                .map(|profile| profile.profile_id.as_str())
        })
    })?;
    metadata.profiles.iter().position(|profile| {
        profile.profile_id == active_profile_id
            && (Path::new(&profile.codex_home).starts_with(managed_profile_root())
                || Path::new(&profile.codex_home).starts_with(legacy_managed_profile_root()))
    })
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

fn workspace_profile_home(
    environment: &str,
    user_id: Option<u64>,
    client_id: Option<&str>,
    workspace_id: u64,
) -> PathBuf {
    profile_home_for_id(&profile_id(environment, user_id, client_id, workspace_id))
}

fn profile_home_for_id(profile_id: &str) -> PathBuf {
    managed_profile_root().join(profile_short_key(profile_id))
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

fn select_new_profile_home(
    metadata: &CredentialMetadata,
    environment: &str,
    user_id: Option<u64>,
    client_id: Option<&str>,
    workspace_id: u64,
) -> Result<PathBuf> {
    let profile_id = profile_id(environment, user_id, client_id, workspace_id);
    if default_home_can_bind_profile(metadata, &profile_id)? {
        return Ok(default_original_codex_home());
    }
    Ok(workspace_profile_home(
        environment,
        user_id,
        client_id,
        workspace_id,
    ))
}

fn default_home_can_bind_profile(metadata: &CredentialMetadata, profile_id: &str) -> Result<bool> {
    let default_home = default_original_codex_home();
    if metadata.profiles.iter().any(|profile| {
        Path::new(&profile.codex_home) == default_home && profile.profile_id != profile_id
    }) {
        return Ok(false);
    }

    if let Some(configured_home) = user_environment::read_codex_home()? {
        let is_legacy_managed_pointer = configured_home.starts_with(managed_profile_root())
            || configured_home.starts_with(legacy_managed_profile_root());
        if !is_legacy_managed_pointer {
            return Ok(false);
        }
    }
    if metadata.original_codex_home_state.captured
        && metadata.original_codex_home_state.value.is_some()
    {
        return Ok(false);
    }

    let profile_key = profile_short_key(profile_id);
    match read_valid_ownership(&default_home) {
        Ok(Some(ownership)) => {
            return Ok(ownership.profile_key.as_deref() == Some(profile_key.as_str()))
        }
        Err(_) => return Ok(false),
        Ok(None) => {}
    }
    match read_default_home_reservation(&default_home) {
        Ok(Some(reservation)) => return Ok(reservation.profile_key == profile_key),
        Err(_) => return Ok(false),
        Ok(None) => {}
    }
    Ok(!default_home.exists())
}

fn reserve_default_home(home: &Path, profile_id: &str) -> Result<()> {
    if let Some(ownership) = read_valid_ownership(home)? {
        if ownership.profile_key.as_deref() == Some(profile_short_key(profile_id).as_str()) {
            return Ok(());
        }
        anyhow::bail!("默认 Codex 状态目录已经绑定其他档案: {}", home.display());
    }
    if let Some(reservation) = read_default_home_reservation(home)? {
        if reservation.profile_key == profile_short_key(profile_id) {
            return Ok(());
        }
        anyhow::bail!("默认 Codex 状态目录已经被其他档案预留: {}", home.display());
    }
    if home.exists() {
        anyhow::bail!(
            "默认 Codex 状态目录已存在且不受百积木控制: {}",
            home.display()
        );
    }
    fs::create_dir_all(home)
        .with_context(|| format!("创建默认 Codex 状态目录失败: {}", home.display()))?;
    set_private_directory(home)?;
    write_default_home_reservation(home, profile_id)
}

fn write_default_home_reservation(home: &Path, profile_id: &str) -> Result<()> {
    let reservation = CodexHomeReservation {
        schema_version: OWNERSHIP_SCHEMA_VERSION,
        owner: OWNERSHIP_OWNER.to_string(),
        reserved_at_epoch_seconds: now_epoch_seconds(),
        profile_key: profile_short_key(profile_id),
    };
    let path = home.join(OWNERSHIP_RESERVATION_FILE);
    atomic_write_private(&path, &serde_json::to_vec_pretty(&reservation)?)?;
    let verified = read_default_home_reservation(home)?
        .context("百积木 Codex 默认目录预留标记写入后无法回读")?;
    if verified != reservation {
        anyhow::bail!("百积木 Codex 默认目录预留标记回读不一致");
    }
    Ok(())
}

fn read_default_home_reservation(home: &Path) -> Result<Option<CodexHomeReservation>> {
    let path = home.join(OWNERSHIP_RESERVATION_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read(&path)
        .with_context(|| format!("读取百积木 Codex 默认目录预留标记失败: {}", path.display()))?;
    let reservation: CodexHomeReservation = crate::json_compat::from_slice(&content)
        .with_context(|| format!("解析百积木 Codex 默认目录预留标记失败: {}", path.display()))?;
    if reservation.schema_version != OWNERSHIP_SCHEMA_VERSION
        || !matches!(
            reservation.owner.as_str(),
            OWNERSHIP_OWNER | LEGACY_OWNERSHIP_OWNER
        )
        || reservation.profile_key.len() != 24
        || !reservation
            .profile_key
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        anyhow::bail!(
            "百积木 Codex 默认目录预留标记不受当前版本支持: {}",
            path.display()
        );
    }
    Ok(Some(reservation))
}

fn commit_default_home_ownership(profile: &CredentialProfile) -> Result<()> {
    let profile_home = Path::new(&profile.codex_home);
    if profile_home != default_original_codex_home() {
        return Ok(());
    }
    let profile_key = profile_short_key(&profile.profile_id);
    if let Some(ownership) = read_valid_ownership(profile_home)? {
        if ownership.profile_key.as_deref() == Some(profile_key.as_str()) {
            if ownership.owner == OWNERSHIP_OWNER {
                let _ = fs::remove_file(profile_home.join(OWNERSHIP_RESERVATION_FILE));
                return Ok(());
            }
        } else if ownership.schema_version != LEGACY_OWNERSHIP_SCHEMA_VERSION {
            anyhow::bail!("默认 Codex 状态目录已经绑定其他百积木档案");
        }
    }
    if read_codex_api_key(&profile_home.join(OWNED_AUTH_FILE))?.is_none()
        || !managed_config_ready(&profile_home.join(OWNED_CONFIG_FILE))
    {
        anyhow::bail!("默认 Codex 状态目录尚未完成百积木初始化");
    }
    let ownership = CodexHomeOwnership {
        schema_version: OWNERSHIP_SCHEMA_VERSION,
        owner: OWNERSHIP_OWNER.to_string(),
        initialized_at_epoch_seconds: now_epoch_seconds(),
        managed_files: vec![OWNED_AUTH_FILE.to_string(), OWNED_CONFIG_FILE.to_string()],
        profile_key: Some(profile_key),
    };
    let marker = profile_home.join(OWNERSHIP_MARKER_FILE);
    atomic_write_private(&marker, &serde_json::to_vec_pretty(&ownership)?)?;
    let verified =
        read_valid_ownership(profile_home)?.context("百积木 Codex 所有权标记写入后无法回读")?;
    if verified != ownership {
        anyhow::bail!("百积木 Codex 所有权标记回读不一致");
    }
    let reservation = profile_home.join(OWNERSHIP_RESERVATION_FILE);
    if reservation.exists() {
        fs::remove_file(&reservation).with_context(|| {
            format!("清理默认 Codex 目录预留标记失败: {}", reservation.display())
        })?;
    }
    Ok(())
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
    let supported_schema = marker.schema_version == OWNERSHIP_SCHEMA_VERSION
        || marker.schema_version == LEGACY_OWNERSHIP_SCHEMA_VERSION;
    let valid_profile_key = match marker.schema_version {
        OWNERSHIP_SCHEMA_VERSION => marker
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

#[cfg(test)]
mod tests {
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
            desktop_defaults_version: 0,
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
        let pending = pending_profile_home_migration().unwrap().unwrap();
        assert_eq!(
            pending.active_home_before.as_deref(),
            Some(legacy_home.as_path())
        );
        assert_eq!(
            pending.active_home_after.as_deref(),
            Some(migrated_home.as_path())
        );
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
        assert!(pending_profile_home_migration().unwrap().is_none());
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

        let pending = pending_profile_home_migration().unwrap().unwrap();
        assert_eq!(
            pending.active_home_before.as_deref(),
            Some(legacy_home.as_path())
        );
        assert_eq!(
            pending.active_home_after.as_deref(),
            Some(user_home.join(".codex").as_path())
        );
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
        assert!(pending_profile_home_migration().unwrap().is_none());
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
        let pending = pending_profile_home_migration().unwrap().unwrap();
        assert_eq!(
            pending.active_home_before.as_deref(),
            Some(private_home.as_path())
        );
        assert_eq!(
            pending.active_home_after.as_deref(),
            Some(default_home.as_path())
        );

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
            desktop_defaults_version: 0,
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
            desktop_defaults_version: 0,
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
    fn legacy_boolean_visibility_is_enabled_without_changing_the_selected_permission() {
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-legacy-permission-visibility-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join(CODEX_GLOBAL_STATE_FILE);
        let selected = json!({"kind": "agent-mode", "agentMode": "auto"});
        fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "unrelated-root-state": {"keep": true},
                (PERSISTED_ATOM_STATE_KEY): {
                    (ONBOARDING_COMPLETED_KEY): true,
                    (PERMISSION_MODE_VISIBILITY_KEY): false,
                    "permission-selection-by-host-id:local": selected,
                    "unrelated-persisted-state": [1, 2, 3]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(ensure_full_access_choice_visible(&root).unwrap());

        let migrated: Value = crate::json_compat::from_slice(&fs::read(&path).unwrap()).unwrap();
        let persisted = migrated[PERSISTED_ATOM_STATE_KEY].as_object().unwrap();
        assert_eq!(persisted[PERMISSION_MODE_VISIBILITY_KEY], Value::Bool(true));
        assert_eq!(persisted["permission-selection-by-host-id:local"], selected);
        assert_eq!(persisted["unrelated-persisted-state"], json!([1, 2, 3]));
        assert_eq!(migrated["unrelated-root-state"], json!({"keep": true}));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn object_visibility_preserves_guardian_and_future_fields() {
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-object-permission-visibility-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join(CODEX_GLOBAL_STATE_FILE);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                (PERSISTED_ATOM_STATE_KEY): {
                    (LAST_COMPLETED_ONBOARDING_KEY): 42,
                    (PERMISSION_MODE_VISIBILITY_KEY): {
                        "guardian-approvals": false,
                        "full-access": false,
                        "future-mode": "keep"
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(ensure_full_access_choice_visible(&root).unwrap());

        let migrated: Value = crate::json_compat::from_slice(&fs::read(&path).unwrap()).unwrap();
        let visibility = migrated[PERSISTED_ATOM_STATE_KEY][PERMISSION_MODE_VISIBILITY_KEY]
            .as_object()
            .unwrap();
        assert_eq!(visibility["guardian-approvals"], Value::Bool(false));
        assert_eq!(visibility["full-access"], Value::Bool(true));
        assert_eq!(visibility["future-mode"], Value::String("keep".to_string()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_default_is_applied_once_and_later_user_changes_are_kept() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-once-permission-visibility-{}-{}",
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
        let profile = test_workspace_profile(&data_dir, 642);
        let profile_home = PathBuf::from(&profile.codex_home);
        fs::create_dir_all(&profile_home).unwrap();
        let state_path = profile_home.join(CODEX_GLOBAL_STATE_FILE);
        fs::write(
            &state_path,
            serde_json::to_vec_pretty(&json!({
                (PERSISTED_ATOM_STATE_KEY): {
                    (ONBOARDING_COMPLETED_KEY): true,
                    (PERMISSION_MODE_VISIBILITY_KEY): false,
                    "permission-selection-by-host-id:local": {
                        "kind": "agent-mode",
                        "agentMode": "auto"
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        save_metadata(&CredentialMetadata {
            profiles: vec![profile],
            ..CredentialMetadata::default()
        })
        .unwrap();

        apply_workspace_desktop_defaults(&profile_home).unwrap();
        assert_eq!(
            load_metadata().unwrap().profiles[0].desktop_defaults_version,
            DESKTOP_DEFAULTS_VERSION
        );

        let mut state: Value =
            crate::json_compat::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        state[PERSISTED_ATOM_STATE_KEY][PERMISSION_MODE_VISIBILITY_KEY] = Value::Bool(false);
        fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
        apply_workspace_desktop_defaults(&profile_home).unwrap();
        let unchanged: Value =
            crate::json_compat::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(
            unchanged[PERSISTED_ATOM_STATE_KEY][PERMISSION_MODE_VISIBILITY_KEY],
            Value::Bool(false)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn onboarding_override_is_reapplied_before_the_migration_is_completed() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-pending-permission-visibility-{}-{}",
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
        let profile = test_workspace_profile(&data_dir, 1390);
        let profile_home = PathBuf::from(&profile.codex_home);
        fs::create_dir_all(&profile_home).unwrap();
        save_metadata(&CredentialMetadata {
            profiles: vec![profile],
            ..CredentialMetadata::default()
        })
        .unwrap();

        apply_workspace_desktop_defaults(&profile_home).unwrap();
        assert_eq!(
            load_metadata().unwrap().profiles[0].desktop_defaults_version,
            0
        );

        let state_path = profile_home.join(CODEX_GLOBAL_STATE_FILE);
        let mut state: Value =
            crate::json_compat::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        state[PERSISTED_ATOM_STATE_KEY][ONBOARDING_COMPLETED_KEY] = Value::Bool(true);
        state[PERSISTED_ATOM_STATE_KEY][PERMISSION_MODE_VISIBILITY_KEY] = json!({
            "guardian-approvals": true,
            "full-access": false
        });
        fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();

        apply_workspace_desktop_defaults(&profile_home).unwrap();
        let migrated: Value =
            crate::json_compat::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(
            migrated[PERSISTED_ATOM_STATE_KEY][PERMISSION_MODE_VISIBILITY_KEY]["full-access"],
            Value::Bool(true)
        );
        assert_eq!(
            load_metadata().unwrap().profiles[0].desktop_defaults_version,
            DESKTOP_DEFAULTS_VERSION
        );
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
            desktop_defaults_version: 0,
        }
    }
}
