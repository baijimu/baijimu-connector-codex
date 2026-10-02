mod baijimu_cli;
mod child_process;
mod cli;
mod codex_workspace;
mod codex_workspace_activation;
mod credential;
mod desktop;
mod json_compat;
mod process_runtime;
mod product_config;
mod setup;
mod state_access;
mod system_compatibility;
mod user_environment;

use cli::run;
use process_runtime::*;
use rand::{rngs::OsRng, RngCore};
use serde_json::{json, Map, Value};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 18110;
const MANAGEMENT_TOKEN_FILE: &str = "management-token";
const CONNECTOR_HEALTH_IO_TIMEOUT: Duration = Duration::from_secs(1);
const CONNECTOR_HEALTH_MAX_RESPONSE_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug)]
struct ServerOptions {
    host: String,
    port: u16,
    daemon: bool,
}

struct AppState {
    setup: setup::SetupManager,
    management_token: String,
    startup: StartupReadiness,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum StartupPhase {
    Initializing,
    Ready,
    Failed,
}

#[derive(Clone, Debug)]
struct StartupSnapshot {
    phase: StartupPhase,
    message: String,
    error: Option<String>,
    started_at: String,
    completed_at: Option<String>,
}

#[derive(Clone)]
struct StartupReadiness {
    inner: Arc<Mutex<StartupSnapshot>>,
}

impl StartupReadiness {
    fn initializing() -> Self {
        Self {
            inner: Arc::new(Mutex::new(StartupSnapshot {
                phase: StartupPhase::Initializing,
                message: "正在初始化 Codex 桌面管理器".to_string(),
                error: None,
                started_at: timestamp(),
                completed_at: None,
            })),
        }
    }

    fn snapshot(&self) -> StartupSnapshot {
        self.inner
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_else(|_| StartupSnapshot {
                phase: StartupPhase::Failed,
                message: "Codex 桌面管理器初始化状态不可用".to_string(),
                error: Some("startup readiness lock poisoned".to_string()),
                started_at: timestamp(),
                completed_at: Some(timestamp()),
            })
    }

    fn ready(&self) {
        if let Ok(mut snapshot) = self.inner.lock() {
            snapshot.phase = StartupPhase::Ready;
            snapshot.message = "Codex 桌面管理器已就绪".to_string();
            snapshot.error = None;
            snapshot.completed_at = Some(timestamp());
        }
    }
}

impl StartupSnapshot {
    fn is_ready(&self) -> bool {
        self.phase == StartupPhase::Ready
    }

    fn status_name(&self) -> &'static str {
        match self.phase {
            StartupPhase::Initializing => "initializing",
            StartupPhase::Ready => "ready",
            StartupPhase::Failed => "failed",
        }
    }

    fn to_value(&self) -> Value {
        json!({
            "status": self.status_name(),
            "message": self.message,
            "error": self.error,
            "startedAt": self.started_at,
            "completedAt": self.completed_at,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupReadinessDecision {
    Ready,
    Start(u64),
    Initializing,
    Failed,
    NeedsWorkspace,
}

fn decide_setup_readiness(
    setup: &setup::SetupStatus,
    current_workspace_id: Option<u64>,
    current_workspace_authorized: bool,
    workspace_ready: bool,
) -> SetupReadinessDecision {
    if setup.status == "running" {
        return SetupReadinessDecision::Initializing;
    }
    let Some(workspace_id) = current_workspace_id.filter(|_| current_workspace_authorized) else {
        return SetupReadinessDecision::NeedsWorkspace;
    };
    if setup.status == "failed" && setup.workspace_id == Some(workspace_id) {
        return SetupReadinessDecision::Failed;
    }
    if setup.status == "needs_retry"
        || (setup.status == "interrupted" && setup.automatic_retry_count <= 1)
    {
        return SetupReadinessDecision::Start(workspace_id);
    }
    if setup.status == "interrupted" {
        return SetupReadinessDecision::Failed;
    }
    if setup.status == "succeeded" && setup.workspace_id == Some(workspace_id) && workspace_ready {
        SetupReadinessDecision::Ready
    } else {
        SetupReadinessDecision::Start(workspace_id)
    }
}

#[derive(Debug)]
struct HttpError {
    status: u16,
    message: String,
    code: Option<Value>,
    data: Option<Value>,
}

impl HttpError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            code: None,
            data: None,
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(500, message)
    }

    fn coded(
        status: u16,
        message: impl Into<String>,
        code: impl Into<String>,
        data: Value,
    ) -> Self {
        Self {
            status,
            message: message.into(),
            code: Some(Value::String(code.into())),
            data: Some(data),
        }
    }
}

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let result = run(args);
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn start_server(options: ServerOptions) -> Result<(), String> {
    let listener = TcpListener::bind((options.host.as_str(), options.port))
        .map_err(|error| error.to_string())?;
    let management_token = load_or_create_management_token()
        .map_err(|error| format!("failed to initialize management token: {error}"))?;
    fs::write(pid_path(), format!("{}\n", std::process::id()))
        .map_err(|error| format!("failed to record connector process id: {error}"))?;
    println!(
        "{}",
        json!({"ok": true, "url": format!("http://{}:{}", options.host, options.port), "pid": std::process::id()})
    );
    let setup = setup::SetupManager::load();
    let startup = StartupReadiness::initializing();
    // Process readiness only covers the already-bound management server. Legacy profile
    // migration and desktop activation belong to explicit management operations; running them
    // here makes Bridge Agent installation depend on user AppX inventory and desktop state.
    startup.ready();
    let state = Arc::new(AppState {
        setup,
        management_token,
        startup,
    });
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&state);
                thread::spawn(move || {
                    let _ = handle_connection(stream, state);
                });
            }
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
    Ok(())
}

