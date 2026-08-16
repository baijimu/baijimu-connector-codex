mod baijimu_cli;
mod child_process;
mod codex_binary;
mod credential;
mod desktop;
mod events;
mod json_compat;
mod project_checkout;
mod setup;
mod system_compatibility;
mod thread_state;
mod user_environment;

use rand::{rngs::OsRng, RngCore};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 18110;
const DEFAULT_LISTEN: &str = "stdio://";
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 120_000;
const MAX_EVENTS: usize = 1000;
const DEFAULT_PROJECT_LIMIT: usize = 100;
const DEFAULT_PROJECT_THREAD_PAGE_LIMIT: usize = 100;
const DEFAULT_PROJECT_THREAD_MAX_PAGES: usize = 100;
const DEFAULT_THREAD_SORT_KEY: &str = "updated_at";
const DEFAULT_THREAD_SORT_DIRECTION: &str = "desc";
const MANAGEMENT_TOKEN_FILE: &str = "management-token";
const CONNECTOR_HEALTH_IO_TIMEOUT: Duration = Duration::from_secs(1);
const CONNECTOR_HEALTH_MAX_RESPONSE_BYTES: u64 = 64 * 1024;
const DOMAIN_EVENT_PUBLISH_ATTEMPTS: usize = 5;
const DOMAIN_EVENT_RETRY_BASE_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
struct ServerOptions {
    host: String,
    port: u16,
    listen: String,
    extra_args: Vec<String>,
    request_timeout_ms: u64,
    daemon: bool,
}

#[derive(Clone, Debug)]
struct ConnectorEvent {
    sequence: u64,
    received_at: String,
    method: String,
    params: Value,
}

struct CodexClient {
    options: ServerOptions,
    active_codex_home: PathBuf,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
    initialized: bool,
    next_id: u64,
    events: VecDeque<ConnectorEvent>,
    event_sequence: u64,
    started_at: Option<String>,
    last_exit: Option<Value>,
    codex_binary_resolution: Option<codex_binary::Resolution>,
    codex_binary_error: Option<codex_binary::ResolutionError>,
    codex_cli_inspection: Option<codex_binary::CliInspection>,
    event_publisher: Option<EventPublisher>,
    event_stream_id: String,
}

#[derive(Clone)]
struct EventPublisher {
    endpoint: String,
    token: String,
    client: reqwest::blocking::Client,
}

struct AppState {
    client: Mutex<CodexClient>,
    credential_management: Mutex<()>,
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
                message: "正在初始化 Codex Connector 本机环境".to_string(),
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
                message: "Codex Connector 初始化状态不可用".to_string(),
                error: Some("startup readiness lock poisoned".to_string()),
                started_at: timestamp(),
                completed_at: Some(timestamp()),
            })
    }

    fn ready(&self) {
        if let Ok(mut snapshot) = self.inner.lock() {
            snapshot.phase = StartupPhase::Ready;
            snapshot.message = "Codex Connector 已就绪".to_string();
            snapshot.error = None;
            snapshot.completed_at = Some(timestamp());
        }
    }

    fn fail(&self, error: String) {
        if let Ok(mut snapshot) = self.inner.lock() {
            snapshot.phase = StartupPhase::Failed;
            snapshot.message = "Codex Connector 初始化失败".to_string();
            snapshot.error = Some(error);
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
    cli_ready: bool,
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
    if setup.status == "succeeded"
        && setup.workspace_id == Some(workspace_id)
        && cli_ready
        && workspace_ready
    {
        SetupReadinessDecision::Ready
    } else {
        SetupReadinessDecision::Start(workspace_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StateProjectRoot {
    path: String,
    project_id: Option<String>,
    project_name: Option<String>,
    root_paths: Vec<String>,
}

impl CodexClient {
    fn new(options: ServerOptions, event_publisher: Option<EventPublisher>) -> Self {
        let mut client = Self {
            options,
            active_codex_home: credential::active_codex_home(),
            child: None,
            stdin: None,
            stdout: None,
            initialized: false,
            next_id: 1,
            events: VecDeque::new(),
            event_sequence: 0,
            started_at: None,
            last_exit: None,
            codex_binary_resolution: None,
            codex_binary_error: None,
            codex_cli_inspection: None,
            event_publisher,
            event_stream_id: random_event_id(),
        };
        client.refresh_codex_binary_resolution();
        client
    }

    fn refresh_codex_binary_resolution(&mut self) {
        match codex_binary::resolve() {
            Ok(resolution) => {
                self.codex_binary_resolution = Some(resolution);
                self.codex_binary_error = None;
            }
            Err(error) => {
                self.codex_cli_inspection = None;
                self.codex_binary_resolution = None;
                self.codex_binary_error = Some(error);
            }
        }
    }

    fn status(&mut self) -> Value {
        let (running, pid) = match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => {
                    self.initialized = false;
                    self.last_exit = Some(json!({
                        "code": status.code(),
                        "signal": null,
                        "at": timestamp(),
                    }));
                    (false, Some(child.id()))
                }
                Ok(None) => (true, Some(child.id())),
                Err(error) => {
                    self.initialized = false;
                    self.last_exit = Some(json!({"error": error.to_string(), "at": timestamp()}));
                    (false, Some(child.id()))
                }
            },
            None => (false, None),
        };
        let binary_status = if let Some(resolution) = &self.codex_binary_resolution {
            resolution.status_value(self.codex_cli_inspection.as_ref())
        } else if let Some(error) = &self.codex_binary_error {
            error.status_value()
        } else {
            json!({
                "mode": "auto",
                "resolved": null,
                "source": null,
                "checkedPaths": [],
                "version": null,
                "appServerSupported": null,
                "inspectionError": null,
                "error": null,
            })
        };
        let active_binary = self
            .codex_binary_resolution
            .as_ref()
            .map(|resolution| resolution.path.display().to_string())
            .unwrap_or_default();
        json!({
            "connector": {
                "name": "@baijimu/connector-codex",
                "version": VERSION,
                "pid": std::process::id(),
            },
            "appServer": {
                "running": running,
                "initialized": self.initialized,
                "pid": pid,
                "codexBinary": active_binary,
                "codexBinaryResolution": binary_status,
                "listen": self.options.listen,
                "startedAt": self.started_at,
                "lastExit": self.last_exit,
                "codexHome": self.active_codex_home,
            },
            "events": {
                "latestSequence": self.event_sequence,
                "retained": self.events.len(),
            }
        })
    }

    fn ensure_started(&mut self) -> Result<(), HttpError> {
        self.refresh_active_home();
        let running = self
            .child
            .as_mut()
            .is_some_and(|child| child.try_wait().ok().flatten().is_none());
        if !running {
            self.start_process()?;
        }
        if !self.initialized {
            self.initialize()?;
        }
        Ok(())
    }

    fn refresh_active_home(&mut self) {
        let desired_home = credential::active_codex_home();
        if desired_home != self.active_codex_home {
            self.shutdown();
            self.active_codex_home = desired_home;
        }
    }

    fn switch_to_active_profile(&mut self) {
        self.shutdown();
        self.active_codex_home = credential::active_codex_home();
    }

    fn start_process(&mut self) -> Result<(), HttpError> {
        let resolution = match codex_binary::resolve() {
            Ok(resolution) => resolution,
            Err(error) => {
                let message = error.to_string();
                let data = error.data_value();
                self.codex_binary_resolution = None;
                self.codex_binary_error = Some(error);
                self.codex_cli_inspection = None;
                return Err(HttpError::coded(
                    500,
                    message,
                    "CODEX_BINARY_NOT_FOUND",
                    data,
                ));
            }
        };
        let resolved_binary = resolution.path.clone();
        let inspection = codex_binary::inspect(&resolution);
        if self.options.extra_args.is_empty() && !inspection.app_server_supported {
            let message = inspection.error.clone().unwrap_or_else(|| {
                "the selected Codex CLI does not support app-server".to_string()
            });
            self.codex_cli_inspection = Some(inspection);
            self.codex_binary_resolution = Some(resolution);
            return Err(HttpError::coded(
                500,
                message.clone(),
                "CODEX_APP_SERVER_UNSUPPORTED",
                json!({"resolved": resolved_binary, "error": message}),
            ));
        }
        self.codex_cli_inspection = Some(inspection);
        self.codex_binary_resolution = Some(resolution);
        self.codex_binary_error = None;
        let args = if self.options.extra_args.is_empty() {
            vec![
                "app-server".to_string(),
                "--listen".to_string(),
                self.options.listen.clone(),
            ]
        } else {
            self.options.extra_args.clone()
        };
        let mut command = Command::new(&resolved_binary);
        child_process::isolate_from_connector_environment(&mut command);
        let spawn_result = command
            .args(args)
            .env("CODEX_HOME", &self.active_codex_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawn_result {
            Ok(child) => child,
            Err(error) => {
                let message = format!(
                    "failed to start Codex executable '{}': {error}",
                    resolved_binary.display()
                );
                self.codex_binary_error = Some(codex_binary::ResolutionError {
                    checked_paths: vec![resolved_binary.clone()],
                    reason: error.to_string(),
                });
                self.codex_binary_resolution = None;
                self.codex_cli_inspection = None;
                return Err(HttpError::coded(
                    500,
                    message,
                    "CODEX_PROCESS_START_FAILED",
                    json!({"resolved": resolved_binary, "error": error.to_string()}),
                ));
            }
        };
        self.stdin = child.stdin.take();
        self.stdout = child.stdout.take().map(BufReader::new);
        self.initialized = false;
        self.started_at = Some(timestamp());
        self.last_exit = None;
        if let Some(stderr) = child.stderr.take() {
            let events = Arc::new(Mutex::new(()));
            let _guard = events;
            thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    line.clear();
                }
            });
        }
        self.child = Some(child);
        Ok(())
    }

    fn initialize(&mut self) -> Result<(), HttpError> {
        let result = self.request_inner(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "baijimu_connector_codex",
                    "title": "Baijimu Codex Connector",
                    "version": VERSION,
                },
                "capabilities": {
                    "experimentalApi": true,
                }
            }),
            30_000,
            true,
        )?;
        let _ = result;
        self.send_notification("initialized", json!({}))?;
        self.initialized = true;
        Ok(())
    }

    fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout_ms: Option<u64>,
    ) -> Result<Value, HttpError> {
        self.ensure_started()?;
        self.request_inner(
            method,
            params,
            timeout_ms.unwrap_or(self.options.request_timeout_ms),
            false,
        )
    }

    fn request_inner(
        &mut self,
        method: &str,
        params: Value,
        timeout_ms: u64,
        skip_started: bool,
    ) -> Result<Value, HttpError> {
        if !skip_started && !self.initialized {
            self.ensure_started()?;
        }
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"method": method, "id": id, "params": params});
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| HttpError::internal("codex app-server is not writable"))?;
        writeln!(stdin, "{message}").map_err(|error| HttpError::internal(error.to_string()))?;
        stdin
            .flush()
            .map_err(|error| HttpError::internal(error.to_string()))?;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut line = String::new();
        loop {
            if Instant::now() >= deadline {
                return Err(HttpError::internal(format!(
                    "codex app-server request timed out: {method}"
                )));
            }
            line.clear();
            let read = self
                .stdout
                .as_mut()
                .ok_or_else(|| HttpError::internal("codex app-server stdout is unavailable"))?
                .read_line(&mut line)
                .map_err(|error| HttpError::internal(error.to_string()))?;
            if read == 0 {
                return Err(HttpError::internal("codex app-server exited"));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(trimmed) {
                Ok(value) => value,
                Err(error) => {
                    self.push_event(
                        "connector/parseError",
                        json!({"line": trimmed, "error": error.to_string()}),
                    );
                    continue;
                }
            };
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = value.get("error") {
                    return Err(HttpError {
                        status: 500,
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("codex app-server error")
                            .to_string(),
                        code: error.get("code").cloned(),
                        data: error.get("data").cloned(),
                    });
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            if value.get("id").is_some() {
                self.push_event("connector/unmatchedResponse", value);
            } else {
                let method = value
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("codex/notification")
                    .to_string();
                let params = value.get("params").cloned().unwrap_or(value);
                self.push_event(&method, params);
            }
        }
    }

    fn send_notification(&mut self, method: &str, params: Value) -> Result<(), HttpError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| HttpError::internal("codex app-server is not writable"))?;
        writeln!(stdin, "{}", json!({"method": method, "params": params}))
            .map_err(|error| HttpError::internal(error.to_string()))?;
        stdin
            .flush()
            .map_err(|error| HttpError::internal(error.to_string()))
    }

    fn push_event(&mut self, method: &str, params: Value) {
        if method == "turn/completed" {
            if let Some(thread_id) = params.get("threadId").and_then(Value::as_str) {
                if let Err(error) = thread_state::mark_thread_unread(&connector_home(), thread_id) {
                    eprintln!("failed to persist unread Codex thread {thread_id}: {error}");
                }
            }
        }
        self.event_sequence += 1;
        let received_at = timestamp();
        let domain_event = match events::normalize_codex_notification(
            method,
            &params,
            &received_at,
            &self.event_stream_id,
            self.event_sequence,
        ) {
            Ok(event) => event,
            Err(error) => {
                eprintln!("failed to normalize Codex domain event {method}: {error}");
                None
            }
        };
        let event = ConnectorEvent {
            sequence: self.event_sequence,
            received_at,
            method: method.to_string(),
            params,
        };
        if let Some(publisher) = self.event_publisher.clone() {
            publisher.publish_raw(event.clone());
            if let Some(domain_event) = domain_event {
                publisher.publish_domain(domain_event);
            }
        }
        self.events.push_back(event);
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
    }

    fn recent_events(&self, body: &Value) -> Value {
        let after_sequence = body
            .get("afterSequence")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let limit = body
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(100)
            .clamp(1, 500) as usize;
        let events = self
            .events
            .iter()
            .filter(|event| event.sequence > after_sequence)
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|event| {
                json!({
                    "sequence": event.sequence,
                    "receivedAt": event.received_at,
                    "method": event.method,
                    "params": event.params,
                })
            })
            .collect::<Vec<_>>();
        json!({"latestSequence": self.event_sequence, "events": events})
    }

    fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin = None;
        self.stdout = None;
        self.initialized = false;
        self.started_at = None;
    }
}

