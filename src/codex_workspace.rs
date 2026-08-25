use anyhow::{Context, Result};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{credential, process_runtime::connector_home};

const CATALOG_SCHEMA_VERSION: u32 = 1;
const CATALOG_FILE: &str = "codex-workspaces.json";
const DEFAULT_WORKSPACE_ID: &str = "default";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexWorkspace {
    pub workspace_id: String,
    pub name: String,
    pub codex_home: String,
    #[serde(default)]
    pub auth_profile_id: Option<String>,
    pub created_at_epoch_seconds: u64,
    pub updated_at_epoch_seconds: u64,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub imported: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceCatalog {
    schema_version: u32,
    active_workspace_id: String,
    workspaces: Vec<CodexWorkspace>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceState {
    pub active_workspace_id: String,
    pub workspaces: Vec<CodexWorkspace>,
}

pub fn state(default_auth_profile_id: Option<&str>) -> Result<WorkspaceState> {
    let mut catalog = load(default_auth_profile_id)?;
    normalize(&mut catalog, default_auth_profile_id)?;
    let active_workspace_id = catalog.active_workspace_id.clone();
    for workspace in &mut catalog.workspaces {
        workspace.active = workspace.workspace_id == active_workspace_id;
    }
    Ok(WorkspaceState {
        active_workspace_id,
        workspaces: catalog.workspaces,
    })
}

pub fn create(name: &str, auth_profile_id: &str) -> Result<CodexWorkspace> {
    let name = validate_name(name)?;
    credential::prepare_profile_activation(auth_profile_id)?;
    let mut catalog = load(None)?;
    anyhow::ensure!(
        !catalog
            .workspaces
            .iter()
            .any(|workspace| workspace.name.to_lowercase() == name.to_lowercase()),
        "已存在同名 Codex 工作区"
    );
    let workspace_id = unique_workspace_id(&catalog);
    let home = workspace_root().join(&workspace_id).join("codex-home");
    fs::create_dir_all(&home)
        .with_context(|| format!("创建 Codex 工作区目录失败: {}", home.display()))?;
    set_private_directory(&home)?;
    let now = now_epoch_seconds();
    let workspace = CodexWorkspace {
        workspace_id,
        name,
        codex_home: home.display().to_string(),
        auth_profile_id: Some(auth_profile_id.to_string()),
        created_at_epoch_seconds: now,
        updated_at_epoch_seconds: now,
        active: false,
        is_default: false,
        imported: false,
    };
    catalog.workspaces.push(workspace.clone());
    save(&catalog)?;
    if let Err(error) = credential::apply_profile_to_home(auth_profile_id, &home, None, false) {
        catalog
            .workspaces
            .retain(|item| item.workspace_id != workspace.workspace_id);
        save(&catalog).context("创建工作区失败，且回滚工作区目录登记失败")?;
        if home.exists() {
            fs::remove_dir_all(&home)
                .with_context(|| format!("清理创建失败的工作区目录失败: {}", home.display()))?;
        }
        return Err(error);
    }
    Ok(workspace)
}

pub fn switch_auth_profile(
    workspace_id: &str,
    auth_profile_id: &str,
    checkpoint_current: bool,
) -> Result<CodexWorkspace> {
    credential::prepare_profile_activation(auth_profile_id)?;
    let mut catalog = load(None)?;
    let workspace_index = catalog
        .workspaces
        .iter()
        .position(|workspace| workspace.workspace_id == workspace_id)
        .context("Codex 工作区不存在")?;
    let previous_profile_id = catalog.workspaces[workspace_index].auth_profile_id.clone();
    let previous_updated_at = catalog.workspaces[workspace_index].updated_at_epoch_seconds;
    let home = PathBuf::from(&catalog.workspaces[workspace_index].codex_home);
    catalog.workspaces[workspace_index].auth_profile_id = Some(auth_profile_id.to_string());
    catalog.workspaces[workspace_index].updated_at_epoch_seconds = now_epoch_seconds();
    save(&catalog)?;
    if let Err(error) = credential::apply_profile_to_home(
        auth_profile_id,
        &home,
        previous_profile_id.as_deref(),
        checkpoint_current,
    ) {
        catalog.workspaces[workspace_index].auth_profile_id = previous_profile_id;
        catalog.workspaces[workspace_index].updated_at_epoch_seconds = previous_updated_at;
        save(&catalog).context("认证通道切换失败，且回滚工作区目录登记失败")?;
        return Err(error);
    }
    Ok(catalog.workspaces[workspace_index].clone())
}

pub fn activate(workspace_id: &str) -> Result<CodexWorkspace> {
    let mut catalog = load(None)?;
    let now = now_epoch_seconds();
    let workspace = catalog
        .workspaces
        .iter_mut()
        .find(|workspace| workspace.workspace_id == workspace_id)
        .context("Codex 工作区不存在")?;
    anyhow::ensure!(
        workspace.auth_profile_id.is_some(),
        "Codex 工作区尚未选择认证通道"
    );
    workspace.updated_at_epoch_seconds = now;
    let result = workspace.clone();
    catalog.active_workspace_id = workspace_id.to_string();
    save(&catalog)?;
    Ok(result)
}

pub fn active() -> Result<CodexWorkspace> {
    let catalog = load(None)?;
    catalog
        .workspaces
        .into_iter()
        .find(|workspace| workspace.workspace_id == catalog.active_workspace_id)
        .context("当前 Codex 工作区不存在")
}

pub fn workspace(workspace_id: &str) -> Result<CodexWorkspace> {
    load(None)?
        .workspaces
        .into_iter()
        .find(|workspace| workspace.workspace_id == workspace_id)
        .context("Codex 工作区不存在")
}

pub fn refresh_auth_profile(auth_profile_id: &str) -> Result<()> {
    let catalog = load(None)?;
    for workspace in catalog
        .workspaces
        .iter()
        .filter(|workspace| workspace.auth_profile_id.as_deref() == Some(auth_profile_id))
    {
        credential::apply_profile_to_home(
            auth_profile_id,
            Path::new(&workspace.codex_home),
            None,
            false,
        )?;
    }
    Ok(())
}

fn load(default_auth_profile_id: Option<&str>) -> Result<WorkspaceCatalog> {
    let path = catalog_path();
    let mut catalog = if path.exists() {
        let bytes = fs::read(&path)
            .with_context(|| format!("读取 Codex 工作区目录失败: {}", path.display()))?;
        crate::json_compat::from_slice::<WorkspaceCatalog>(&bytes)
            .with_context(|| format!("解析 Codex 工作区目录失败: {}", path.display()))?
    } else {
        WorkspaceCatalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            active_workspace_id: DEFAULT_WORKSPACE_ID.to_string(),
            workspaces: Vec::new(),
        }
    };
    let changed = normalize(&mut catalog, default_auth_profile_id)?;
    if changed || !path.exists() {
        save(&catalog)?;
    }
    Ok(catalog)
}

fn normalize(
    catalog: &mut WorkspaceCatalog,
    default_auth_profile_id: Option<&str>,
) -> Result<bool> {
    anyhow::ensure!(
        catalog.schema_version == CATALOG_SCHEMA_VERSION,
        "Codex 工作区目录版本不受支持"
    );
    let mut changed = false;
    let default_home = credential::default_codex_home();
    if let Some(workspace) = catalog
        .workspaces
        .iter_mut()
        .find(|workspace| workspace.workspace_id == DEFAULT_WORKSPACE_ID)
    {
        let expected_home = default_home.display().to_string();
        if workspace.codex_home != expected_home {
            workspace.codex_home = expected_home;
            changed = true;
        }
        if default_auth_profile_id.is_some()
            && workspace.auth_profile_id.as_deref() != default_auth_profile_id
        {
            workspace.auth_profile_id = default_auth_profile_id.map(str::to_string);
            changed = true;
        }
        workspace.is_default = true;
        workspace.imported = false;
    } else {
        let now = now_epoch_seconds();
        catalog.workspaces.push(CodexWorkspace {
            workspace_id: DEFAULT_WORKSPACE_ID.to_string(),
            name: "默认工作区".to_string(),
            codex_home: default_home.display().to_string(),
            auth_profile_id: default_auth_profile_id.map(str::to_string),
            created_at_epoch_seconds: now,
            updated_at_epoch_seconds: now,
            active: false,
            is_default: true,
            imported: false,
        });
        changed = true;
    }
    if import_legacy_workspaces(catalog, credential::legacy_codex_workspace_candidates()?)? {
        changed = true;
    }
    if !catalog
        .workspaces
        .iter()
        .any(|workspace| workspace.workspace_id == catalog.active_workspace_id)
    {
        catalog.active_workspace_id = DEFAULT_WORKSPACE_ID.to_string();
        changed = true;
    }
    catalog.workspaces.sort_by(|left, right| {
        right
            .is_default
            .cmp(&left.is_default)
            .then_with(|| {
                right
                    .updated_at_epoch_seconds
                    .cmp(&left.updated_at_epoch_seconds)
            })
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(changed)
}

fn import_legacy_workspaces(
    catalog: &mut WorkspaceCatalog,
    candidates: Vec<credential::LegacyCodexWorkspaceCandidate>,
) -> Result<bool> {
    let mut changed = false;
    for candidate in candidates {
        let codex_home = candidate.codex_home.display().to_string();
        if catalog
            .workspaces
            .iter()
            .any(|workspace| workspace.codex_home == codex_home)
        {
            continue;
        }
        let workspace_id = imported_workspace_id(&candidate.profile_id, &candidate.codex_home);
        anyhow::ensure!(
            !catalog
                .workspaces
                .iter()
                .any(|workspace| workspace.workspace_id == workspace_id),
            "历史 Codex 工作区标识冲突: {}",
            candidate.codex_home.display()
        );
        let timestamp = candidate.initialized_at_epoch_seconds;
        catalog.workspaces.push(CodexWorkspace {
            workspace_id,
            name: candidate.workspace_name,
            codex_home,
            auth_profile_id: Some(candidate.profile_id),
            created_at_epoch_seconds: timestamp,
            updated_at_epoch_seconds: timestamp,
            active: false,
            is_default: false,
            imported: true,
        });
        changed = true;
    }
    Ok(changed)
}

fn imported_workspace_id(profile_id: &str, codex_home: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(b"baijimu-codex-imported-workspace-v1\0");
    digest.update(profile_id.as_bytes());
    digest.update(b"\0");
    digest.update(codex_home.as_os_str().to_string_lossy().as_bytes());
    format!("{:x}", digest.finalize())[..24].to_string()
}

fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "工作区名称不能为空");
    anyhow::ensure!(name.chars().count() <= 80, "工作区名称不能超过 80 个字符");
    anyhow::ensure!(
        !name.chars().any(char::is_control),
        "工作区名称不能包含控制字符"
    );
    Ok(name.to_string())
}

fn unique_workspace_id(catalog: &WorkspaceCatalog) -> String {
    loop {
        let mut bytes = [0_u8; 12];
        OsRng.fill_bytes(&mut bytes);
        let id = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if !catalog
            .workspaces
            .iter()
            .any(|workspace| workspace.workspace_id == id)
        {
            return id;
        }
    }
}

fn workspace_root() -> PathBuf {
    connector_home().join("codex-workspaces")
}

fn catalog_path() -> PathBuf {
    connector_home().join(CATALOG_FILE)
}

fn save(catalog: &WorkspaceCatalog) -> Result<()> {
    atomic_write_private(&catalog_path(), &serde_json::to_vec_pretty(catalog)?)
}

fn atomic_write_private(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().context("Codex 工作区目录文件缺少父目录")?;
    fs::create_dir_all(parent)?;
    set_private_directory(parent)?;
    let temporary = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        now_epoch_seconds()
    ));
    fs::write(&temporary, content)?;
    set_private_file(&temporary)?;
    if let Err(error) = replace_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    set_private_file(path)
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, path: &Path) -> Result<()> {
    fs::rename(temporary, path)
        .with_context(|| format!("提交 Codex 工作区目录失败: {}", path.display()))
}