fn test_control_enabled() -> bool {
    env::var("CODEX_DESKTOP_ENABLE_TEST_SHUTDOWN")
        .ok()
        .as_deref()
        == Some("1")
}

fn connector_identity() -> Value {
    json!({
        "name": "@baijimu/codex-desktop",
        "version": VERSION,
        "pid": std::process::id(),
    })
}

fn startup_response(snapshot: &StartupSnapshot) -> Value {
    json!({
        "ok": snapshot.is_ready(),
        "status": {
            "connector": connector_identity(),
            "startup": snapshot.to_value(),
        },
        "error": (!snapshot.is_ready()).then(|| json!({
            "code": if snapshot.phase == StartupPhase::Failed {
                "connector_initialization_failed"
            } else {
                "connector_initializing"
            },
            "message": snapshot.error.as_deref().unwrap_or(&snapshot.message),
        })),
    })
}

fn handle_connection(mut stream: TcpStream, state: Arc<AppState>) -> Result<(), String> {
    let request = read_http_request(&mut stream)?;
    let path = request
        .path
        .split('?')
        .next()
        .unwrap_or(request.path.as_str())
        .to_string();
    let test_shutdown = request.method == "POST"
        && path == "/__shutdown"
        && env::var("CODEX_DESKTOP_ENABLE_TEST_SHUTDOWN")
            .ok()
            .as_deref()
            == Some("1");
    if requires_management_authorization(&request.method, &path, test_shutdown)
        && !management_authorized(request.authorization.as_deref(), &state.management_token)
    {
        return write_json(
            &mut stream,
            401,
            &json!({"ok": false, "error": {"code": "UNAUTHORIZED", "message": "local app authorization required"}}),
        );
    }
    let response = match (request.method.as_str(), path.as_str()) {
        ("GET", "/healthz") => {
            let snapshot = state.startup.snapshot();
            let mut response = startup_response(&snapshot);
            response["ok"] = Value::Bool(true);
            (200, response)
        }
        ("GET", "/readyz") => {
            let snapshot = state.startup.snapshot();
            let status = if snapshot.is_ready() { 200 } else { 503 };
            (status, startup_response(&snapshot))
        }
        ("POST", "/__shutdown")
            if env::var("CODEX_DESKTOP_ENABLE_TEST_SHUTDOWN")
                .ok()
                .as_deref()
                == Some("1") =>
        {
            thread::spawn(|| {
                thread::sleep(Duration::from_millis(20));
                std::process::exit(0);
            });
            (200, json!({"ok": true}))
        }
        (method, path) if path.starts_with("/management/") => {
            if let Some(response) = startup_not_ready_response(&state.startup) {
                return write_json(&mut stream, 503, &response);
            }
            {
                let body = if request.body.is_empty() {
                    json!({})
                } else {
                    serde_json::from_slice(&request.body).map_err(|error| error.to_string())?
                };
                match handle_management(method, path, &body, &state) {
                    Ok(data) => (200, json!({"ok": true, "data": data})),
                    Err(error) => (
                        error.status,
                        json!({"ok": false, "error": {"message": error.message, "code": error.code, "data": error.data}}),
                    ),
                }
            }
        }
        _ => (404, json!({"ok": false, "error": {"message": "not found"}})),
    };
    write_json(&mut stream, response.0, &response.1)
}

fn requires_management_authorization(method: &str, path: &str, test_shutdown: bool) -> bool {
    !(test_shutdown || method == "GET" && path == "/readyz")
}

fn startup_not_ready_response(startup: &StartupReadiness) -> Option<Value> {
    let snapshot = startup.snapshot();
    (!snapshot.is_ready()).then(|| startup_response(&snapshot))
}

fn management_operation_label(path: &str) -> &str {
    match path {
        "/management/v1/prepare-local-state" => "初始化本地数据",
        "/management/v1/codex/reauthorize" => "重新授权",
        "/management/v1/codex/auth-channel" => "切换认证通道",
        "/management/v1/codex/workspaces" => "创建工作区",
        "/management/v1/codex/workspaces/activate" => "切换工作区",
        "/management/v1/codex/launch" => "打开 Codex",
        "/management/v1/codex/restart" => "重启 Codex",
        "/management/v1/codex/restore-external-home" => "恢复用户环境",
        "/management/v1/setup/verify-router" => "验证路由",
        _ => "管理操作",
    }
}