impl EventPublisher {
    fn from_env() -> Option<Self> {
        let endpoint = env::var("BAIJIMU_CONNECTOR_EVENT_ENDPOINT").ok()?;
        let token_path = env::var("BAIJIMU_CONNECTOR_EVENT_TOKEN_FILE").ok()?;
        let token = fs::read_to_string(token_path).ok()?.trim().to_string();
        if endpoint.trim().is_empty() || token.is_empty() {
            return None;
        }
        Some(Self {
            endpoint,
            token,
            client: reqwest::blocking::Client::new(),
        })
    }

    fn publish_raw(&self, event: ConnectorEvent) {
        self.publish(
            "codexNotification",
            random_event_id(),
            event.received_at.clone(),
            json!({
                "sequence": event.sequence,
                "receivedAt": event.received_at,
                "method": event.method,
                "params": event.params,
            }),
            1,
        );
    }

    fn publish_domain(&self, event: events::DomainEvent) {
        self.publish(
            event.name,
            event.event_id,
            event.occurred_at,
            event.payload,
            DOMAIN_EVENT_PUBLISH_ATTEMPTS,
        );
    }

    fn publish(
        &self,
        event_name: &'static str,
        event_id: String,
        occurred_at: String,
        payload: Value,
        attempts: usize,
    ) {
        let publisher = self.clone();
        thread::spawn(move || {
            let request = json!({
                "connectorId": "com.baijimu.connector.codex",
                "event": event_name,
                "eventId": event_id,
                "payload": payload,
                "occurredAt": occurred_at,
            });
            let attempts = attempts.max(1);
            for attempt in 1..=attempts {
                match publisher
                    .client
                    .post(&publisher.endpoint)
                    .bearer_auth(&publisher.token)
                    .json(&request)
                    .send()
                {
                    Ok(response) if response.status().is_success() => return,
                    Ok(response) if !retryable_event_status(response.status().as_u16()) => {
                        eprintln!(
                            "failed to publish {event_name}: bridge returned HTTP {}",
                            response.status()
                        );
                        return;
                    }
                    Ok(response) if attempt == attempts => {
                        eprintln!(
                            "failed to publish {event_name} after {attempts} attempts: bridge returned HTTP {}",
                            response.status()
                        );
                    }
                    Err(error) if attempt == attempts => {
                        eprintln!(
                            "failed to publish {event_name} after {attempts} attempts: {error}"
                        );
                    }
                    Ok(_) | Err(_) => {}
                }
                if attempt < attempts {
                    let multiplier = 1_u32 << (attempt - 1).min(8);
                    thread::sleep(DOMAIN_EVENT_RETRY_BASE_DELAY * multiplier);
                }
            }
        });
    }
}