#[cfg(windows)]
fn replace_file(temporary: &Path, path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let source = temporary
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let target = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("提交 Codex 工作区目录失败: {}", path.display()));
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

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_environment::TEST_ENVIRONMENT_LOCK;

    fn workspace_catalog(default_home: &Path) -> WorkspaceCatalog {
        WorkspaceCatalog {
            schema_version: CATALOG_SCHEMA_VERSION,
            active_workspace_id: DEFAULT_WORKSPACE_ID.to_string(),
            workspaces: vec![CodexWorkspace {
                workspace_id: DEFAULT_WORKSPACE_ID.to_string(),
                name: "默认工作区".to_string(),
                codex_home: default_home.display().to_string(),
                auth_profile_id: Some("personal:chatgpt".to_string()),
                created_at_epoch_seconds: 1,
                updated_at_epoch_seconds: 1,
                active: true,
                is_default: true,
                imported: false,
            }],
        }
    }

    #[test]
    fn workspace_names_are_user_data_not_path_segments() {
        assert_eq!(validate_name(" 研发 / Codex ").unwrap(), "研发 / Codex");
        assert!(validate_name("").is_err());
        assert!(validate_name(&"a".repeat(81)).is_err());
    }

    #[test]
    fn catalog_bootstrap_preserves_the_existing_home_as_default_workspace() {
        let _guard = TEST_ENVIRONMENT_LOCK.lock().unwrap();
        let previous = std::env::var_os("BAIJIMU_LOCAL_APP_DATA_DIR");
        let data_dir = std::env::temp_dir().join(format!(
            "codex-workspace-catalog-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::env::set_var("BAIJIMU_LOCAL_APP_DATA_DIR", &data_dir);

        let catalog = state(Some("personal:installation-backup")).unwrap();

        assert_eq!(catalog.active_workspace_id, DEFAULT_WORKSPACE_ID);
        assert_eq!(catalog.workspaces.len(), 1);
        assert!(catalog.workspaces[0].is_default);
        assert!(catalog.workspaces[0].active);
        assert_eq!(
            catalog.workspaces[0].auth_profile_id.as_deref(),
            Some("personal:installation-backup")
        );
        assert!(catalog_path().is_file());

        if let Some(previous) = previous {
            std::env::set_var("BAIJIMU_LOCAL_APP_DATA_DIR", previous);
        } else {
            std::env::remove_var("BAIJIMU_LOCAL_APP_DATA_DIR");
        }
        fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn legacy_homes_are_imported_as_idempotent_peer_workspaces() {
        let root = std::env::temp_dir().join(format!(
            "codex-legacy-workspace-import-{}-{}",
            std::process::id(),
            now_epoch_seconds()
        ));
        let default_home = root.join("default");
        let legacy_home = root.join("legacy");
        let mut catalog = workspace_catalog(&default_home);
        let candidate = credential::LegacyCodexWorkspaceCandidate {
            profile_id: "prod:user-25:client-device-a:workspace-1390".to_string(),
            workspace_name: "产品研发".to_string(),
            codex_home: legacy_home.clone(),
            initialized_at_epoch_seconds: 42,
        };

        assert!(import_legacy_workspaces(&mut catalog, vec![candidate.clone()]).unwrap());
        assert_eq!(catalog.workspaces.len(), 2);
        let imported = &catalog.workspaces[1];
        assert!(imported.imported);
        assert!(!imported.is_default);
        assert_eq!(imported.name, "产品研发");
        assert_eq!(imported.codex_home, legacy_home.display().to_string());
        assert_eq!(
            imported.auth_profile_id.as_deref(),
            Some(candidate.profile_id.as_str())
        );
        assert_eq!(imported.created_at_epoch_seconds, 42);
        assert!(!import_legacy_workspaces(&mut catalog, vec![candidate]).unwrap());
        assert_eq!(catalog.workspaces.len(), 2);
    }
}
