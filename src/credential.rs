use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use toml_edit::{value, DocumentMut, Item, Table};

use crate::user_environment;

const METADATA_VERSION: u32 = 3;
const METADATA_FILE: &str = "codex-credentials.json";
const DEFAULT_MODEL: &str = "gpt-5.6-sol";
const ROUTER_PROVIDER: &str = "baijimu-router";
const ROUTER_BASE_URL: &str = "https://router.baijimu.com/api/claudecode/v1";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    #[default]
    Chatgpt,
    Baijimu,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CredentialProfile {
    #[serde(default)]
    pub profile_id: String,
    #[serde(default = "default_environment")]
    pub environment: String,
    #[serde(default)]
    pub user_id: Option<u64>,
    #[serde(default)]
    pub client_id: Option<String>,
    pub workspace_id: u64,
    pub workspace_name: String,
    pub model: String,
    pub activated_at_epoch_seconds: u64,
    #[serde(default)]
    pub codex_home: String,
    #[serde(default)]
    pub credential_status: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceOption {
    pub workspace_id: u64,
    pub name: String,
    pub authorized: bool,
    pub configured: bool,
    pub user_ids: Vec<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatGptProfileState {
    pub configured: bool,
    pub auth_mode: Option<String>,
    pub account_id: Option<String>,
    pub codex_home: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialManagerState {
    pub active_mode: AuthMode,
    pub current_workspace_id: Option<u64>,
    pub active_workspace_id: Option<u64>,
    pub codex_configured: bool,
    pub credential_status: String,
    pub active_profile: Option<CredentialProfile>,
    pub profiles: Vec<CredentialProfile>,
    pub workspaces: Vec<WorkspaceOption>,
    pub chatgpt: ChatGptProfileState,
    pub discovery_warning: Option<String>,
    pub shared_auth_path: String,
    pub original_codex_home_state: OriginalCodexHomeState,
    pub original_codex_home: String,
    pub active_codex_home: String,
    pub user_codex_home: Option<String>,
    pub user_codex_home_synchronized: bool,
    pub desktop_environment_managed: bool,
    pub codex_auth_path: String,
    pub codex_config_path: String,
}

#[derive(Clone, Debug)]
pub struct PreparedWorkspaceProfile {
    pub profile: CredentialProfile,
    pub credential: String,
}

#[derive(Clone, Debug)]
pub struct ActiveHomeSnapshot {
    metadata: CredentialMetadata,
    user_codex_home: Option<PathBuf>,
}

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
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OriginalCodexHomeState {
    #[serde(default)]
    pub captured: bool,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub capture_source: String,
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
        }
    }
}

#[derive(Clone, Debug)]
struct LocalMachineCredential {
    workspace_ids: Vec<u64>,
    token: String,
    user_id: Option<u64>,
    client_id: Option<String>,
    issued_at: String,
    issued_at_epoch_seconds: u64,
}

#[derive(Clone, Debug)]
struct SharedCredentialStore {
    environment: String,
    base_url: String,
    current_workspace_id: Option<u64>,
    credentials: Vec<LocalMachineCredential>,
}

pub fn state() -> Result<CredentialManagerState> {
    let mut metadata = load_metadata()?;
    let original_home = original_home_from_metadata(&metadata);
    let chatgpt = read_chatgpt_state(&original_home)?;
    let shared_store = load_shared_credential_store();
    let mut warning = shared_store.as_ref().err().map(ToString::to_string);
    let (current_workspace_id, base_url, mut workspaces) = match shared_store.as_ref() {
        Ok(store) => {
            let (discovered, discovery_warning) = discover_workspaces(store);
            if discovery_warning.is_some() {
                warning = discovery_warning;
            }
            (
                store.current_workspace_id,
                Some(store.base_url.as_str()),
                merge_workspace_options(store, discovered, &metadata),
            )
        }
        Err(_) => (None, None, workspace_options_from_metadata(&metadata)),
    };

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
    let user_codex_home = user_environment::read_codex_home()?;
    let desired_user_codex_home = desired_user_codex_home(&metadata);
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
        if let (Some(profile), Some(base_url)) = (active_profile.as_mut(), base_url) {
            if let Some(key) = read_codex_api_key(&auth_path)? {
                match validate_workspace_credential(base_url, &key, profile.workspace_id) {
                    Ok(true) => {
                        credential_status = "verified".to_string();
                        codex_configured = managed_config_ready(&config_path);
                        profile.credential_status = "verified".to_string();
                    }
                    Ok(false) => {
                        credential_status = "invalid".to_string();
                        profile.credential_status = "invalid".to_string();
                    }
                    Err(error) => {
                        credential_status = "unverified".to_string();
                        profile.credential_status = "unverified".to_string();
                        warning.get_or_insert_with(|| format!("暂时无法校验当前凭证：{error}"));
                    }
                }
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
        shared_auth_path: shared_auth_path().display().to_string(),
        original_codex_home_state: metadata.original_codex_home_state.clone(),
        original_codex_home: original_home.display().to_string(),
        active_codex_home: active_home.display().to_string(),
        user_codex_home: user_codex_home
            .as_ref()
            .map(|path| path.display().to_string()),
        user_codex_home_synchronized: user_codex_home == desired_user_codex_home,
        desktop_environment_managed: user_environment::persisted_for_desktop(),
        codex_auth_path: auth_path.display().to_string(),
        codex_config_path: config_path.display().to_string(),
    })
}

pub fn prepare_workspace_profile(workspace_id: u64) -> Result<PreparedWorkspaceProfile> {
    if workspace_id == 0 {
        anyhow::bail!("工作区 ID 必须大于 0");
    }
    let store = load_shared_credential_store()?;
    let local = select_local_machine_credential(&store, workspace_id)
        .context("本机授权不包含该工作区，请先为当前百积木账号授权这个工作区")?;
    let user_id = local.user_id;
    let client_id = local.client_id.clone();
    let profile_id = profile_id(
        &store.environment,
        user_id,
        client_id.as_deref(),
        workspace_id,
    );
    let profile_home = workspace_profile_home(
        &store.environment,
        user_id,
        client_id.as_deref(),
        workspace_id,
    );
    let auth_path = profile_home.join("auth.json");
    let existing = read_codex_api_key(&auth_path)?;
    let credential = match existing {
        Some(key) => match validate_workspace_credential(&store.base_url, &key, workspace_id) {
            Ok(true) => key,
            Ok(false) => issue_workspace_credential_with_store(&store, local, workspace_id)?,
            Err(error) => {
                return Err(error)
                    .context("暂时无法校验已保存的工作区凭证；为避免重复签发，本次未进行切换")
            }
        },
        _ => issue_workspace_credential_with_store(&store, local, workspace_id)?,
    };
    write_workspace_auth(&auth_path, &credential)?;
    write_workspace_config(&profile_home.join("config.toml"))?;

    let (workspaces, _) = discover_workspaces(&store);
    let workspace_name = workspaces
        .into_iter()
        .find(|item| item.workspace_id == workspace_id)
        .map(|item| item.name)
        .unwrap_or_else(|| format!("工作区 {workspace_id}"));
    let mut metadata = load_metadata()?;
    let previous_activation = metadata
        .profiles
        .iter()
        .find(|item| item.profile_id == profile_id)
        .map(|item| item.activated_at_epoch_seconds)
        .unwrap_or_default();
    let profile = CredentialProfile {
        profile_id: profile_id.clone(),
        environment: store.environment.clone(),
        user_id,
        client_id,
        workspace_id,
        workspace_name,
        model: DEFAULT_MODEL.to_string(),
        activated_at_epoch_seconds: previous_activation,
        codex_home: profile_home.display().to_string(),
        credential_status: "verified".to_string(),
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
    commit_active_home_change(&previous, &metadata)?;
    metadata
        .profiles
        .into_iter()
        .find(|item| item.profile_id == prepared.profile_id)
        .context("激活后未找到工作区凭证档案")
}

pub fn activate_chatgpt_profile() -> Result<PathBuf> {
    let previous = load_metadata()?;
    let home = original_home_from_metadata(&previous);
    let mut metadata = previous.clone();
    metadata.active_mode = AuthMode::Chatgpt;
    metadata.active_profile_id = None;
    metadata.active_workspace_id = None;
    commit_active_home_change(&previous, &metadata)?;
    Ok(home)
}

pub fn active_codex_home() -> PathBuf {
    load_metadata()
        .ok()
        .and_then(|metadata| {
            if metadata.active_mode == AuthMode::Chatgpt {
                return Some(original_home_from_metadata(&metadata));
            }
            metadata.active_profile_id.as_deref().and_then(|id| {
                metadata
                    .profiles
                    .iter()
                    .find(|profile| profile.profile_id == id)
                    .map(|profile| PathBuf::from(&profile.codex_home))
            })
        })
        .unwrap_or_else(default_original_codex_home)
}

pub fn original_codex_home() -> PathBuf {
    load_metadata()
        .map(|metadata| original_home_from_metadata(&metadata))
        .unwrap_or_else(|_| default_original_codex_home())
}

pub fn reconcile_active_user_codex_home() -> Result<()> {
    let metadata = load_metadata()?;
    let desired = desired_user_codex_home(&metadata);
    user_environment::activate_codex_home(desired.as_deref())?;
    Ok(())
}

pub fn active_home_snapshot() -> Result<ActiveHomeSnapshot> {
    Ok(ActiveHomeSnapshot {
        metadata: load_metadata()?,
        user_codex_home: user_environment::read_codex_home()?,
    })
}

pub fn restore_active_home(snapshot: ActiveHomeSnapshot) -> Result<()> {
    save_metadata(&snapshot.metadata)?;
    user_environment::activate_codex_home(snapshot.user_codex_home.as_deref())?;
    Ok(())
}

#[cfg(test)]
fn current_workspace_id() -> Result<u64> {
    let store = load_shared_credential_store()?;
    if let Some(workspace_id) = store.current_workspace_id.filter(|value| *value > 0) {
        return Ok(workspace_id);
    }
    let workspace_ids = store
        .credentials
        .iter()
        .flat_map(|item| item.workspace_ids.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    match workspace_ids.as_slice() {
        [workspace_id] => Ok(*workspace_id),
        [] => anyhow::bail!("本机授权不包含工作区"),
        _ => anyhow::bail!("本机授权包含多个工作区，但没有设置当前工作区"),
    }
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

fn load_shared_credential_store() -> Result<SharedCredentialStore> {
    let path = shared_auth_path();
    let content = fs::read_to_string(&path)
        .with_context(|| format!("读取百积木本机授权失败: {}", path.display()))?;
    let document: Value = serde_json::from_str(&content)
        .with_context(|| format!("解析百积木本机授权失败: {}", path.display()))?;
    let environment = document
        .get("currentEnvironment")
        .and_then(Value::as_str)
        .unwrap_or("prod")
        .to_string();
    let configured_base_url = document
        .get("environments")
        .and_then(|v| v.get(&environment))
        .and_then(|v| v.get("baseUrl"))
        .and_then(Value::as_str)
        .unwrap_or("https://www.baijimu.com");
    let credentials = document
        .get("credentials")
        .or_else(|| document.get("machineCredentials"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let mut workspace_ids = value
                .get("workspaceIds")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
                .filter(|id| *id > 0)
                .collect::<Vec<_>>();
            if let Some(id) = value
                .get("workspaceId")
                .and_then(Value::as_u64)
                .filter(|id| *id > 0)
            {
                workspace_ids.push(id);
            }
            workspace_ids.sort_unstable();
            workspace_ids.dedup();
            let token = value.get("token").and_then(Value::as_str)?.trim();
            (!token.is_empty() && !workspace_ids.is_empty()).then(|| LocalMachineCredential {
                workspace_ids,
                token: token.to_string(),
                user_id: value.get("userId").and_then(Value::as_u64),
                client_id: value
                    .get("clientId")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                issued_at: value
                    .get("issuedAt")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                issued_at_epoch_seconds: value
                    .get("issuedAtEpochSeconds")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    if credentials.is_empty() {
        anyhow::bail!("本机还没有工作区授权，请先在百积木中完成设备授权");
    }
    Ok(SharedCredentialStore {
        environment,
        base_url: normalize_baijimu_root_url(configured_base_url),
        current_workspace_id: document.get("currentWorkspaceId").and_then(Value::as_u64),
        credentials,
    })
}

fn select_local_machine_credential(
    store: &SharedCredentialStore,
    workspace_id: u64,
) -> Option<&LocalMachineCredential> {
    store
        .credentials
        .iter()
        .filter(|item| item.workspace_ids.contains(&workspace_id))
        .max_by(|left, right| {
            (left.issued_at_epoch_seconds, left.issued_at.as_str())
                .cmp(&(right.issued_at_epoch_seconds, right.issued_at.as_str()))
        })
}

fn issue_workspace_credential_with_store(
    store: &SharedCredentialStore,
    local: &LocalMachineCredential,
    workspace_id: u64,
) -> Result<String> {
    let response = post_baijimu_json(
        &store.base_url,
        &format!("/llm-credential/partner/v1/workspaces/{workspace_id}/llm-credentials/create"),
        &local.token,
        json!({"workspaceId": workspace_id, "projectId": null}),
    )?;
    let data = unwrap_baijimu_data(&response)?;
    let credential = ["llmCredential", "credential", "apiKey"]
        .iter()
        .find_map(|field| data.get(*field).and_then(Value::as_str))
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .context("平台已响应，但没有返回 LLM credential")?
        .to_string();
    if !validate_workspace_credential(&store.base_url, &credential, workspace_id)? {
        anyhow::bail!("新签发的 LLM credential 归属或有效性校验失败");
    }
    Ok(credential)
}

fn validate_workspace_credential(
    base_url: &str,
    credential: &str,
    workspace_id: u64,
) -> Result<bool> {
    let Some(validated) = validate_credential(base_url, credential)? else {
        return Ok(false);
    };
    Ok(
        validated.get("workspaceId").and_then(Value::as_u64) == Some(workspace_id)
            && validated.get("projectId").is_none_or(Value::is_null),
    )
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
    let original_path = original_codex_home().join("config.toml");
    let mut document = if original_path.exists() {
        fs::read_to_string(&original_path)
            .with_context(|| format!("读取原有 Codex 配置失败: {}", original_path.display()))?
            .parse::<DocumentMut>()
            .context("解析原有 Codex config.toml 失败")?
    } else {
        DocumentMut::new()
    };
    document["model"] = value(DEFAULT_MODEL);
    document["model_provider"] = value(ROUTER_PROVIDER);
    document["sandbox_mode"] = value("danger-full-access");
    document["approval_policy"] = value("on-request");
    document["cli_auth_credentials_store"] = value("file");
    document["forced_login_method"] = value("api");
    if document
        .get("model_providers")
        .and_then(Item::as_table)
        .is_none()
    {
        document["model_providers"] = Item::Table(Table::new());
    }
    if document["model_providers"]
        .as_table()
        .and_then(|table| table.get(ROUTER_PROVIDER))
        .and_then(Item::as_table)
        .is_none()
    {
        document["model_providers"][ROUTER_PROVIDER] = Item::Table(Table::new());
    }
    let provider = &mut document["model_providers"][ROUTER_PROVIDER];
    provider["name"] = value(ROUTER_PROVIDER);
    provider["base_url"] = value(ROUTER_BASE_URL);
    provider["wire_api"] = value("responses");
    provider["requires_openai_auth"] = value(true);
    atomic_write_private(path, document.to_string().as_bytes())?;
    verify_private_file(path)
}

fn managed_config_ready(path: &Path) -> bool {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<DocumentMut>().ok())
        .is_some_and(|doc| {
            doc.get("model_provider").and_then(Item::as_str) == Some(ROUTER_PROVIDER)
                && doc
                    .get("model_providers")
                    .and_then(Item::as_table)
                    .and_then(|table| table.get(ROUTER_PROVIDER))
                    .and_then(Item::as_table)
                    .and_then(|table| table.get("base_url"))
                    .and_then(Item::as_str)
                    == Some(ROUTER_BASE_URL)
        })
}

fn read_chatgpt_state(home: &Path) -> Result<ChatGptProfileState> {
    let path = home.join("auth.json");
    if !path.exists() {
        return Ok(ChatGptProfileState {
            configured: false,
            auth_mode: None,
            account_id: None,
            codex_home: home.display().to_string(),
        });
    }
    let value: Value = serde_json::from_str(
        &fs::read_to_string(&path)
            .with_context(|| format!("读取 ChatGPT 登录状态失败: {}", path.display()))?,
    )
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
        configured,
        auth_mode,
        account_id,
        codex_home: home.display().to_string(),
    })
}

fn merge_workspace_options(
    store: &SharedCredentialStore,
    discovered: Vec<WorkspaceOption>,
    metadata: &CredentialMetadata,
) -> Vec<WorkspaceOption> {
    let mut ids = discovered
        .iter()
        .map(|item| item.workspace_id)
        .collect::<BTreeSet<_>>();
    ids.extend(
        store
            .credentials
            .iter()
            .flat_map(|item| item.workspace_ids.iter().copied()),
    );
    ids.extend(metadata.profiles.iter().map(|item| item.workspace_id));
    ids.into_iter()
        .map(|workspace_id| {
            let name = discovered
                .iter()
                .find(|item| item.workspace_id == workspace_id)
                .map(|item| item.name.clone())
                .or_else(|| {
                    metadata
                        .profiles
                        .iter()
                        .find(|item| item.workspace_id == workspace_id)
                        .map(|item| item.workspace_name.clone())
                })
                .unwrap_or_else(|| format!("工作区 {workspace_id}"));
            let user_ids = store
                .credentials
                .iter()
                .filter(|item| item.workspace_ids.contains(&workspace_id))
                .filter_map(|item| item.user_id)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            WorkspaceOption {
                workspace_id,
                name,
                authorized: store
                    .credentials
                    .iter()
                    .any(|item| item.workspace_ids.contains(&workspace_id)),
                configured: metadata
                    .profiles
                    .iter()
                    .any(|item| item.workspace_id == workspace_id),
                user_ids,
            }
        })
        .collect()
}

fn workspace_options_from_metadata(metadata: &CredentialMetadata) -> Vec<WorkspaceOption> {
    metadata
        .profiles
        .iter()
        .map(|profile| WorkspaceOption {
            workspace_id: profile.workspace_id,
            name: profile.workspace_name.clone(),
            authorized: false,
            configured: true,
            user_ids: profile.user_id.into_iter().collect(),
        })
        .collect()
}

fn discover_workspaces(store: &SharedCredentialStore) -> (Vec<WorkspaceOption>, Option<String>) {
    let token = store
        .current_workspace_id
        .and_then(|id| select_local_machine_credential(store, id))
        .or_else(|| {
            store
                .credentials
                .iter()
                .max_by_key(|item| (&item.issued_at, item.issued_at_epoch_seconds))
        });
    let Some(token) = token else {
        return (Vec::new(), Some("本机没有工作区授权".to_string()));
    };
    let result = post_baijimu_json(
        &store.base_url,
        "/lowcode3/partner/v1/workspaces/list",
        &token.token,
        json!({"pageNum":1,"pageSize":200}),
    )
    .and_then(|response| {
        let data = unwrap_baijimu_data(&response)?;
        Ok(data
            .get("list")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                Some(WorkspaceOption {
                    workspace_id: item.get("id").and_then(Value::as_u64)?,
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("未命名工作区")
                        .trim()
                        .to_string(),
                    authorized: false,
                    configured: false,
                    user_ids: Vec::new(),
                })
            })
            .collect::<Vec<_>>())
    });
    match result {
        Ok(items) => (items, None),
        Err(error) => (Vec::new(), Some(format!("暂时无法读取工作区名称：{error}"))),
    }
}

fn post_baijimu_json(base_url: &str, path: &str, token: &str, body: Value) -> Result<Value> {
    let response = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(45))
        .build()
        .context("创建平台请求失败")?
        .post(format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        ))
        .bearer_auth(token)
        .json(&body)
        .send()
        .context("请求百积木平台失败")?;
    let status = response.status();
    let payload = response.text().context("读取百积木平台响应失败")?;
    if !status.is_success() {
        anyhow::bail!("百积木平台返回 HTTP {status}: {}", compact_body(&payload));
    }
    serde_json::from_str(&payload).context("百积木平台返回了无效 JSON")
}

fn validate_credential(base_url: &str, credential: &str) -> Result<Option<Value>> {
    let response = Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(45))
        .build()
        .context("创建凭证校验请求失败")?
        .post(format!(
            "{}/llm-credential/validateCredential",
            base_url.trim_end_matches('/')
        ))
        .bearer_auth(credential)
        .json(&json!({"key": credential}))
        .send()
        .context("请求凭证校验服务失败")?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Ok(None);
    }
    let payload = response.text().context("读取凭证校验响应失败")?;
    if !status.is_success() {
        anyhow::bail!("凭证校验服务返回 HTTP {status}: {}", compact_body(&payload));
    }
    let response: Value = serde_json::from_str(&payload).context("凭证校验服务返回了无效 JSON")?;
    let data = unwrap_baijimu_data(&response)?;
    let valid = data.get("valid").and_then(Value::as_bool).unwrap_or(false);
    let allowed = data
        .get("allowed")
        .and_then(Value::as_bool)
        .unwrap_or(valid);
    Ok((valid && allowed).then(|| data.clone()))
}