fn handle_management(
    method: &str,
    path: &str,
    body: &Value,
    state: &AppState,
) -> Result<Value, HttpError> {
    // Setup owns its reservation in the background worker. Reads never reserve it.
    let _operation = if method == "POST"
        && !matches!(
            path,
            "/management/v1/setup/ensure-ready"
                | "/management/v1/setup/retry"
                | "/management/v1/codex/initialize"
                | "/management/v1/workspace-discovery"
        ) {
        Some(
            state_access::Operation::begin(management_operation_label(path)).map_err(|error| {
                HttpError::coded(
                    409,
                    error.to_string(),
                    "OPERATION_IN_PROGRESS",
                    state_access::operation_state(),
                )
            })?,
        )
    } else {
        None
    };
    match (method, path) {
        ("POST", "/management/v1/prepare-local-state") => {
            state_access::initialize_local_data()
                .map_err(|error| HttpError::new(409, format!("本地数据初始化失败：{error:#}")))?;
            drop(_operation);
            credential_state_value()
        }
        ("POST", "/management/v1/workspace-discovery") => credential::discover_workspaces()
            .map_err(|error| HttpError::new(409, error.to_string())),
        ("GET", "/management/v1/setup/state") => serde_json::to_value(state.setup.state())
            .map_err(|error| HttpError::internal(error.to_string())),
        ("POST", "/management/v1/setup/ensure-ready") => ensure_codex_ready(state),
        ("POST", "/management/v1/setup/reveal-package") => {
            state
                .setup
                .reveal_installer_package()
                .map_err(|error| HttpError::new(409, error.to_string()))?;
            Ok(json!({"opened": true}))
        }
        ("POST", "/management/v1/setup/retry") => {
            let workspace_id = body
                .get("workspaceId")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0)
                .ok_or_else(|| HttpError::new(400, "必须提供 workspaceId"))?;
            serde_json::to_value(
                state
                    .setup
                    .start_with_elevation(
                        workspace_id,
                        true,
                        body.get("elevate")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )
                    .map_err(|error| HttpError::new(409, error.to_string()))?,
            )
            .map_err(|error| HttpError::internal(error.to_string()))
        }
        ("POST", "/management/v1/setup/verify-router") => {
            let workspace_id = body
                .get("workspaceId")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0)
                .ok_or_else(|| HttpError::new(400, "必须提供 workspaceId"))?;
            let auth = baijimu_cli::auth_status()
                .map_err(|error| HttpError::new(409, error.to_string()))?;
            if !auth.authenticated || !auth.workspace_ids.contains(&workspace_id) {
                return Err(HttpError::new(403, "当前设备授权不包含该工作区"));
            }
            let router_credential = credential::router_credential_for_workspace(workspace_id)
                .map_err(|error| HttpError::new(409, error.to_string()))?;
            serde_json::to_value(
                state
                    .setup
                    .verify_router(workspace_id, router_credential)
                    .map_err(|error| HttpError::new(409, error.to_string()))?,
            )
            .map_err(|error| HttpError::internal(error.to_string()))
        }
        ("GET", "/management/v1/credential-state") => credential_state_value(),
        ("POST", "/management/v1/codex/restore-external-home") => {
            if let Some(current) = user_environment::read_codex_home()
                .map_err(|error| HttpError::new(409, error.to_string()))?
            {
                let workspaces = codex_workspace::state()
                    .map_err(|error| HttpError::internal(error.to_string()))?;
                if codex_home_is_registered(&current, &workspaces.workspaces) {
                    return Err(HttpError::new(
                        409,
                        "当前用户级 CODEX_HOME 是已登记 Codex 工作区的活动投影，不能作为旧版残留恢复",
                    ));
                }
            }
            {
                let _write =
                    state_access::write().map_err(|e| HttpError::internal(e.to_string()))?;
                credential::restore_legacy_global_codex_home()
                    .map_err(|error| HttpError::new(409, error.to_string()))?;
            }
            user_environment::notify_environment_change().map_err(|error| {
                HttpError::new(409, format!("本地环境已恢复，但通知桌面进程失败：{error}"))
            })?;
            credential_state_value()
        }
        ("POST", "/management/v1/codex/initialize") => {
            let workspace_id = body
                .get("workspaceId")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0)
                .ok_or_else(|| HttpError::new(400, "必须提供 workspaceId"))?;
            serde_json::to_value(
                state
                    .setup
                    .start(workspace_id, false)
                    .map_err(|error| HttpError::new(409, error.to_string()))?,
            )
            .map_err(|error| HttpError::internal(error.to_string()))
        }
        ("POST", "/management/v1/codex/reauthorize") => {
            ensure_default_workspace_ready(state, "重新授权")?;
            let workspace_id = body
                .get("workspaceId")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0)
                .ok_or_else(|| HttpError::new(400, "必须提供 workspaceId"))?;
            let prepared = credential::prepare_workspace_reauthorization(workspace_id)
                .map_err(|error| HttpError::new(409, error.to_string()))?;
            let auth_profile_id = prepared.profile.profile_id.clone();
            let active_codex_workspace = codex_workspace::active()
                .map_err(|error| HttpError::internal(error.to_string()))?;
            let active =
                active_codex_workspace.auth_profile_id.as_deref() == Some(auth_profile_id.as_str());
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let desktop_switch = if active && !test_control_enabled() {
                Some(
                    desktop::stop_for_workspace_switch()
                        .map_err(desktop_compatibility_http_error)?,
                )
            } else {
                None
            };
            let commit_result = (|| -> anyhow::Result<()> {
                let _write = state_access::write()?;
                credential::commit_workspace_reauthorization(prepared)?;
                codex_workspace::refresh_auth_profile(&auth_profile_id)
            })();
            if let Err(error) = commit_result {
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                if let Some(desktop_switch) = desktop_switch {
                    desktop_switch
                        .restart_workspace_if_needed(std::path::Path::new(
                            &active_codex_workspace.codex_home,
                        ))
                        .map_err(desktop_compatibility_http_error)?;
                }
                return Err(HttpError::new(409, error.to_string()));
            }
            credential_state_value()
        }
        ("POST", "/management/v1/codex/auth-channel") => {
            ensure_default_workspace_ready(state, "切换认证通道")?;
            let profile_id = body
                .get("authProfileId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| HttpError::new(400, "必须提供 authProfileId"))?;
            let workspace_id = body
                .get("codexWorkspaceId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("default");
            let target = codex_workspace::workspace(workspace_id)
                .map_err(|error| HttpError::new(404, error.to_string()))?;
            let active_workspace = codex_workspace::active()
                .map_err(|error| HttpError::internal(error.to_string()))?;
            let is_active = active_workspace.workspace_id == workspace_id;
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let desktop_switch = if is_active && !test_control_enabled() {
                Some(
                    desktop::stop_for_workspace_switch()
                        .map_err(desktop_compatibility_http_error)?,
                )
            } else {
                None
            };
            let switched = match (|| -> anyhow::Result<_> {
                let _write = state_access::write()?;
                codex_workspace::switch_auth_profile(workspace_id, profile_id, is_active)
            })() {
                Ok(workspace) => workspace,
                Err(error) => {
                    #[cfg(any(target_os = "macos", target_os = "windows"))]
                    if let Some(desktop_switch) = desktop_switch {
                        desktop_switch
                            .restart_workspace_if_needed(std::path::Path::new(&target.codex_home))
                            .map_err(desktop_compatibility_http_error)?;
                    }
                    return Err(HttpError::new(409, error.to_string()));
                }
            };
            let _ = switched;
            credential_state_value()
        }
        ("POST", "/management/v1/codex/workspaces") => {
            ensure_default_workspace_ready(state, "新增工作区")?;
            let name = body
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| HttpError::new(400, "必须提供工作区名称"))?;
            let profile_id = body
                .get("authProfileId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| HttpError::new(400, "必须提供 authProfileId"))?;
            {
                let _write =
                    state_access::write().map_err(|e| HttpError::internal(e.to_string()))?;
                codex_workspace::create(name, profile_id)
                    .map_err(|error| HttpError::new(409, error.to_string()))?;
            }
            credential_state_value()
        }
        ("POST", "/management/v1/codex/workspaces/activate") => {
            ensure_default_workspace_ready(state, "打开工作区")?;
            let workspace_id = body
                .get("codexWorkspaceId")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| HttpError::new(400, "必须提供 codexWorkspaceId"))?;
            codex_workspace_activation::switch(workspace_id, !test_control_enabled())
                .map_err(|error| HttpError::new(409, error.to_string()))?;

            credential_state_value()
        }
        ("POST", "/management/v1/codex/launch") => {
            ensure_default_workspace_ready(state, "启动 Codex")?;
            let active = codex_workspace::active()
                .map_err(|error| HttpError::internal(error.to_string()))?;
            if !test_control_enabled() {
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                {
                    desktop::launch_workspace(Path::new(&active.codex_home))
                        .map_err(desktop_compatibility_http_error)?;
                }
            }
            Ok(json!({
                "launched": true,
                "codexWorkspaceId": active.workspace_id,
            }))
        }
        ("POST", "/management/v1/codex/restart") => {
            ensure_default_workspace_ready(state, "重启 Codex")?;
            let active = codex_workspace::active()
                .map_err(|error| HttpError::internal(error.to_string()))?;
            if !test_control_enabled() {
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                {
                    desktop::stop_for_workspace_switch()
                        .map_err(desktop_compatibility_http_error)?;
                    desktop::launch_workspace(std::path::Path::new(&active.codex_home))
                        .map_err(desktop_compatibility_http_error)?;
                }
            }
            Ok(json!({"restarted": true}))
        }
        _ => Err(HttpError::new(404, format!("未知的管理接口路径：{path}"))),
    }
}

