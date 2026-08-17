use serde::{Deserialize, Serialize};
#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
use std::error::Error;
#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
use std::fmt;

#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
pub const MACOS_STEP_NAMES: [&str; 7] = [
    "检查 ChatGPT 桌面应用",
    "读取应用安装包清单",
    "下载 ChatGPT 桌面应用",
    "校验并安装应用",
    "创建百积木 LLM 凭证和配置",
    "验证百积木路由",
    "完成安装配置",
];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum InstallerStepState {
    Pending,
    Running,
    Completed,
    Skipped,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstallerStep {
    pub index: usize,
    pub name: String,
    pub state: InstallerStepState,
    pub detail: String,
    pub downloaded_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstallerStatus {
    pub title: String,
    pub locale: String,
    pub platform: String,
    pub started_at: String,
    pub updated_at: String,
    pub current_step: usize,
    pub status_path: String,
    pub result_path: String,
    pub steps: Vec<InstallerStep>,
}

#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
impl InstallerStatus {
    pub fn macos(started_at: String, status_path: String, result_path: String) -> Self {
        Self {
            title: "百积木正在安装 ChatGPT 桌面应用".to_string(),
            locale: "zh-CN".to_string(),
            platform: "macos".to_string(),
            updated_at: started_at.clone(),
            started_at,
            current_step: 0,
            status_path,
            result_path,
            steps: MACOS_STEP_NAMES
                .iter()
                .enumerate()
                .map(|(offset, name)| InstallerStep {
                    index: offset + 1,
                    name: (*name).to_string(),
                    state: InstallerStepState::Pending,
                    detail: String::new(),
                    downloaded_bytes: None,
                    total_bytes: None,
                })
                .collect(),
        }
    }

    pub fn update_step(
        &mut self,
        index: usize,
        state: InstallerStepState,
        detail: impl Into<String>,
        downloaded_bytes: Option<u64>,
        total_bytes: Option<u64>,
        updated_at: String,
    ) -> Result<(), InstallerProgressError> {
        let step_count = self.steps.len();
        let step = self
            .steps
            .get_mut(index.saturating_sub(1))
            .filter(|step| step.index == index)
            .ok_or(InstallerProgressError { index, step_count })?;
        step.state = state;
        step.detail = detail.into();
        step.downloaded_bytes = downloaded_bytes;
        step.total_bytes = total_bytes;
        self.current_step = index;
        self.updated_at = updated_at;
        Ok(())
    }

    pub fn complete_pending(
        &mut self,
        state: InstallerStepState,
        detail: impl Into<String>,
        updated_at: String,
    ) {
        let detail = detail.into();
        for step in &mut self.steps {
            if step.state == InstallerStepState::Pending {
                step.state = state.clone();
                step.detail = detail.clone();
            }
        }
        self.updated_at = updated_at;
    }
}

#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallerProgressError {
    pub index: usize,
    pub step_count: usize,
}

#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
impl fmt::Display for InstallerProgressError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "安装步骤 {} 超出有效范围 1..={}",
            self.index, self.step_count
        )
    }
}

#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
impl Error for InstallerProgressError {}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallerResultEnvelope {
    pub ok: bool,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MacosInstallerResult {
    pub ok: bool,
    pub platform: String,
    pub started_at: String,
    pub codex_home: String,
    pub app_installed: bool,
    pub app_install_method: String,
    pub app_path: String,
    pub version: String,
    pub bundle_id: String,
    pub workspace_id: u64,
    pub project_id: Option<u64>,
    pub llm_credential_created: bool,
    pub config_written: bool,
    pub auth_written: bool,
    pub router_http_status: Option<u16>,
    pub model: String,
    pub elapsed_ms: u128,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[cfg(any(target_os = "macos", all(test, not(target_os = "windows"))))]
impl MacosInstallerResult {
    pub fn pending(
        started_at: String,
        codex_home: String,
        workspace_id: u64,
        model: String,
    ) -> Self {
        Self {
            ok: false,
            platform: "macos".to_string(),
            started_at,
            codex_home,
            app_installed: false,
            app_install_method: String::new(),
            app_path: String::new(),
            version: String::new(),
            bundle_id: String::new(),
            workspace_id,
            project_id: None,
            llm_credential_created: false,
            config_written: false,
            auth_written: false,
            router_http_status: None,
            model,
            elapsed_ms: 0,
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;

    #[test]
    fn typed_progress_updates_only_the_requested_step() {
        let mut status = InstallerStatus::macos(
            "2026-08-14T00:00:00Z".to_string(),
            "/tmp/status.json".to_string(),
            "/tmp/result.json".to_string(),
        );
        status
            .update_step(
                3,
                InstallerStepState::Running,
                "正在下载",
                Some(123),
                Some(1_000),
                "2026-08-14T00:00:01Z".to_string(),
            )
            .unwrap();

        assert_eq!(status.current_step, 3);
        assert_eq!(status.steps[2].detail, "正在下载");
        assert_eq!(status.steps[2].downloaded_bytes, Some(123));
        assert_eq!(status.steps[2].total_bytes, Some(1_000));
        assert_eq!(status.steps[6].detail, "");
        assert_eq!(status.steps[6].state, InstallerStepState::Pending);
    }

    #[test]
    fn typed_progress_rejects_unknown_steps() {
        let mut status = InstallerStatus::macos(
            "2026-08-14T00:00:00Z".to_string(),
            "/tmp/status.json".to_string(),
            "/tmp/result.json".to_string(),
        );
        let error = status
            .update_step(
                8,
                InstallerStepState::Running,
                "invalid",
                None,
                None,
                "2026-08-14T00:00:01Z".to_string(),
            )
            .unwrap_err();
        assert_eq!(error.index, 8);
        assert_eq!(error.step_count, 7);
    }

    #[test]
    fn installer_status_contract_rejects_unknown_fields() {
        let payload = r#"{
          "title":"安装", "locale":"zh-CN", "platform":"macos",
          "startedAt":"start", "updatedAt":"now", "currentStep":0,
          "statusPath":"status", "resultPath":"result", "steps":[],
          "unexpected":true
        }"#;
        assert!(serde_json::from_slice::<InstallerStatus>(payload.as_bytes()).is_err());
    }
}