fn retryable_event_status(status: u16) -> bool {
    status == 408 || status == 429 || status >= 500
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

fn run(args: Vec<String>) -> Result<(), String> {
    let parsed = parse_args(&args)?;
    match parsed.command.as_str() {
        "--version" => {
            println!("{VERSION}");
            Ok(())
        }
        "help" | "" => {
            print_help();
            Ok(())
        }
        "start" => {
            let options = server_options(&parsed)?;
            if options.daemon {
                daemonize(&options)
            } else {
                start_server(options)
            }
        }
        "status" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "pidPath": pid_path(),
                    "pid": fs::read_to_string(pid_path()).ok().map(|value| value.trim().to_string()),
                    "logPath": log_path(),
                }))
                .unwrap()
            );
            Ok(())
        }
        "stop" => {
            let options = server_options(&parsed)?;
            let Ok(health) = connector_health(&options) else {
                println!(
                    "{}",
                    json!({"ok": true, "stopped": false, "reason": "healthy connector process not found"})
                );
                return Ok(());
            };
            let pid = verified_connector_pid(&health)?;
            terminate_process(pid)?;
            let _ = fs::remove_file(pid_path());
            println!("{}", json!({"ok": true, "stopped": true, "pid": pid}));
            Ok(())
        }
        "credential-state" => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &credential::state().map_err(|error| error.to_string())?
                )
                .map_err(|error| error.to_string())?
            );
            Ok(())
        }
        "checkout-project" => {
            let request = project_checkout::CheckoutRequest {
                workspace_id: required_u64_arg(&parsed, "workspaceId")?,
                project_id: required_u64_arg(&parsed, "projectId")?,
                branch: string_arg(&parsed, "branch"),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &project_checkout::prepare(request).map_err(|error| error.to_string())?
                )
                .map_err(|error| error.to_string())?
            );
            Ok(())
        }
        other => Err(format!("unknown command: {other}")),
    }
}

fn string_arg(parsed: &ParsedArgs, key: &str) -> Option<String> {
    parsed
        .values
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn required_u64_arg(parsed: &ParsedArgs, key: &str) -> Result<u64, String> {
    string_arg(parsed, key)
        .ok_or_else(|| format!("--{} is required", to_kebab_case(key)))?
        .parse::<u64>()
        .map_err(|_| format!("--{} must be a positive integer", to_kebab_case(key)))
        .and_then(|value| {
            if value == 0 {
                Err(format!(
                    "--{} must be greater than zero",
                    to_kebab_case(key)
                ))
            } else {
                Ok(value)
            }
        })
}

#[derive(Default)]
struct ParsedArgs {
    command: String,
    values: Map<String, Value>,
    flags: Map<String, Value>,
}

fn parse_args(args: &[String]) -> Result<ParsedArgs, String> {
    let mut parsed = ParsedArgs {
        command: args.first().cloned().unwrap_or_else(|| "help".to_string()),
        ..Default::default()
    };
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if !arg.starts_with("--") {
            index += 1;
            continue;
        }
        let raw = &arg[2..];
        let (key, inline) = raw.split_once('=').unwrap_or((raw, ""));
        let key = to_camel_case(key);
        if key == "codexBinary" {
            return Err(
                "--codex-binary is no longer supported; Codex CLI discovery is automatic"
                    .to_string(),
            );
        }
        if matches!(key.as_str(), "daemon" | "help" | "version") {
            parsed.flags.insert(key, Value::Bool(true));
            index += 1;
            continue;
        }
        let value = if inline.is_empty() {
            index += 1;
            args.get(index)
                .ok_or_else(|| format!("missing value for --{raw}"))?
                .clone()
        } else {
            inline.to_string()
        };
        parsed.values.insert(key, Value::String(value));
        index += 1;
    }
    if parsed.flags.get("version").and_then(Value::as_bool) == Some(true) {
        parsed.command = "--version".to_string();
    }
    Ok(parsed)
}