fn state_read_http_error(error: anyhow::Error) -> HttpError {
    let committing = error.to_string().starts_with("STATE_COMMITTING:");
    HttpError::coded(
        if committing { 409 } else { 500 },
        error.to_string(),
        if committing {
            "STATE_COMMITTING"
        } else {
            "STATE_UNAVAILABLE"
        },
        state_access::operation_state(),
    )
}

fn credential_state_value() -> Result<Value, HttpError> {
    let _read = state_access::read().map_err(state_read_http_error)?;
    let credential_state =
        credential::state().map_err(|error| HttpError::internal(error.to_string()))?;
    let workspace_state =
        codex_workspace::state().map_err(|error| HttpError::internal(error.to_string()))?;
    let intentional_environment_projection = credential_state
        .external_codex_home
        .as_deref()
        .is_some_and(|home| codex_home_is_registered(Path::new(home), &workspace_state.workspaces));
    let mut value = serde_json::to_value(&credential_state)
        .map_err(|error| HttpError::internal(error.to_string()))?;
    value["codexWorkspaces"] = serde_json::to_value(&workspace_state.workspaces)
        .map_err(|error| HttpError::internal(error.to_string()))?;
    value["activeCodexWorkspaceId"] = Value::String(workspace_state.active_workspace_id.clone());
    if let Some(active) = workspace_state
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == workspace_state.active_workspace_id)
    {
        value["activeCodexWorkspace"] =
            serde_json::to_value(active).map_err(|error| HttpError::internal(error.to_string()))?;
        value["activeCodexHome"] = Value::String(active.codex_home.clone());
        if let Some(profile_id) = active.auth_profile_id.as_deref() {
            if let Some(profile) = credential_state
                .profiles
                .iter()
                .find(|profile| profile.profile_id == profile_id)
            {
                value["activeProfile"] = serde_json::to_value(profile)
                    .map_err(|error| HttpError::internal(error.to_string()))?;
            }
        }
    }
    if intentional_environment_projection {
        value["legacyGlobalCodexHome"]["restoreRequired"] = Value::Bool(false);
        value["legacyGlobalCodexHome"]["canRestore"] = Value::Bool(false);
    }
    value["operation"] = state_access::operation_state();
    value["localDataInitialization"] = state_access::initialization_state();
    value["localDataReady"] = Value::Bool(
        state_access::local_data_ready().map_err(|e| HttpError::internal(e.to_string()))?,
    );
    Ok(value)
}

