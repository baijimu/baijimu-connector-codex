use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const BAIJIMU_BINARY_ENV: &str = "CODEX_DESKTOP_BAIJIMU_BINARY";
const WORKSPACE_PAGE_SIZE: u64 = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthStatus {
    pub authenticated: bool,
    pub base_url: String,
    pub current_workspace_id: Option<u64>,
    pub workspace_ids: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthStatusContract {
    authenticated: bool,
    base_url: String,
    current_workspace_id: Option<u64>,
    workspace_ids: Vec<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlatformEnvelope<T> {
    error_code: String,
    value: Option<String>,
    data: T,
}

impl<T> PlatformEnvelope<T> {
    fn into_data(self, operation: &str) -> Result<T> {
        if self.error_code != "0" {
            bail!(
                "baijimu CLI {operation} 失败（{}）：{}",
                self.error_code,
                self.value.as_deref().unwrap_or("平台操作失败")
            );
        }
        Ok(self.data)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceListContract {
    current_workspace_id: Option<u64>,
    data: WorkspaceListDataContract,
    error_code: String,
    value: Option<String>,
}

impl WorkspaceListContract {
    fn into_page(self) -> Result<WorkspacePage> {
        if self.error_code != "0" {
            bail!(
                "baijimu CLI workspace list 失败（{}）：{}",
                self.error_code,
                self.value.as_deref().unwrap_or("平台操作失败")
            );
        }
        Ok(WorkspacePage {
            current_workspace_id: self.current_workspace_id,
            items: self.data.list,
            total_pages: self.data.total_pages,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkspaceListDataContract {
    list: Vec<WorkspaceContract>,
    total_pages: u64,
}

#[derive(Clone, Debug, Deserialize)]
struct WorkspaceContract {
    id: u64,
    name: String,
}

struct WorkspacePage {
    current_workspace_id: Option<u64>,
    items: Vec<WorkspaceContract>,
    total_pages: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LlmCredentialContract {
    created: bool,
    key_type: String,
    workspace_id: u64,
    project_id: Option<u64>,
    credential: String,
}

pub fn command() -> Result<Command> {
    Ok(Command::new(binary()?))
}

pub fn auth_status() -> Result<AuthStatus> {
    let contract: AuthStatusContract = run_json("auth status", &["auth", "status"])?;
    if contract.authenticated && contract.workspace_ids.is_empty() {
        bail!("baijimu CLI 报告已认证，但授权工作区为空");
    }
    Ok(AuthStatus {
        authenticated: contract.authenticated,
        base_url: required_text(contract.base_url, "auth status.baseUrl")?,
        current_workspace_id: contract.current_workspace_id.filter(|id| *id > 0),
        workspace_ids: positive_unique_ids(contract.workspace_ids, "auth status.workspaceIds")?,
    })
}

pub fn list_workspaces() -> Result<Vec<Workspace>> {
    let mut page = 1_u64;
    let mut expected_current_workspace = None;
    let mut workspaces = BTreeMap::new();
    loop {
        let page_text = page.to_string();
        let page_size_text = WORKSPACE_PAGE_SIZE.to_string();
        let contract: WorkspaceListContract = run_json(
            "workspace list",
            &[
                "workspace",
                "list",
                "--json",
                "--page",
                &page_text,
                "--page-size",
                &page_size_text,
            ],
        )?;
        let result = contract.into_page()?;
        match expected_current_workspace {
            None => expected_current_workspace = result.current_workspace_id,
            Some(expected) if result.current_workspace_id != Some(expected) => {
                bail!("baijimu CLI workspace list 分页期间当前工作区发生变化")
            }
            _ => {}
        }
        for workspace in result.items {
            if workspace.id == 0 {
                bail!("baijimu CLI workspace list 返回了非法工作区 ID");
            }
            let name = required_text(workspace.name, "workspace list.data.list[].name")?;
            workspaces.insert(
                workspace.id,
                Workspace {
                    id: workspace.id,
                    name,
                },
            );
        }
        let total_pages = result.total_pages.max(1);
        if page >= total_pages {
            break;
        }
        page += 1;
    }
    Ok(workspaces.into_values().collect())
}

pub fn get_workspace(workspace_id: u64) -> Result<Workspace> {
    require_workspace_id(workspace_id)?;
    let workspace_text = workspace_id.to_string();
    let envelope: PlatformEnvelope<WorkspaceContract> = run_json(
        "workspace get",
        &["workspace", "get", &workspace_text, "--json"],
    )?;
    let workspace = envelope.into_data("workspace get")?;
    if workspace.id != workspace_id {
        bail!(
            "baijimu CLI workspace get 返回的工作区不匹配：expected={workspace_id}, actual={}",
            workspace.id
        );
    }
    Ok(Workspace {
        id: workspace.id,
        name: required_text(workspace.name, "workspace get.data.name")?,
    })
}

pub fn create_llm_credential(workspace_id: u64) -> Result<String> {
    require_workspace_id(workspace_id)?;
    let workspace_text = workspace_id.to_string();
    let contract: LlmCredentialContract = run_json(
        "llm-credential create",
        &[
            "llm-credential",
            "create",
            "--json",
            "--workspace-id",
            &workspace_text,
            "--show-secret",
        ],
    )?;
    if !contract.created || contract.key_type != "llmCredential" {
        bail!("baijimu CLI 未确认 LLM credential 已创建");
    }
    if contract.workspace_id != workspace_id || contract.project_id.is_some() {
        bail!("baijimu CLI 返回的 LLM credential 归属不匹配");
    }
    required_text(contract.credential, "llm-credential create.credential")
}

fn binary() -> Result<PathBuf> {
    let value = env::var_os(BAIJIMU_BINARY_ENV)
        .filter(|value| !value.is_empty())
        .context("Bridge Agent 未注入平台管理的 baijimu CLI 绝对路径；请升级或重启 Bridge Agent")?;
    validate_binary_path(PathBuf::from(value))
}

fn validate_binary_path(path: PathBuf) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("{BAIJIMU_BINARY_ENV} 必须是绝对路径，不能依赖 PATH 查找")
    }
    if !Path::new(&path).is_file() {
        bail!("Bridge Agent 注入的 baijimu CLI 不存在：{}", path.display())
    }
    Ok(path)
}

fn run_json<T>(operation: &str, args: &[&str]) -> Result<T>
where
    T: DeserializeOwned,
{
    let output = command()?
        .args(args)
        .output()
        .with_context(|| format!("启动 baijimu CLI {operation} 失败；请检查平台管理的 CLI 安装"))?;
    if !output.status.success() {
        let detail = compact_error(&String::from_utf8_lossy(&output.stderr));
        bail!(
            "baijimu CLI {operation} 失败{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!("：{detail}")
            }
        );
    }
    crate::json_compat::from_slice(&output.stdout)
        .with_context(|| format!("baijimu CLI {operation} 未返回合法 JSON"))
}

fn required_text(value: String, field: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("baijimu CLI 响应缺少 {field}");
    }
    Ok(value.to_string())
}

fn require_workspace_id(workspace_id: u64) -> Result<()> {
    if workspace_id == 0 {
        bail!("工作区 ID 必须大于 0");
    }
    Ok(())
}

fn positive_unique_ids(mut ids: Vec<u64>, field: &str) -> Result<Vec<u64>> {
    if ids.contains(&0) {
        bail!("baijimu CLI 响应中的 {field} 包含非法 ID");
    }
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

fn compact_error(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(400)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_status_contract_requires_owned_fields_and_normalizes_ids() {
        let contract: AuthStatusContract = crate::json_compat::from_slice(
            br#"{
                "authenticated": true,
                "baseUrl": "https://api.baijimu.com",
                "configuredCurrentWorkspaceId": 642,
                "credentialCount": 2,
                "currentWorkspaceId": 642,
                "sharedAuthPath": "/owned/by/baijimu-cli/auth.json",
                "verification": null,
                "workspaceIds": [1390, 642, 642]
            }"#,
        )
        .unwrap();
        let status = AuthStatus {
            authenticated: contract.authenticated,
            base_url: required_text(contract.base_url, "baseUrl").unwrap(),
            current_workspace_id: contract.current_workspace_id,
            workspace_ids: positive_unique_ids(contract.workspace_ids, "workspaceIds").unwrap(),
        };
        assert_eq!(status.workspace_ids, vec![642, 1390]);
        assert_eq!(status.current_workspace_id, Some(642));
    }

    #[test]
    fn workspace_contract_rejects_failed_envelopes() {
        let envelope: PlatformEnvelope<WorkspaceContract> = crate::json_compat::from_slice(
            r#"{
                "errorCode": "401",
                "value": "PAT 无效或已过期",
                "data": {"id": 642, "name": "不会使用"}
            }"#
            .as_bytes(),
        )
        .unwrap();
        let error = envelope.into_data("workspace get").unwrap_err();
        assert!(error.to_string().contains("PAT 无效或已过期"));
    }

    #[test]
    fn llm_credential_contract_exposes_only_the_typed_secret_field() {
        let contract: LlmCredentialContract = crate::json_compat::from_slice(
            br#"{
                "created": true,
                "keyType": "llmCredential",
                "workspaceId": 642,
                "projectId": null,
                "agentConfigId": null,
                "agentSessionId": null,
                "sessionId": null,
                "maskedLlmCredential": "secret****tail",
                "credential": "workspace-llm-secret",
                "llmCredential": "workspace-llm-secret",
                "apiKey": "workspace-llm-secret"
            }"#,
        )
        .unwrap();
        assert!(contract.created);
        assert_eq!(contract.workspace_id, 642);
        assert_eq!(contract.credential, "workspace-llm-secret");
    }

    #[test]
    fn invalid_workspace_ids_fail_closed() {
        assert!(require_workspace_id(0).is_err());
        assert!(positive_unique_ids(vec![642, 0], "workspaceIds").is_err());
    }

    #[test]
    fn managed_cli_path_requires_an_absolute_existing_file() {
        assert!(validate_binary_path(PathBuf::from("baijimu")).is_err());
        let executable = std::env::current_exe().unwrap();
        assert_eq!(
            validate_binary_path(executable.clone()).unwrap(),
            executable
        );
    }
}
