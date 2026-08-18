use super::*;

#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
pub fn default_model() -> &'static str {
    crate::product_config::get().default_model.as_str()
}

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
    pub available: bool,
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
    pub original_codex_home_state: OriginalCodexHomeState,
    pub original_codex_home: String,
    pub active_codex_home: String,
    pub external_codex_home: Option<String>,
    pub legacy_global_codex_home: LegacyGlobalCodexHomeState,
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
    pub(super) metadata: CredentialMetadata,
    pub codex_home: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingProfileHomeMigration {
    pub active_home_before: Option<PathBuf>,
    pub active_home_after: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyGlobalCodexHomeState {
    pub restore_required: bool,
    pub can_restore: bool,
    pub current_value: Option<String>,
    pub restore_value: Option<String>,
    pub restored_at_epoch_seconds: Option<u64>,
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