fn codex_home_is_registered(home: &Path, workspaces: &[codex_workspace::CodexWorkspace]) -> bool {
    workspaces.iter().any(|workspace| {
        user_environment::codex_homes_match(home, Path::new(&workspace.codex_home))
    })
}

fn ensure_default_workspace_ready(state: &AppState, operation: &str) -> Result<(), HttpError> {
    if state.setup.state().status == "succeeded"
        && state_access::local_data_ready().map_err(|e| HttpError::new(409, e.to_string()))?
    {
        return Ok(());
    }
    Err(HttpError::new(
        409,
        format!("Codex 尚未完成安装配置，不能{operation}"),
    ))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn desktop_compatibility_http_error(error: anyhow::Error) -> HttpError {
    if let Some(unsupported) = system_compatibility::unsupported_os_version(&error) {
        return HttpError::coded(
            409,
            unsupported.to_string(),
            system_compatibility::ERROR_CODE_UNSUPPORTED_OS_VERSION,
            json!({
                "platform": unsupported.platform(),
                "currentVersion": unsupported.current_version(),
                "minimumVersion": unsupported.minimum_version(),
                "application": unsupported.application(),
            }),
        );
    }
    HttpError::new(409, format!("{error:#}"))
}

fn setup_readiness_value(
    readiness: &str,
    message: impl Into<String>,
    setup: setup::SetupStatus,
) -> Value {
    json!({
        "readiness": readiness,
        "message": message.into(),
        "setup": setup,
    })
}

fn ensure_codex_ready(state: &AppState) -> Result<Value, HttpError> {
    // This is an explicit command. Local migration/bootstrap never occurs in GET.
    let local_ready = {
        let _read = state_access::read().map_err(state_read_http_error)?;
        state_access::local_data_ready().map_err(|e| HttpError::new(409, e.to_string()))?
    };
    if !local_ready {
        let _operation = state_access::Operation::begin("初始化本地数据")
            .map_err(|e| HttpError::new(409, e.to_string()))?;
        state_access::initialize_local_data()
            .map_err(|e| HttpError::new(409, format!("本地数据初始化失败：{e:#}")))?;
    }
    let auth = baijimu_cli::auth_status().map_err(|e| HttpError::new(409, e.to_string()))?;
    let current_workspace_id = auth.current_workspace_id;
    let current_workspace_authorized = auth.authenticated
        && current_workspace_id.is_some_and(|id| auth.workspace_ids.contains(&id));
    let setup_status = state.setup.state();
    let workspace_ready = {
        let _read = state_access::read().map_err(state_read_http_error)?;
        current_workspace_id.is_some_and(credential::codex_ready_for_workspace)
    };
    match decide_setup_readiness(
        &setup_status,
        current_workspace_id,
        current_workspace_authorized,
        workspace_ready,
    ) {
        SetupReadinessDecision::Ready => Ok(setup_readiness_value(
            "ready",
            "Codex 桌面环境已就绪",
            setup_status,
        )),
        SetupReadinessDecision::Start(workspace_id) => {
            let setup_status = state
                .setup
                .start(workspace_id, false)
                .map_err(|error| HttpError::new(409, error.to_string()))?;
            Ok(setup_readiness_value(
                "initializing",
                "正在自动下载安装并配置 Codex 桌面环境",
                setup_status,
            ))
        }
        SetupReadinessDecision::Initializing => Ok(setup_readiness_value(
            "initializing",
            "正在自动下载安装并配置本机 Codex",
            setup_status,
        )),
        SetupReadinessDecision::Failed => Ok(setup_readiness_value(
            "failed",
            setup_status
                .error
                .clone()
                .unwrap_or_else(|| "Codex 初始化失败，请检查失败步骤后重试".to_string()),
            setup_status,
        )),
        SetupReadinessDecision::NeedsWorkspace => Ok(setup_readiness_value(
            "needs_workspace",
            "当前百积木账号没有明确且已授权的工作区，请先完成工作区授权",
            setup_status,
        )),
    }
}

fn management_authorized(header: Option<&str>, expected: &str) -> bool {
    let provided = header
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default()
        .as_bytes();
    let expected = expected.as_bytes();
    if provided.len() != expected.len() {
        return false;
    }
    provided
        .iter()
        .zip(expected)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    let mut buffer = Vec::new();
    let mut temp = [0_u8; 4096];
    let headers_end;
    loop {
        let n = stream.read(&mut temp).map_err(|error| error.to_string())?;
        if n == 0 {
            return Err("connection closed".to_string());
        }
        buffer.extend_from_slice(&temp[..n]);
        if let Some(end) = find_headers_end(&buffer) {
            headers_end = end;
            break;
        }
    }
    let content_length = parse_content_length(&buffer[..headers_end]).unwrap_or(0);
    let body_start = headers_end + 4;
    while buffer.len() < body_start + content_length {
        let n = stream.read(&mut temp).map_err(|error| error.to_string())?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&temp[..n]);
    }
    let header_text = String::from_utf8_lossy(&buffer[..headers_end]);
    let request_line = header_text.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let authorization = header_text.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().to_string())
    });
    Ok(HttpRequest {
        method: parts.next().unwrap_or_default().to_string(),
        path: parts.next().unwrap_or_default().to_string(),
        authorization,
        body: buffer[body_start..].to_vec(),
    })
}