fn unwrap_baijimu_data(response: &Value) -> Result<&Value> {
    if let Some(code) = response
        .get("errorCode")
        .or_else(|| response.get("error_code"))
        .and_then(Value::as_str)
    {
        if code != "0" {
            let message = response
                .get("value")
                .or_else(|| response.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("平台操作失败");
            anyhow::bail!("{message}（{code}）");
        }
        return Ok(response.get("data").unwrap_or(&Value::Null));
    }
    Ok(response.get("data").unwrap_or(response))
}

fn compact_body(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(400)
        .collect()
}

fn load_metadata() -> Result<CredentialMetadata> {
    let path = metadata_path();
    let source = if path.exists() {
        Some(path.clone())
    } else if legacy_metadata_path().exists() {
        Some(legacy_metadata_path())
    } else {
        None
    };
    let mut metadata = if let Some(source) = source.as_ref() {
        let content = fs::read_to_string(source)
            .with_context(|| format!("读取 Codex 凭证元数据失败: {}", source.display()))?;
        serde_json::from_str::<CredentialMetadata>(&content)
            .with_context(|| format!("解析 Codex 凭证元数据失败: {}", source.display()))?
    } else {
        CredentialMetadata::default()
    };
    let previous_version = metadata.version;
    let needs_version_migration = previous_version < METADATA_VERSION;
    for profile in &mut metadata.profiles {
        normalize_profile(profile);
    }
    if previous_version < 2 && metadata.active_profile_id.is_none() {
        metadata.active_profile_id = metadata.active_workspace_id.and_then(|id| {
            metadata
                .profiles
                .iter()
                .find(|p| p.workspace_id == id)
                .map(|p| p.profile_id.clone())
        });
        if metadata.active_profile_id.is_some() {
            metadata.active_mode = AuthMode::Baijimu;
        }
    }
    let baseline_captured = capture_original_codex_home(&mut metadata)?;
    metadata.version = METADATA_VERSION;
    if source.as_ref() != Some(&path) || needs_version_migration || baseline_captured {
        save_metadata(&metadata)?;
    }
    if let Some(source) = source.filter(|source| source != &path) {
        fs::remove_file(&source)
            .with_context(|| format!("清理旧版元数据失败: {}", source.display()))?;
    }
    Ok(metadata)
}

fn capture_original_codex_home(metadata: &mut CredentialMetadata) -> Result<bool> {
    if metadata.original_codex_home_state.captured {
        return Ok(false);
    }
    let current = user_environment::read_codex_home()?;
    let managed_root = connector_data_dir().join("codex-profiles");
    let managed_pointer = current.as_ref().is_some_and(|path| {
        path.starts_with(&managed_root)
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

fn default_original_codex_home() -> PathBuf {
    home_dir().join(".codex")
}

fn desired_user_codex_home(metadata: &CredentialMetadata) -> Option<PathBuf> {
    match metadata.active_mode {
        AuthMode::Chatgpt => metadata
            .original_codex_home_state
            .value
            .as_deref()
            .map(PathBuf::from),
        AuthMode::Baijimu => metadata.active_profile_id.as_deref().and_then(|id| {
            metadata
                .profiles
                .iter()
                .find(|profile| profile.profile_id == id)
                .map(|profile| PathBuf::from(&profile.codex_home))
        }),
    }
}

fn commit_active_home_change(
    previous: &CredentialMetadata,
    next: &CredentialMetadata,
) -> Result<()> {
    let previous_environment = user_environment::read_codex_home()?;
    save_metadata(next)?;
    let desired = desired_user_codex_home(next);
    if let Err(error) = user_environment::activate_codex_home(desired.as_deref()) {
        let metadata_rollback = save_metadata(previous).err();
        let environment_rollback =
            user_environment::activate_codex_home(previous_environment.as_deref()).err();
        let mut message = format!("切换用户级 CODEX_HOME 失败，已执行回滚：{error}");
        if let Some(rollback) = metadata_rollback {
            message.push_str(&format!("；元数据回滚失败：{rollback}"));
        }
        if let Some(rollback) = environment_rollback {
            message.push_str(&format!("；环境变量回滚失败：{rollback}"));
        }
        anyhow::bail!(message);
    }
    Ok(())
}

fn save_metadata(metadata: &CredentialMetadata) -> Result<()> {
    atomic_write_private(&metadata_path(), &serde_json::to_vec_pretty(metadata)?)?;
    verify_private_file(&metadata_path())
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

fn sort_profiles(profiles: &mut [CredentialProfile]) {
    profiles.sort_by(|left, right| {
        (&left.workspace_name, &left.profile_id).cmp(&(&right.workspace_name, &right.profile_id))
    });
}

fn read_codex_api_key(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(
        &fs::read_to_string(path)
            .with_context(|| format!("读取 Codex 认证文件失败: {}", path.display()))?,
    )
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
    connector_data_dir()
        .join("codex-profiles")
        .join("baijimu")
        .join(sanitize_path_segment(environment))
        .join(format!("user-{}", user_id.unwrap_or_default()))
        .join(format!(
            "client-{}",
            sanitize_path_segment(client_id.unwrap_or("local"))
        ))
        .join(format!("workspace-{workspace_id}"))
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

fn normalize_baijimu_root_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    let root = trimmed.strip_suffix("/lowcode3").unwrap_or(trimmed);
    match root {
        "https://www.baijimu.com" | "https://baijimu.com" => "https://api.baijimu.com".to_string(),
        _ => root.to_string(),
    }
}

fn shared_auth_path() -> PathBuf {
    if let Some(config_home) = std::env::var_os("BAIJIMU_CONFIG_HOME") {
        return PathBuf::from(config_home).join("baijimu").join("auth.json");
    }
    home_dir().join(".config").join("baijimu").join("auth.json")
}

fn metadata_path() -> PathBuf {
    connector_data_dir().join(METADATA_FILE)
}
fn legacy_metadata_path() -> PathBuf {
    shared_auth_path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(METADATA_FILE)
}
fn connector_data_dir() -> PathBuf {
    std::env::var_os("BAIJIMU_CONNECTOR_DATA_DIR")
        .or_else(|| std::env::var_os("CODEX_CONNECTOR_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".baijimu-connector-codex"))
}
fn home_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}
fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn atomic_write_private(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("创建目录失败: {}", parent.display()))?;
        set_private_directory(parent)?;
    }
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let temp = path.with_extension(format!("tmp-{}-{unique}", std::process::id()));
    fs::write(&temp, content).with_context(|| format!("写入临时文件失败: {}", temp.display()))?;
    set_private_file(&temp)?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&temp, path).with_context(|| format!("替换文件失败: {}", path.display()))?;
    set_private_file(path)?;
    Ok(())
}
fn verify_private_file(path: &Path) -> Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("回读文件失败: {}", path.display()))?;
    if !metadata.is_file() || metadata.len() == 0 {
        anyhow::bail!("文件为空或不是普通文件: {}", path.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            anyhow::bail!("文件权限不是 600: {}", path.display());
        }
    }
    Ok(())
}
fn set_private_directory(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn set_private_file(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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
        fs::write(legacy_metadata_path(), serde_json::to_vec_pretty(&json!({"version":1,"profiles":[{"workspaceId":12,"workspaceName":"测试工作区","model":DEFAULT_MODEL,"activatedAtEpochSeconds":56}],"activeWorkspaceId":12})).unwrap()).unwrap();
        let metadata = load_metadata().unwrap();
        assert_eq!(metadata.active_mode, AuthMode::Baijimu);
        assert!(metadata.active_profile_id.is_some());
        assert!(metadata_path().exists());
        assert!(!legacy_metadata_path().exists());
        verify_private_file(&metadata_path()).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unified_auth_store_selects_requested_workspace_and_newest_identity() {
        let _guard = ENVIRONMENT_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "baijimu-codex-auth-test-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let config_home = root.join("config");
        fs::create_dir_all(config_home.join("baijimu")).unwrap();
        let _config = EnvironmentRestore::set("BAIJIMU_CONFIG_HOME", &config_home);
        fs::write(shared_auth_path(), serde_json::to_vec_pretty(&json!({"schemaVersion":2,"currentEnvironment":"prod","currentWorkspaceId":1390,"environments":{"prod":{"baseUrl":"https://api.baijimu.com"}},"credentials":[{"workspaceIds":[1390],"token":"old","userId":24,"issuedAt":"2026-01-01"},{"workspaceIds":[1390],"token":"new","userId":25,"issuedAt":"2026-02-01"},{"workspaceIds":[1200],"token":"other","userId":25,"issuedAt":"2026-03-01"}]})).unwrap()).unwrap();
        let store = load_shared_credential_store().unwrap();
        let selected = select_local_machine_credential(&store, 1390).unwrap();
        assert_eq!(selected.token, "new");
        assert_eq!(selected.user_id, Some(25));
        assert_eq!(current_workspace_id().unwrap(), 1390);
        assert!(select_local_machine_credential(&store, 9999).is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn production_website_auth_endpoint_maps_to_api_origin() {
        assert_eq!(
            normalize_baijimu_root_url("https://www.baijimu.com/lowcode3/"),
            "https://api.baijimu.com"
        );
        assert_eq!(
            normalize_baijimu_root_url("https://api.baijimu.com/lowcode3"),
            "https://api.baijimu.com"
        );
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
        let _codex = EnvironmentRestore::set("CODEX_HOME", &managed_home);
        let _data = EnvironmentRestore::set("BAIJIMU_CONNECTOR_DATA_DIR", &data_dir);
        let profile = CredentialProfile {
            profile_id: "prod:user-25:client-device-a:workspace-1203".to_string(),
            environment: "prod".to_string(),
            user_id: Some(25),
            client_id: Some("device-a".to_string()),
            workspace_id: 1203,
            workspace_name: "工作区 1203".to_string(),
            model: DEFAULT_MODEL.to_string(),
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

        activate_chatgpt_profile().unwrap();
        assert_eq!(std::env::var_os("CODEX_HOME"), None);
        assert_eq!(load_metadata().unwrap().active_mode, AuthMode::Chatgpt);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn original_value_does_not_drift_after_workspace_activation_and_restart_reconcile() {
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
        let workspace_home = PathBuf::from(&profile.codex_home);
        assert_eq!(
            std::env::var_os("CODEX_HOME"),
            Some(workspace_home.into_os_string())
        );
        assert_eq!(original_codex_home(), personal_home);

        reconcile_active_user_codex_home().unwrap();
        assert_eq!(original_codex_home(), personal_home);
        activate_chatgpt_profile().unwrap();
        assert_eq!(
            std::env::var_os("CODEX_HOME"),
            Some(personal_home.into_os_string())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_pointer_activation_restores_metadata_and_previous_environment() {
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

        crate::user_environment::fail_next_activation();
        let error = activate_prepared_workspace_profile(&profile).unwrap_err();
        assert!(error.to_string().contains("已执行回滚"));
        assert_eq!(load_metadata().unwrap().active_mode, AuthMode::Chatgpt);
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
            model: DEFAULT_MODEL.to_string(),
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

    fn test_workspace_profile(data_dir: &Path, workspace_id: u64) -> CredentialProfile {
        let profile_id = format!("prod:user-25:client-device-a:workspace-{workspace_id}");
        CredentialProfile {
            profile_id,
            environment: "prod".to_string(),
            user_id: Some(25),
            client_id: Some("device-a".to_string()),
            workspace_id,
            workspace_name: format!("工作区 {workspace_id}"),
            model: DEFAULT_MODEL.to_string(),
            activated_at_epoch_seconds: 0,
            codex_home: data_dir
                .join("codex-profiles")
                .join(format!("workspace-{workspace_id}"))
                .display()
                .to_string(),
            credential_status: "verified".to_string(),
        }
    }
}