fn server_options(parsed: &ParsedArgs) -> Result<ServerOptions, String> {
    let value = |key: &str| parsed.values.get(key).and_then(Value::as_str);
    let extra_args = if let Some(raw) = value("codexArgs") {
        serde_json::from_str::<Vec<String>>(raw).map_err(|error| error.to_string())?
    } else if let Ok(raw) = env::var("CODEX_CONNECTOR_CODEX_ARGS") {
        serde_json::from_str::<Vec<String>>(&raw).map_err(|error| error.to_string())?
    } else {
        Vec::new()
    };
    Ok(ServerOptions {
        host: value("host")
            .map(str::to_string)
            .or_else(|| env::var("CODEX_CONNECTOR_HOST").ok())
            .unwrap_or_else(|| DEFAULT_HOST.to_string()),
        port: value("port")
            .and_then(|value| value.parse().ok())
            .or_else(|| {
                env::var("CODEX_CONNECTOR_PORT")
                    .ok()
                    .and_then(|value| value.parse().ok())
            })
            .unwrap_or(DEFAULT_PORT),
        listen: value("listen")
            .map(str::to_string)
            .or_else(|| env::var("CODEX_CONNECTOR_LISTEN").ok())
            .unwrap_or_else(|| DEFAULT_LISTEN.to_string()),
        request_timeout_ms: value("requestTimeoutMs")
            .and_then(|value| value.parse().ok())
            .or_else(|| {
                env::var("CODEX_CONNECTOR_REQUEST_TIMEOUT_MS")
                    .ok()
                    .and_then(|value| value.parse().ok())
            })
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS),
        daemon: parsed.flags.get("daemon").and_then(Value::as_bool) == Some(true),
        extra_args,
    })
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
    let state = Arc::new(AppState {
        client: Mutex::new(CodexClient::new(options, EventPublisher::from_env())),
        credential_management: Mutex::new(()),
        setup,
        management_token,
        startup: StartupReadiness::initializing(),
    });
    let initializing_state = Arc::clone(&state);
    thread::spawn(move || match initialize_server() {
        Ok(()) => initializing_state.startup.ready(),
        Err(error) => {
            eprintln!("Codex Connector initialization failed: {error}");
            initializing_state.startup.fail(error);
        }
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

fn initialize_server() -> Result<(), String> {
    if test_control_enabled() {
        if let Ok(delay_ms) = env::var("CODEX_CONNECTOR_TEST_STARTUP_DELAY_MS") {
            let delay_ms = delay_ms
                .parse::<u64>()
                .map_err(|error| format!("invalid test startup delay: {error}"))?;
            thread::sleep(Duration::from_millis(delay_ms.min(30_000)));
        }
        if let Ok(error) = env::var("CODEX_CONNECTOR_TEST_STARTUP_FAILURE") {
            if !error.trim().is_empty() {
                return Err(error);
            }
        }
    }
    let Some(migration) = credential::pending_profile_home_migration()
        .map_err(|error| format!("检查旧版 Codex 档案迁移状态失败: {error}"))?
    else {
        return Ok(());
    };
    let desktop_switch = if migration.active_home_before.is_some() {
        desktop::stop_for_codex_home_switch()
            .map_err(|error| format!("迁移旧版 Codex 档案前停止桌面应用失败: {error}"))?
    } else {
        desktop::DesktopSwitch::default()
    };
    match credential::state() {
        Ok(state) => desktop_switch
            .restart_and_verify(Path::new(&state.active_codex_home))
            .map(|_| ())
            .map_err(|error| format!("旧版 Codex 档案迁移完成，但桌面应用重启失败: {error}")),
        Err(error) => {
            let recovery_home = migration
                .active_home_before
                .as_deref()
                .filter(|path| path.exists())
                .or_else(|| {
                    migration
                        .active_home_after
                        .as_deref()
                        .filter(|path| path.exists())
                });
            let restart_error =
                recovery_home.and_then(|path| desktop_switch.restart_and_verify(path).err());
            let mut message = format!("迁移旧版 Codex 档案失败: {error}");
            if let Some(restart_error) = restart_error {
                message.push_str(&format!("；恢复桌面应用失败: {restart_error}"));
            }
            Err(message)
        }
    }
}

fn test_control_enabled() -> bool {
    env::var("CODEX_CONNECTOR_ENABLE_TEST_SHUTDOWN")
        .ok()
        .as_deref()
        == Some("1")
}

fn connector_identity() -> Value {
    json!({
        "name": "@baijimu/connector-codex",
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
            if env::var("CODEX_CONNECTOR_ENABLE_TEST_SHUTDOWN")
                .ok()
                .as_deref()
                == Some("1") =>
        {
            {
                let mut client = state
                    .client
                    .lock()
                    .map_err(|_| "client lock poisoned".to_string())?;
                client.shutdown();
            }
            thread::spawn(|| {
                thread::sleep(Duration::from_millis(20));
                std::process::exit(0);
            });
            (200, json!({"ok": true}))
        }
        ("POST", path) if path.starts_with("/invoke/") => {
            if let Some(response) = startup_not_ready_response(&state.startup) {
                return write_json(&mut stream, 503, &response);
            }
            let body = if request.body.is_empty() {
                json!({})
            } else {
                serde_json::from_slice(&request.body).map_err(|error| error.to_string())?
            };
            let mut client = state
                .client
                .lock()
                .map_err(|_| "client lock poisoned".to_string())?;
            match handle_invoke(path, &body, &mut client) {
                Ok(data) => (200, json!({"ok": true, "data": data})),
                Err(error) => (
                    error.status,
                    json!({
                        "ok": false,
                        "error": {
                            "message": error.message,
                            "code": error.code,
                            "data": error.data,
                        }
                    }),
                ),
            }
        }
        (method, path) if path.starts_with("/management/") => {
            if let Some(response) = startup_not_ready_response(&state.startup) {
                return write_json(&mut stream, 503, &response);
            }
            if !management_authorized(request.authorization.as_deref(), &state.management_token) {
                (
                    401,
                    json!({"ok": false, "error": {"message": "management authorization required"}}),
                )
            } else {
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

fn startup_not_ready_response(startup: &StartupReadiness) -> Option<Value> {
    let snapshot = startup.snapshot();
    (!snapshot.is_ready()).then(|| startup_response(&snapshot))
}

fn handle_management(
    method: &str,
    path: &str,
    body: &Value,
    state: &AppState,
) -> Result<Value, HttpError> {
    match (method, path) {
        ("GET", "/management/v1/setup/state") => serde_json::to_value(state.setup.state())
            .map_err(|error| HttpError::internal(error.to_string())),
        ("POST", "/management/v1/setup/ensure-ready") => ensure_codex_ready(state),
        ("POST", "/management/v1/setup/retry") => {
            let _credential_guard = state
                .credential_management
                .lock()
                .map_err(|_| HttpError::internal("凭证管理状态锁异常"))?;
            let workspace_id = body
                .get("workspaceId")
                .and_then(Value::as_u64)
                .filter(|value| *value > 0)
                .ok_or_else(|| HttpError::new(400, "必须提供 workspaceId"))?;
            let (codex_cli, verify_app_server_capability) = {
                let mut client = state
                    .client
                    .lock()
                    .map_err(|_| HttpError::internal("Codex 客户端状态锁异常"))?;
                client.shutdown();
                let codex_cli = usable_codex_cli_for_setup(&mut client)?;
                (codex_cli, client.options.extra_args.is_empty())
            };
            serde_json::to_value(
                state
                    .setup
                    .start(workspace_id, codex_cli, true, verify_app_server_capability)
                    .map_err(|error| HttpError::new(409, error.to_string()))?,
            )
            .map_err(|error| HttpError::internal(error.to_string()))
        }
        ("GET", "/management/v1/credential-state") => {
            let _credential_guard = state
                .credential_management
                .lock()
                .map_err(|_| HttpError::internal("凭证管理状态锁异常"))?;
            serde_json::to_value(
                credential::state().map_err(|error| HttpError::internal(error.to_string()))?,
            )
            .map_err(|error| HttpError::internal(error.to_string()))
        }
        ("POST", "/management/v1/codex/restore-external-home") => {
            let _credential_guard = state
                .credential_management
                .lock()
                .map_err(|_| HttpError::internal("凭证管理状态锁异常"))?;
            serde_json::to_value(
                credential::restore_legacy_global_codex_home()
                    .map_err(|error| HttpError::new(409, error.to_string()))?,
            )
            .map_err(|error| HttpError::internal(error.to_string()))
        }
        ("POST", "/management/v1/codex/launch") => {
            let _credential_guard = state
                .credential_management
                .lock()
                .map_err(|_| HttpError::internal("凭证管理状态锁异常"))?;
            if state.setup.state().status == "running" {
                return Err(HttpError::new(
                    409,
                    "Codex 正在安装配置，完成后再启动所选工作区",
                ));
            }
            let mode = body
                .get("mode")
                .and_then(Value::as_str)
                .ok_or_else(|| HttpError::new(400, "必须提供 mode"))?;
            let workspace_id = match mode {
                "chatgpt" => None,
                "baijimu" => Some(
                    body.get("workspaceId")
                        .and_then(Value::as_u64)
                        .filter(|value| *value > 0)
                        .ok_or_else(|| HttpError::new(400, "必须提供 workspaceId"))?,
                ),
                _ => return Err(HttpError::new(400, "mode 必须是 chatgpt 或 baijimu")),
            };
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            if !test_control_enabled() {
                desktop::verify_system_compatibility().map_err(desktop_compatibility_http_error)?;
            }
            let prepared_workspace = workspace_id
                .map(credential::prepare_workspace_profile)
                .transpose()
                .map_err(|error| HttpError::new(409, error.to_string()))?;
            let snapshot = credential::active_home_snapshot()
                .map_err(|error| HttpError::internal(error.to_string()))?;
            let previous_home = snapshot.codex_home.clone();
            let mut client = state
                .client
                .lock()
                .map_err(|_| HttpError::internal("Codex 客户端状态锁异常"))?;
            client.shutdown();
            let desktop_switch = if test_control_enabled() {
                desktop::DesktopSwitch::default()
            } else {
                desktop::stop_for_codex_home_switch().map_err(|error| {
                    client.switch_to_active_profile();
                    let _ = client.ensure_started();
                    HttpError::new(409, error.to_string())
                })?
            };
            let activation = match mode {
                "chatgpt" => credential::activate_chatgpt_profile().map(|_| ()),
                "baijimu" => credential::activate_prepared_workspace_profile(
                    &prepared_workspace.expect("prepared above").profile,
                )
                .map(|_| ()),
                _ => unreachable!("mode validated above"),
            };
            if let Err(error) = activation {
                client.switch_to_active_profile();
                let _ = client.ensure_started();
                let _ = desktop_switch.restart_and_verify(&previous_home);
                return Err(HttpError::new(409, error.to_string()));
            }
            client.switch_to_active_profile();
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let selected_home = credential::active_codex_home();
            let verification = client
                .ensure_started()
                .map_err(|error| error.message)
                .and_then(|_| {
                    if test_control_enabled() {
                        return Ok(());
                    }
                    #[cfg(any(target_os = "macos", target_os = "windows"))]
                    {
                        desktop::launch_and_verify(&selected_home)
                            .map_err(|error| error.to_string())
                    }
                    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
                    {
                        Ok(())
                    }
                })
                .and_then(|_| credential::state().map_err(|error| error.to_string()));
            match verification {
                Ok(credential_state) => serde_json::to_value(credential_state)
                    .map_err(|error| HttpError::internal(error.to_string())),
                Err(error) => {
                    client.shutdown();
                    let _ = desktop::stop_for_codex_home_switch();
                    let rollback = credential::restore_active_home(snapshot);
                    client.switch_to_active_profile();
                    let app_server_rollback =
                        client.ensure_started().err().map(|item| item.message);
                    let desktop_rollback = desktop_switch.restart_and_verify(&previous_home).err();
                    let mut message = format!("Codex 启动验证失败，已恢复原有档案：{error}");
                    if let Err(rollback) = rollback {
                        message.push_str(&format!("；状态指针回滚失败：{rollback}"));
                    }
                    if let Some(rollback) = app_server_rollback {
                        message.push_str(&format!("；app-server 回滚验证失败：{rollback}"));
                    }
                    if let Some(rollback) = desktop_rollback {
                        message.push_str(&format!("；桌面应用回滚验证失败：{rollback}"));
                    }
                    Err(HttpError::new(409, message))
                }
            }
        }
        ("POST", "/management/v1/projects/checkout") => {
            let _project_guard = state
                .credential_management
                .lock()
                .map_err(|_| HttpError::internal("项目检出状态锁异常"))?;
            let request: project_checkout::CheckoutRequest =
                serde_json::from_value(body.clone())
                    .map_err(|error| HttpError::new(400, format!("项目检出请求无效：{error}")))?;
            let result = project_checkout::prepare(request)
                .map_err(|error| HttpError::internal(error.to_string()))?;
            serde_json::to_value(result).map_err(|error| HttpError::internal(error.to_string()))
        }
        ("POST", "/management/v1/codex/projects") => {
            handle_management_invoke("/invoke/listProjects", body, state)
        }
        ("POST", "/management/v1/codex/sessions") => {
            handle_management_invoke("/invoke/listSessions", body, state)
        }
        ("POST", "/management/v1/codex/sessions/read") => {
            handle_management_invoke("/invoke/readThread", body, state)
        }
        ("POST", "/management/v1/codex/sessions/read-state") => {
            handle_management_invoke("/invoke/setThreadReadState", body, state)
        }
        ("POST", "/management/v1/codex/sessions/start") => {
            handle_management_invoke("/invoke/startThread", body, state)
        }
        ("POST", "/management/v1/codex/turns") => {
            handle_management_invoke("/invoke/listThreadTurns", body, state)
        }
        ("POST", "/management/v1/codex/turns/start") => {
            handle_management_invoke("/invoke/startTurn", body, state)
        }
        ("POST", "/management/v1/codex/turns/interrupt") => {
            handle_management_invoke("/invoke/interruptTurn", body, state)
        }
        ("POST", "/management/v1/codex/events") => {
            handle_management_invoke("/invoke/recentEvents", body, state)
        }
        _ => Err(HttpError::new(404, format!("未知的管理接口路径：{path}"))),
    }
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
    HttpError::new(409, error.to_string())
}

fn usable_codex_cli_for_setup(client: &mut CodexClient) -> Result<Option<PathBuf>, HttpError> {
    match codex_binary::resolve() {
        Ok(resolution) => {
            let inspection = codex_binary::inspect(&resolution);
            let supported =
                !client.options.extra_args.is_empty() || inspection.app_server_supported;
            let resolved = resolution.path.clone();
            client.codex_binary_resolution = Some(resolution);
            client.codex_binary_error = None;
            client.codex_cli_inspection = Some(inspection.clone());
            if supported {
                return Ok(Some(resolved));
            }
            Ok(None)
        }
        Err(error) => {
            client.codex_binary_resolution = None;
            client.codex_binary_error = Some(error);
            client.codex_cli_inspection = None;
            Ok(None)
        }
    }
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
    let _credential_guard = state
        .credential_management
        .lock()
        .map_err(|_| HttpError::internal("凭证管理状态锁异常"))?;
    let credential_state = credential::state()
        .map_err(|error| HttpError::new(409, format!("读取当前工作区授权失败：{error}")))?;
    let current_workspace_id = credential_state.current_workspace_id;
    let current_workspace_authorized = current_workspace_id.is_some_and(|workspace_id| {
        credential_state
            .workspaces
            .iter()
            .any(|workspace| workspace.workspace_id == workspace_id && workspace.authorized)
    });
    let setup_status = state.setup.state();
    let mut client = state
        .client
        .lock()
        .map_err(|_| HttpError::internal("Codex 客户端状态锁异常"))?;
    let codex_cli = usable_codex_cli_for_setup(&mut client)?;
    let verify_app_server_capability = client.options.extra_args.is_empty();
    let workspace_ready = current_workspace_id.is_some_and(credential::codex_ready_for_workspace);
    match decide_setup_readiness(
        &setup_status,
        current_workspace_id,
        current_workspace_authorized,
        codex_cli.is_some(),
        workspace_ready,
    ) {
        SetupReadinessDecision::Ready => {
            client.ensure_started().map_err(|error| {
                HttpError::coded(
                    409,
                    error.message,
                    "CODEX_START_FAILED",
                    error.data.unwrap_or_else(|| json!({})),
                )
            })?;
            Ok(setup_readiness_value(
                "ready",
                "本机 Codex 已就绪",
                setup_status,
            ))
        }
        SetupReadinessDecision::Start(workspace_id) => {
            client.shutdown();
            let setup_status = state
                .setup
                .start(workspace_id, codex_cli, false, verify_app_server_capability)
                .map_err(|error| HttpError::new(409, error.to_string()))?;
            Ok(setup_readiness_value(
                "initializing",
                "正在自动下载安装并配置本机 Codex",
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

fn handle_management_invoke(
    invoke_path: &str,
    body: &Value,
    state: &AppState,
) -> Result<Value, HttpError> {
    let mut client = state
        .client
        .lock()
        .map_err(|_| HttpError::internal("Codex 客户端状态锁异常"))?;
    handle_invoke(invoke_path, body, &mut client)
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

fn handle_invoke(path: &str, body: &Value, client: &mut CodexClient) -> Result<Value, HttpError> {
    match path {
        "/invoke/status" => Ok(client.status()),
        "/invoke/listThreads" | "/invoke/listSessions" => list_threads(body, client),
        "/invoke/listProjects" => list_projects(body, client),
        "/invoke/searchThreads" => {
            if string_field(body, "searchTerm").is_none()
                && body
                    .pointer("/params/searchTerm")
                    .and_then(Value::as_str)
                    .is_none()
            {
                return Err(HttpError::new(400, "searchTerm is required"));
            }
            let params = merge_params(
                body,
                &[
                    "cursor",
                    "limit",
                    "sortKey",
                    "sortDirection",
                    "sourceKinds",
                    "archived",
                    "searchTerm",
                ],
            );
            Ok(json!({"result": client.request("thread/search", params, timeout_ms(body))?}))
        }
        "/invoke/readThread" => {
            let Some(thread_id) = string_field(body, "threadId").or_else(|| {
                body.pointer("/params/threadId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }) else {
                return Err(HttpError::new(400, "threadId is required"));
            };
            let mut params = merge_params(body, &["threadId", "includeTurns"]);
            params["threadId"] = Value::String(thread_id);
            Ok(json!({"result": client.request("thread/read", params, timeout_ms(body))?}))
        }
        "/invoke/setThreadReadState" => {
            let Some(thread_id) = string_param(body, "threadId") else {
                return Err(HttpError::new(400, "threadId is required"));
            };
            let Some(has_unread_turn) = body
                .get("hasUnreadTurn")
                .or_else(|| body.pointer("/params/hasUnreadTurn"))
                .and_then(Value::as_bool)
            else {
                return Err(HttpError::new(400, "hasUnreadTurn is required"));
            };
            let mut observed_updated_at = body
                .get("observedUpdatedAt")
                .or_else(|| body.pointer("/params/observedUpdatedAt"))
                .cloned();
            if observed_updated_at.is_none() {
                let result = client.request(
                    "thread/read",
                    json!({"threadId": thread_id, "includeTurns": false}),
                    timeout_ms(body),
                )?;
                observed_updated_at = result.pointer("/thread/updatedAt").cloned();
            }
            client.refresh_active_home();
            let result = thread_state::set_thread_read_state(
                &connector_home(),
                &client.active_codex_home,
                &thread_id,
                has_unread_turn,
                observed_updated_at,
            )
            .map_err(HttpError::internal)?;
            Ok(json!({"result": result}))
        }
        "/invoke/listThreadTurns" => list_thread_turns(body, client),
        "/invoke/listApps" => {
            let params = merge_params(body, &["cursor", "limit", "threadId", "forceRefetch"]);
            Ok(json!({"result": client.request("app/list", params, timeout_ms(body))?}))
        }
        "/invoke/startThread" => {
            let mut params = merge_params(body, &[]);
            if let Some(model) = body.get("model").cloned() {
                params["model"] = model;
            }
            if let Some(cwd) = body.get("cwd").cloned() {
                params["cwd"] = cwd;
            }
            Ok(json!({"result": client.request("thread/start", params, timeout_ms(body))?}))
        }
        "/invoke/resumeThread" => {
            let Some(thread_id) = string_field(body, "threadId") else {
                return Err(HttpError::new(400, "threadId is required"));
            };
            let mut params = merge_params(body, &[]);
            params["threadId"] = Value::String(thread_id);
            for key in ["excludeTurns", "initialTurnsPage"] {
                if let Some(value) = body.get(key).cloned() {
                    params[key] = value;
                }
            }
            Ok(json!({"result": client.request("thread/resume", params, timeout_ms(body))?}))
        }
        "/invoke/startTurn" => {
            let Some(thread_id) = string_field(body, "threadId") else {
                return Err(HttpError::new(400, "threadId is required"));
            };
            let Some(input) = body.get("input").cloned() else {
                return Err(HttpError::new(400, "input is required"));
            };
            let mut params = merge_params(body, &[]);
            params["threadId"] = Value::String(thread_id);
            params["input"] = normalize_input(input);
            if let Some(model) = body.get("model").cloned() {
                params["model"] = model;
            }
            if let Some(cwd) = body.get("cwd").cloned() {
                params["cwd"] = cwd;
            }
            Ok(json!({
                "result": client.request("turn/start", params, timeout_ms(body))?,
                "recentEvents": client.recent_events(&json!({"limit": 50})),
            }))
        }
        "/invoke/steerTurn" => {
            let Some(input) = body.get("input").cloned() else {
                return Err(HttpError::new(400, "input is required"));
            };
            let mut params = merge_params(body, &[]);
            for key in ["threadId", "turnId"] {
                if let Some(value) = body.get(key).cloned() {
                    params[key] = value;
                }
            }
            params["input"] = normalize_input(input);
            Ok(json!({
                "result": client.request("turn/steer", params, timeout_ms(body))?,
                "recentEvents": client.recent_events(&json!({"limit": 50})),
            }))
        }
        "/invoke/interruptTurn" => {
            let mut params = merge_params(body, &[]);
            for key in ["threadId", "turnId"] {
                if let Some(value) = body.get(key).cloned() {
                    params[key] = value;
                }
            }
            Ok(json!({
                "result": client.request("turn/interrupt", params, timeout_ms(body))?,
                "recentEvents": client.recent_events(&json!({"limit": 50})),
            }))
        }
        "/invoke/recentEvents" => Ok(client.recent_events(body)),
        "/invoke/request" => {
            let Some(method) = string_field(body, "method") else {
                return Err(HttpError::new(400, "method is required"));
            };
            let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
            Ok(json!({
                "result": client.request(&method, params, timeout_ms(body))?,
                "recentEvents": client.recent_events(&json!({"limit": 50})),
            }))
        }
        _ => Err(HttpError::new(404, format!("unknown invoke path: {path}"))),
    }
}

fn list_threads(body: &Value, client: &mut CodexClient) -> Result<Value, HttpError> {
    let mut params = merge_params(
        body,
        &[
            "cursor",
            "limit",
            "sortKey",
            "sortDirection",
            "modelProviders",
            "sourceKinds",
            "archived",
            "cwd",
            "useStateDbOnly",
            "searchTerm",
        ],
    );
    if params.get("sortKey").is_none() {
        params["sortKey"] = Value::String(DEFAULT_THREAD_SORT_KEY.to_string());
    }
    if params.get("sortDirection").is_none() {
        params["sortDirection"] = Value::String(DEFAULT_THREAD_SORT_DIRECTION.to_string());
    }
    let mut result = client.request("thread/list", params, timeout_ms(body))?;
    if let Some(data) = result.get_mut("data").and_then(Value::as_array_mut) {
        for item in data.iter_mut() {
            *item = normalize_thread_list_item(item.clone());
        }
        client.refresh_active_home();
        thread_state::enrich_thread_list(&connector_home(), &client.active_codex_home, data)
            .map_err(HttpError::internal)?;
    }
    Ok(json!({"result": result}))
}

fn list_thread_turns(body: &Value, client: &mut CodexClient) -> Result<Value, HttpError> {
    let Some(thread_id) = string_field(body, "threadId").or_else(|| {
        body.pointer("/params/threadId")
            .and_then(Value::as_str)
            .map(str::to_string)
    }) else {
        return Ok(json!({"result": {"data": [], "nextCursor": null, "backwardsCursor": null}}));
    };
    let mut params = merge_params(
        body,
        &["threadId", "cursor", "limit", "sortDirection", "itemsView"],
    );
    params["threadId"] = Value::String(thread_id.clone());
    match client.request("thread/turns/list", params, timeout_ms(body)) {
        Ok(result) => Ok(json!({"result": result})),
        Err(error) => {
            client.push_event(
                "connector/threadTurnsListFallback",
                json!({"threadId": thread_id, "error": error.message, "code": error.code}),
            );
            let result = client.request(
                "thread/read",
                json!({"threadId": thread_id, "includeTurns": true}),
                timeout_ms(body),
            )?;
            let turns = result
                .pointer("/thread/turns")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Ok(
                json!({"result": {"data": turns, "nextCursor": null, "backwardsCursor": null, "fallback": "thread/read"}}),
            )
        }
    }
}

fn list_projects(body: &Value, client: &mut CodexClient) -> Result<Value, HttpError> {
    client.refresh_active_home();
    let home = client.active_codex_home.clone();
    let global_state = read_json_file(&home.join(".codex-global-state.json"));
    let configured = parse_codex_project_config(&home.join("config.toml"));
    let mut projects = Map::new();
    let mut saved_roots = resolve_state_project_references(
        &global_state,
        array_strings(global_state.get("project-order")),
    );
    saved_roots.extend(resolve_state_project_references(
        &global_state,
        array_strings(global_state.get("electron-saved-workspace-roots")),
    ));
    let saved_roots = unique_state_project_roots(saved_roots);
    let saved_order = saved_roots
        .iter()
        .map(|project| project.path.clone())
        .collect::<Vec<_>>();
    if bool_param(body, "includeSaved", true) {
        for project in &saved_roots {
            upsert_project(
                &mut projects,
                &project.path,
                "saved",
                state_project_fields(project),
            );
        }
    }
    for project in resolve_state_project_references(
        &global_state,
        array_strings(global_state.get("active-workspace-roots")),
    ) {
        let mut fields = state_project_fields(&project);
        fields.insert("active".to_string(), Value::Bool(true));
        upsert_project(&mut projects, &project.path, "active", fields);
    }
    for project in resolve_state_project_references(
        &global_state,
        array_strings(global_state.get("pinned-project-ids")),
    ) {
        let mut fields = state_project_fields(&project);
        fields.insert("pinned".to_string(), Value::Bool(true));
        upsert_project(&mut projects, &project.path, "pinned", fields);
    }
    if bool_param(body, "includeTrusted", true) {
        for (path, trust_level) in configured {
            let mut fields = Map::new();
            fields.insert("trustLevel".to_string(), Value::String(trust_level));
            upsert_project(&mut projects, &path, "trusted", fields);
        }
    }
    read_thread_projects(body, client, &mut projects)?;
    let search_term = string_param(body, "searchTerm")
        .unwrap_or_default()
        .to_lowercase();
    let exists_only = bool_param(body, "existsOnly", false);
    let mut items = projects
        .into_values()
        .filter(|project| {
            if exists_only && project.get("exists").and_then(Value::as_bool) != Some(true) {
                return false;
            }
            if search_term.is_empty() {
                return true;
            }
            project
                .get("title")
                .and_then(Value::as_str)
                .is_some_and(|value| value.to_lowercase().contains(&search_term))
                || project
                    .get("path")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.to_lowercase().contains(&search_term))
        })
        .collect::<Vec<_>>();
    items.sort_by(|left, right| {
        let lp = left.get("pinned").and_then(Value::as_bool).unwrap_or(false);
        let rp = right
            .get("pinned")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if lp != rp {
            return rp.cmp(&lp);
        }
        let lpath = left.get("path").and_then(Value::as_str).unwrap_or("");
        let rpath = right.get("path").and_then(Value::as_str).unwrap_or("");
        let li = saved_order
            .iter()
            .position(|path| path == lpath)
            .unwrap_or(usize::MAX);
        let ri = saved_order
            .iter()
            .position(|path| path == rpath)
            .unwrap_or(usize::MAX);
        li.cmp(&ri).then_with(|| {
            left.get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .cmp(right.get("title").and_then(Value::as_str).unwrap_or(""))
        })
    });
    let total = items.len();
    let cursor = usize_param(body, "cursor", 0);
    let limit = usize_param(body, "limit", DEFAULT_PROJECT_LIMIT).clamp(1, 500);
    let page = items
        .into_iter()
        .skip(cursor)
        .take(limit)
        .collect::<Vec<_>>();
    let next_cursor = if cursor + limit < total {
        Value::String((cursor + limit).to_string())
    } else {
        Value::Null
    };
    Ok(
        json!({"result": {"projects": page, "items": page, "total": total, "nextCursor": next_cursor, "codexHome": home}}),
    )
}

fn resolve_state_project_references(
    global_state: &Value,
    references: Vec<String>,
) -> Vec<StateProjectRoot> {
    let known_projects = state_project_roots(global_state);
    let mut resolved = Vec::new();
    for reference in references {
        let matches = known_projects
            .iter()
            .filter(|project| project.project_id.as_deref() == Some(reference.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !matches.is_empty() {
            resolved.extend(matches);
            continue;
        }
        let Some(path) = normalize_state_project_path(&reference) else {
            continue;
        };
        if let Some(project) = known_projects
            .iter()
            .find(|project| project.path == path)
            .cloned()
        {
            resolved.push(project);
        } else {
            resolved.push(StateProjectRoot {
                path: path.clone(),
                project_id: None,
                project_name: None,
                root_paths: vec![path],
            });
        }
    }
    unique_state_project_roots(resolved)
}

fn state_project_roots(global_state: &Value) -> Vec<StateProjectRoot> {
    let mut projects = Vec::new();
    if let Some(local_projects) = global_state
        .get("local-projects")
        .and_then(Value::as_object)
    {
        for (fallback_id, value) in local_projects {
            let project_id = value
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(fallback_id)
                .to_string();
            let project_name = value
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .map(str::to_string);
            let root_paths = unique_strings(
                array_strings(value.get("rootPaths"))
                    .into_iter()
                    .filter_map(|path| normalize_state_project_path(&path))
                    .collect(),
            );
            for path in &root_paths {
                projects.push(StateProjectRoot {
                    path: path.clone(),
                    project_id: Some(project_id.clone()),
                    project_name: project_name.clone(),
                    root_paths: root_paths.clone(),
                });
            }
        }
    }

    let mut assignment_roots: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(assignments) = global_state
        .get("thread-project-assignments")
        .and_then(Value::as_object)
    {
        for assignment in assignments.values() {
            let Some(project_id) = assignment.get("projectId").and_then(Value::as_str) else {
                continue;
            };
            let Some(path) = assignment
                .get("cwd")
                .and_then(Value::as_str)
                .and_then(normalize_state_project_path)
            else {
                continue;
            };
            let roots = assignment_roots.entry(project_id.to_string()).or_default();
            if !roots.contains(&path) {
                roots.push(path);
            }
        }
    }
    for (project_id, root_paths) in assignment_roots {
        if projects
            .iter()
            .any(|project| project.project_id.as_deref() == Some(project_id.as_str()))
        {
            continue;
        }
        for path in &root_paths {
            projects.push(StateProjectRoot {
                path: path.clone(),
                project_id: Some(project_id.clone()),
                project_name: None,
                root_paths: root_paths.clone(),
            });
        }
    }
    unique_state_project_roots(projects)
}

fn unique_state_project_roots(projects: Vec<StateProjectRoot>) -> Vec<StateProjectRoot> {
    let mut out = Vec::new();
    for project in projects {
        if !out
            .iter()
            .any(|candidate: &StateProjectRoot| candidate.path == project.path)
        {
            out.push(project);
        }
    }
    out
}

fn state_project_fields(project: &StateProjectRoot) -> Map<String, Value> {
    let mut fields = Map::new();
    if let Some(project_id) = &project.project_id {
        fields.insert("projectId".to_string(), Value::String(project_id.clone()));
    }
    if let Some(project_name) = &project.project_name {
        fields.insert(
            "projectName".to_string(),
            Value::String(project_name.clone()),
        );
        fields.insert("title".to_string(), Value::String(project_name.clone()));
    }
    fields.insert(
        "rootPaths".to_string(),
        Value::Array(
            project
                .root_paths
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    fields
}

fn normalize_state_project_path(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = if trimmed == "~" {
        home_dir()
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    if !path.is_absolute() {
        return None;
    }
    Some(path.display().to_string())
}

fn read_thread_projects(
    body: &Value,
    client: &mut CodexClient,
    projects: &mut Map<String, Value>,
) -> Result<(), HttpError> {
    if !bool_param(body, "includeThreadStats", true) {
        return Ok(());
    }
    let mut cursor = body
        .get("threadCursor")
        .or_else(|| body.pointer("/params/threadCursor"))
        .cloned()
        .unwrap_or(Value::Null);
    let max_pages =
        usize_param(body, "maxThreadPages", DEFAULT_PROJECT_THREAD_MAX_PAGES).clamp(1, 500);
    let limit =
        usize_param(body, "threadPageLimit", DEFAULT_PROJECT_THREAD_PAGE_LIMIT).clamp(1, 100);
    for _ in 0..max_pages {
        let mut params = json!({
            "cursor": cursor,
            "limit": limit,
            "sortKey": string_param(body, "sortKey").unwrap_or_else(|| DEFAULT_THREAD_SORT_KEY.to_string()),
            "sortDirection": string_param(body, "sortDirection").unwrap_or_else(|| DEFAULT_THREAD_SORT_DIRECTION.to_string()),
        });
        for key in ["archived", "useStateDbOnly"] {
            if let Some(value) = body
                .get(key)
                .or_else(|| body.pointer(&format!("/params/{key}")))
                .cloned()
            {
                params[key] = value;
            }
        }
        let result = client.request("thread/list", params, timeout_ms(body))?;
        let threads = result
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(normalize_thread_list_item)
            .collect::<Vec<_>>();
        for thread in &threads {
            if let Some(cwd) = thread.get("cwd").and_then(Value::as_str) {
                if let Some(path) = normalize_project_path(cwd) {
                    let project = upsert_project(projects, &path, "threads", Map::new());
                    let count = project
                        .get("sessionCount")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        + 1;
                    project["sessionCount"] = Value::from(count);
                    if let Some(timestamp) = thread_timestamp(thread) {
                        project["lastActiveAt"] = timestamp;
                    }
                }
            }
        }
        cursor = result.get("nextCursor").cloned().unwrap_or(Value::Null);
        if cursor.is_null() || threads.is_empty() {
            return Ok(());
        }
    }
    Ok(())
}

fn merge_params(body: &Value, keys: &[&str]) -> Value {
    let mut map = body
        .get("params")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for key in keys {
        if let Some(value) = body.get(*key).cloned() {
            map.insert((*key).to_string(), value);
        }
    }
    Value::Object(map)
}

fn normalize_input(input: Value) -> Value {
    match input {
        Value::String(text) => json!([{"type": "text", "text": text}]),
        other => other,
    }
}

fn normalize_thread_list_item(item: Value) -> Value {
    let Some(thread) = item.get("thread").and_then(Value::as_object) else {
        return item;
    };
    let mut map = item.as_object().cloned().unwrap_or_default();
    for (key, value) in thread {
        map.insert(key.clone(), value.clone());
    }
    Value::Object(map)
}

fn string_field(body: &Value, key: &str) -> Option<String> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn timeout_ms(body: &Value) -> Option<u64> {
    body.get("timeoutMs").and_then(Value::as_u64)
}

fn string_param(body: &Value, key: &str) -> Option<String> {
    body.get(key)
        .or_else(|| body.pointer(&format!("/params/{key}")))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn bool_param(body: &Value, key: &str, default: bool) -> bool {
    body.get(key)
        .or_else(|| body.pointer(&format!("/params/{key}")))
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn usize_param(body: &Value, key: &str, default: usize) -> usize {
    body.get(key)
        .or_else(|| body.pointer(&format!("/params/{key}")))
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(default)
}

fn read_json_file(path: &Path) -> Value {
    fs::read(path)
        .ok()
        .and_then(|content| json_compat::from_slice(&content).ok())
        .unwrap_or_else(|| json!({}))
}

fn parse_codex_project_config(path: &Path) -> Vec<(String, String)> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut current: Option<String> = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(raw) = trimmed
            .strip_prefix("[projects.\"")
            .and_then(|value| value.strip_suffix("\"]"))
        {
            current = normalize_project_path(&raw.replace("\\\"", "\""));
            continue;
        }
        if let Some(project) = current.as_ref() {
            if let Some(value) = trimmed
                .strip_prefix("trust_level")
                .and_then(|value| value.split_once('='))
                .map(|(_, value)| value.trim().trim_matches('"').to_string())
            {
                result.push((project.clone(), value));
            }
        }
    }
    result
}

fn upsert_project<'a>(
    projects: &'a mut Map<String, Value>,
    path: &str,
    source: &str,
    fields: Map<String, Value>,
) -> &'a mut Value {
    let title = Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(path)
        .to_string();
    let project = projects.entry(path.to_string()).or_insert_with(|| {
        json!({
            "id": path,
            "path": path,
            "cwd": path,
            "title": title,
            "exists": Path::new(path).exists(),
            "pinned": false,
            "active": false,
            "trustLevel": null,
            "projectId": null,
            "projectName": null,
            "rootPaths": [path],
            "sessionCount": 0,
            "lastActiveAt": null,
            "gitBranch": null,
            "gitOriginUrl": null,
            "sources": [],
        })
    });
    if let Some(sources) = project.get_mut("sources").and_then(Value::as_array_mut) {
        if !sources.iter().any(|value| value.as_str() == Some(source)) {
            sources.push(Value::String(source.to_string()));
        }
    }
    if let Some(map) = project.as_object_mut() {
        for (key, value) in fields {
            map.insert(key, value);
        }
    }
    project
}

fn thread_timestamp(thread: &Value) -> Option<Value> {
    for key in [
        "recencyAt",
        "recency_at",
        "updatedAt",
        "updated_at",
        "createdAt",
        "created_at",
    ] {
        if let Some(value) = thread.get(key).cloned() {
            return Some(value);
        }
    }
    None
}

fn unique_strings(values: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for value in values {
        if !value.trim().is_empty() && !out.iter().any(|candidate| candidate == &value) {
            out.push(value);
        }
    }
    out
}

fn array_strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn normalize_project_path(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let expanded = if trimmed == "~" {
        home_dir()
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(expanded)
    };
    Some(absolute.display().to_string())
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

fn daemonize(options: &ServerOptions) -> Result<(), String> {
    fs::create_dir_all(connector_home()).map_err(|error| error.to_string())?;
    if let Ok(body) = connector_health(options) {
        if body.get("ok").and_then(Value::as_bool) == Some(true) {
            let pid = body
                .pointer("/status/connector/pid")
                .cloned()
                .unwrap_or(Value::Null);
            if let Some(pid) = pid.as_u64() {
                fs::write(pid_path(), format!("{pid}\n")).map_err(|error| error.to_string())?;
            }
            println!(
                "{}",
                json!({"ok": true, "pid": pid, "existing": true, "url": format!("http://{}:{}", options.host, options.port), "logPath": log_path()})
            );
            return Ok(());
        }
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
        .map_err(|error| error.to_string())?;
    let log_err = log.try_clone().map_err(|error| error.to_string())?;
    let exe = env::current_exe().map_err(|error| error.to_string())?;
    let mut args = vec![
        "start".to_string(),
        "--host".to_string(),
        options.host.clone(),
        "--port".to_string(),
        options.port.to_string(),
        "--listen".to_string(),
        options.listen.clone(),
    ];
    if !options.extra_args.is_empty() {
        args.push("--codex-args".to_string());
        args.push(serde_json::to_string(&options.extra_args).map_err(|error| error.to_string())?);
    }
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    configure_detached_process(&mut command);
    let child = command.spawn().map_err(|error| error.to_string())?;
    let pid = child.id();
    let health = wait_for_connector_health(options, Some(pid))?;
    let real_pid = health
        .pointer("/status/connector/pid")
        .and_then(Value::as_u64)
        .unwrap_or(pid as u64);
    fs::write(pid_path(), format!("{real_pid}\n")).map_err(|error| error.to_string())?;
    println!(
        "{}",
        json!({"ok": true, "pid": real_pid, "url": format!("http://{}:{}", options.host, options.port), "logPath": log_path()})
    );
    Ok(())
}

fn configure_detached_process(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const DETACHED_PROCESS: u32 = 0x00000008;
        command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
}

fn connector_health(options: &ServerOptions) -> Result<Value, String> {
    let addresses = (options.host.as_str(), options.port)
        .to_socket_addrs()
        .map_err(|error| format!("failed to resolve connector health address: {error}"))?;
    let mut stream = None;
    let mut last_connect_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECTOR_HEALTH_IO_TIMEOUT) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(error) => last_connect_error = Some(error),
        }
    }
    let mut stream = stream.ok_or_else(|| {
        last_connect_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| {
                "connector health address resolved to no socket addresses".to_string()
            })
    })?;
    stream
        .set_read_timeout(Some(CONNECTOR_HEALTH_IO_TIMEOUT))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(CONNECTOR_HEALTH_IO_TIMEOUT))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .map_err(|error| error.to_string())?;
    let mut response = Vec::new();
    (&mut stream)
        .take(CONNECTOR_HEALTH_MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut response)
        .map_err(|error| error.to_string())?;
    if response.len() as u64 > CONNECTOR_HEALTH_MAX_RESPONSE_BYTES {
        return Err(format!(
            "connector health response exceeds {} bytes",
            CONNECTOR_HEALTH_MAX_RESPONSE_BYTES
        ));
    }
    let text = String::from_utf8_lossy(&response);
    if !(text.starts_with("HTTP/1.1 200") || text.starts_with("HTTP/1.0 200")) {
        return Err(text.to_string());
    }
    let body = text.split("\r\n\r\n").nth(1).unwrap_or_default();
    serde_json::from_str(body).map_err(|error| error.to_string())
}

fn verified_connector_pid(health: &Value) -> Result<u32, String> {
    if health
        .pointer("/status/connector/name")
        .and_then(Value::as_str)
        != Some("@baijimu/connector-codex")
    {
        return Err("health endpoint does not belong to the Codex connector".to_string());
    }
    health
        .pointer("/status/connector/pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| "connector health response does not contain a valid pid".to_string())
}

fn wait_for_connector_health(
    options: &ServerOptions,
    expected_pid: Option<u32>,
) -> Result<Value, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = "not started".to_string();
    while Instant::now() < deadline {
        match connector_health(options) {
            Ok(body) => {
                let pid_matches = expected_pid.is_none_or(|pid| {
                    body.pointer("/status/connector/pid")
                        .and_then(Value::as_u64)
                        == Some(pid as u64)
                });
                if body.get("ok").and_then(Value::as_bool) == Some(true) && pid_matches {
                    return Ok(body);
                }
            }
            Err(error) => last = error,
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(last)
}

fn connector_home() -> PathBuf {
    env::var_os("BAIJIMU_CONNECTOR_DATA_DIR")
        .or_else(|| env::var_os("CODEX_CONNECTOR_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".baijimu-connector-codex"))
}

fn management_token_path() -> PathBuf {
    connector_home().join(MANAGEMENT_TOKEN_FILE)
}

fn load_or_create_management_token() -> Result<String, String> {
    let home = connector_home();
    fs::create_dir_all(&home).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    let path = management_token_path();
    if let Ok(token) = fs::read_to_string(&path) {
        let token = token.trim();
        if token.len() >= 32 {
            return Ok(token.to_string());
        }
    }
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, format!("{token}\n")).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(&path).map_err(|error| error.to_string())?;
    }
    fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    let persisted = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    if persisted.trim() != token {
        return Err("management token read-back did not match the generated value".to_string());
    }
    Ok(token)
}

fn pid_path() -> PathBuf {
    connector_home().join("connector.pid")
}

fn log_path() -> PathBuf {
    connector_home().join("connector.log")
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}

fn random_event_id() -> String {
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let suffix = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("codex_evt_{suffix}")
}

fn to_camel_case(value: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for ch in value.chars() {
        if ch == '-' {
            upper = true;
            continue;
        }
        if upper {
            out.extend(ch.to_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn terminate_process(pid: u32) -> Result<(), String> {
    let pid = pid.to_string();
    let status = if cfg!(target_os = "windows") {
        Command::new("taskkill")
            .args(["/PID", &pid, "/T", "/F"])
            .status()
    } else {
        Command::new("kill").args(["-TERM", &pid]).status()
    }
    .map_err(|error| format!("failed to stop connector process {pid}: {error}"))?;
    if !status.success() {
        return Err(format!(
            "failed to stop connector process {pid}: command exited with {status}"
        ));
    }
    Ok(())
}

fn to_kebab_case(value: &str) -> String {
    let mut out = String::new();
    for character in value.chars() {
        if character.is_ascii_uppercase() {
            out.push('-');
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

fn print_help() {
    println!(
        "baijimu-connector-codex {VERSION}\n\nUsage:\n  baijimu-connector-codex start [--host 127.0.0.1] [--port 18110] [--listen stdio://] [--daemon]\n  baijimu-connector-codex status\n  baijimu-connector-codex stop\n  baijimu-connector-codex credential-state\n  baijimu-connector-codex checkout-project --workspace-id <id> --project-id <id> [--branch <name>]\n  baijimu-connector-codex --version"
    );
}

#[cfg(test)]
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
                    "name": "@baijimu/connector-codex",
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