struct HttpRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

fn write_json(stream: &mut TcpStream, status: u16, payload: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(payload).map_err(|error| error.to_string())?;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .and_then(|_| stream.write_all(&body))
        .map_err(|error| error.to_string())
}

fn find_headers_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn parse_content_length(headers: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(headers);
    for line in text.lines() {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                return value.trim().parse().ok();
            }
        }
    }
    None
}

#[cfg(test)]
mod http_authorization_tests {
    use super::requires_management_authorization;

    #[test]
    fn bridge_agent_readiness_probe_is_public() {
        assert!(!requires_management_authorization("GET", "/readyz", false));
    }

    #[test]
    fn application_and_management_routes_remain_protected() {
        assert!(requires_management_authorization("GET", "/healthz", false));
        assert!(requires_management_authorization(
            "POST",
            "/management/v1/setup/retry",
            false,
        ));
        assert!(requires_management_authorization(
            "POST",
            "/management/v1/setup/reveal-package",
            false
        ));
        assert!(requires_management_authorization("POST", "/readyz", false));
    }
}

fn print_help() {
    println!(
        "baijimu-codex-desktop {VERSION}\n\nUsage:\n  baijimu-codex-desktop start [--host 127.0.0.1] [--port 18110] [--daemon]\n  baijimu-codex-desktop status\n  baijimu-codex-desktop stop\n  baijimu-codex-desktop credential-state\n  baijimu-codex-desktop --version"
    );
}

#[cfg(all(test, any()))]
mod project_state_tests {
    use super::*;

    #[test]
    fn event_delivery_retries_only_temporary_failures() {
        for status in [408, 429, 500, 503] {
            assert!(retryable_event_status(status), "status {status}");
        }
        for status in [400, 401, 403, 404, 409, 422] {
            assert!(!retryable_event_status(status), "status {status}");
        }
    }

    #[test]
    fn startup_readiness_separates_liveness_from_initialization() {
        let startup = StartupReadiness::initializing();
        let initializing = startup.snapshot();
        assert_eq!(initializing.phase, StartupPhase::Initializing);
        assert!(!initializing.is_ready());
        assert_eq!(
            startup_response(&initializing)["error"]["code"],
            "connector_initializing"
        );

        startup.ready();
        let ready = startup.snapshot();
        assert_eq!(ready.phase, StartupPhase::Ready);
        assert!(ready.is_ready());
        assert_eq!(startup_response(&ready)["ok"], true);
    }

    #[test]
    fn startup_readiness_preserves_the_initialization_root_cause() {
        let startup = StartupReadiness::initializing();
        startup.fail("Connector 元数据初始化超时".to_string());

        let failed = startup.snapshot();
        let response = startup_response(&failed);
        assert_eq!(failed.phase, StartupPhase::Failed);
        assert_eq!(response["error"]["code"], "connector_initialization_failed");
        assert_eq!(response["error"]["message"], "Connector 元数据初始化超时");
    }

    #[test]
    fn reads_global_state_json_with_utf8_bom() {
        let path = env::temp_dir().join(format!(
            "baijimu-codex-global-state-bom-{}",
            std::process::id()
        ));
        fs::write(&path, "\u{feff}{\"projects\":[\"one\"]}").unwrap();

        assert_eq!(read_json_file(&path), json!({"projects": ["one"]}));
        fs::remove_file(path).unwrap();
    }

    fn setup_status(status: &str, workspace_id: Option<u64>) -> setup::SetupStatus {
        setup::SetupStatus {
            status: status.to_string(),
            workspace_id,
            message: status.to_string(),
            error: (status == "failed").then(|| "installer failed".to_string()),
            retryable: matches!(status, "failed" | "interrupted" | "needs_retry"),
            ..setup::SetupStatus::default()
        }
    }

    #[test]
    fn automatic_setup_readiness_covers_install_repair_and_manual_retry_states() {
        assert_eq!(
            decide_setup_readiness(
                &setup_status("pending", None),
                Some(642),
                true,
                false,
                false
            ),
            SetupReadinessDecision::Start(642)
        );
        assert_eq!(
            decide_setup_readiness(
                &setup_status("succeeded", Some(642)),
                Some(642),
                true,
                true,
                true,
            ),
            SetupReadinessDecision::Ready
        );
        assert_eq!(
            decide_setup_readiness(
                &setup_status("succeeded", Some(642)),
                Some(642),
                true,
                false,
                true,
            ),
            SetupReadinessDecision::Start(642)
        );
        assert_eq!(
            decide_setup_readiness(
                &setup_status("running", Some(642)),
                Some(642),
                true,
                false,
                false,
            ),
            SetupReadinessDecision::Initializing
        );
        assert_eq!(
            decide_setup_readiness(
                &setup_status("failed", Some(642)),
                Some(642),
                true,
                false,
                false,
            ),
            SetupReadinessDecision::Failed
        );
        assert_eq!(
            decide_setup_readiness(
                &setup_status("failed", Some(100)),
                Some(642),
                true,
                false,
                false,
            ),
            SetupReadinessDecision::Start(642)
        );
        assert_eq!(
            decide_setup_readiness(
                &setup_status("interrupted", Some(642)),
                Some(642),
                true,
                false,
                false,
            ),
            SetupReadinessDecision::Start(642)
        );
        assert_eq!(
            decide_setup_readiness(
                &setup_status("needs_retry", Some(642)),
                Some(642),
                true,
                false,
                false,
            ),
            SetupReadinessDecision::Start(642)
        );
        let mut repeated_interruption = setup_status("interrupted", Some(642));
        repeated_interruption.automatic_retry_count = 2;
        assert_eq!(
            decide_setup_readiness(&repeated_interruption, Some(642), true, false, false,),
            SetupReadinessDecision::Failed
        );
        assert_eq!(
            decide_setup_readiness(
                &setup_status("pending", None),
                Some(642),
                false,
                false,
                false
            ),
            SetupReadinessDecision::NeedsWorkspace
        );
    }

    fn health_options(port: u16) -> ServerOptions {
        ServerOptions {
            host: DEFAULT_HOST.to_string(),
            port,
            listen: DEFAULT_LISTEN.to_string(),
            extra_args: Vec::new(),
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
            daemon: false,
        }
    }

    #[test]
    fn connector_health_accepts_a_bounded_healthy_response() {
        let listener = TcpListener::bind((DEFAULT_HOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            assert!(stream.read(&mut request).unwrap() > 0);
            let body = r#"{"ok":true,"status":{"connector":{"pid":7}}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });

        let health = connector_health(&health_options(port)).unwrap();

        server.join().unwrap();
        assert_eq!(health["ok"], true);
        assert_eq!(health.pointer("/status/connector/pid"), Some(&json!(7)));
    }

    #[test]
    fn connector_health_read_has_a_hard_timeout() {
        let listener = TcpListener::bind((DEFAULT_HOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(3));
        });
        let started_at = Instant::now();

        let error = connector_health(&health_options(port)).unwrap_err();

        assert!(!error.is_empty());
        assert!(
            started_at.elapsed() < Duration::from_secs(2),
            "health probe waited beyond its configured I/O timeout"
        );
    }

    #[test]
    fn connector_health_rejects_an_oversized_response() {
        let listener = TcpListener::bind((DEFAULT_HOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            assert!(stream.read(&mut request).unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .unwrap();
            stream
                .write_all(&vec![
                    b'x';
                    CONNECTOR_HEALTH_MAX_RESPONSE_BYTES as usize + 1
                ])
                .unwrap();
        });

        let error = connector_health(&health_options(port)).unwrap_err();

        server.join().unwrap();
        assert!(error.contains("response exceeds"), "{error}");
    }

    #[test]
    fn connector_stop_pid_requires_the_codex_health_identity() {
        let valid = json!({
            "status": {
                "connector": {
                    "name": "@baijimu/codex-desktop",
                    "pid": 42
                }
            }
        });
        assert_eq!(verified_connector_pid(&valid).unwrap(), 42);

        let unrelated = json!({
            "status": {
                "connector": {
                    "name": "another-service",
                    "pid": 42
                }
            }
        });
        assert!(verified_connector_pid(&unrelated)
            .unwrap_err()
            .contains("does not belong"));
    }

    #[test]
    fn resolves_current_project_ids_and_keeps_legacy_paths() {
        let local_root = env::temp_dir().join("codex-current-project");
        let assigned_root = env::temp_dir().join("codex-assigned-project");
        let legacy_root = env::temp_dir().join("codex-legacy-project");
        let state = json!({
            "local-projects": {
                "local-current": {
                    "id": "local-current",
                    "name": "Current Project",
                    "rootPaths": [local_root]
                }
            },
            "thread-project-assignments": {
                "thread-1": {
                    "projectId": "remote-project-id",
                    "cwd": assigned_root
                }
            }
        });

        let resolved = resolve_state_project_references(
            &state,
            vec![
                "local-current".to_string(),
                "remote-project-id".to_string(),
                "local-unresolved".to_string(),
                legacy_root.display().to_string(),
            ],
        );

        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0].path, local_root.display().to_string());
        assert_eq!(resolved[0].project_id.as_deref(), Some("local-current"));
        assert_eq!(resolved[0].project_name.as_deref(), Some("Current Project"));
        assert_eq!(resolved[1].path, assigned_root.display().to_string());
        assert_eq!(resolved[1].project_id.as_deref(), Some("remote-project-id"));
        assert_eq!(resolved[2].path, legacy_root.display().to_string());
        assert_eq!(resolved[2].project_id, None);
    }
}

#[cfg(test)]
mod query_isolation_tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Fixture {
        root: PathBuf,
        env: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }
    impl Fixture {
        fn new() -> Self {
            let root = env::temp_dir().join(format!(
                "codex-query-isolation-{}-{}",
                std::process::id(),
                timestamp()
            ));
            fs::create_dir_all(&root).unwrap();
            let values = [
                ("HOME", root.join("user")),
                ("USERPROFILE", root.join("user")),
                ("BAIJIMU_CONFIG_HOME", root.join("config")),
                ("BAIJIMU_LOCAL_APP_DATA_DIR", root.join("data")),
                ("CODEX_HOME", root.join("existing-codex")),
                (
                    "CODEX_DESKTOP_BAIJIMU_BINARY",
                    root.join("must-not-call-cli"),
                ),
            ];
            let env = values
                .into_iter()
                .map(|(name, value)| {
                    let previous = env::var_os(name);
                    env::set_var(name, value);
                    (name, previous)
                })
                .collect();
            Self { root, env }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for (name, value) in &self.env {
                if let Some(value) = value {
                    env::set_var(name, value);
                } else {
                    env::remove_var(name);
                }
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
        let mut result = BTreeMap::new();
        if root.exists() {
            for entry in fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    result.extend(snapshot(&path));
                } else {
                    result.insert(
                        path.clone(),
                        (
                            fs::read(&path).unwrap(),
                            fs::metadata(path).unwrap().modified().unwrap(),
                        ),
                    );
                }
            }
        }
        result
    }
    #[test]
    fn queries_are_pure_during_a_long_operation_and_fail_fast_during_commits() {
        let _env = user_environment::TEST_ENVIRONMENT_LOCK.lock().unwrap();
        let fixture = Fixture::new();
        let before = snapshot(&fixture.root);
        let empty = credential_state_value().unwrap();
        assert_eq!(empty["localDataReady"], false);
        assert_eq!(empty["codexWorkspaces"], json!([]));
        assert_eq!(
            snapshot(&fixture.root),
            before,
            "first GET must not bootstrap"
        );

        let app = AppState {
            setup: setup::SetupManager::load(),
            management_token: "test-only-token".to_string(),
            startup: StartupReadiness::initializing(),
        };
        let prepared = handle_management(
            "POST",
            "/management/v1/prepare-local-state",
            &json!({}),
            &app,
        )
        .unwrap();
        assert_eq!(prepared["operation"]["running"], false);
        assert_eq!(prepared["localDataInitialization"]["status"], "ready");
        let initialized = snapshot(&fixture.root);
        let operation = state_access::Operation::begin("等待桌面进程退出").unwrap();
        let started = Instant::now();
        let response = thread::spawn(credential_state_value)
            .join()
            .unwrap()
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(response["operation"]["running"], true);
        assert_eq!(response["localDataReady"], true);
        assert!(state_access::Operation::begin("并发切换").is_err());
        assert_eq!(snapshot(&fixture.root), initialized);
        {
            let _commit = state_access::write().unwrap();
            let error = thread::spawn(credential_state_value)
                .join()
                .unwrap()
                .unwrap_err();
            assert_eq!(error.status, 409);
            assert_eq!(error.code, Some(json!("STATE_COMMITTING")));
        }
        drop(operation);
        for _ in 0..3 {
            assert!(credential_state_value().is_ok());
        }
        assert_eq!(snapshot(&fixture.root), initialized);

        // Explicit initialization is idempotent, including catalog timestamps.
        state_access::initialize_local_data().unwrap();
        assert_eq!(snapshot(&fixture.root), initialized);
        fs::write(
            fixture.root.join("data/codex-credentials.json"),
            b"{invalid",
        )
        .unwrap();
        let corrupt = snapshot(&fixture.root);
        assert!(credential_state_value().is_err());
        assert_eq!(
            snapshot(&fixture.root),
            corrupt,
            "GET must not repair corrupt files"
        );
        assert!(handle_management(
            "POST",
            "/management/v1/prepare-local-state",
            &json!({}),
            &app
        )
        .is_err());
        assert_eq!(state_access::initialization_state()["status"], "failed");
        assert_eq!(snapshot(&fixture.root), corrupt);
    }
}
