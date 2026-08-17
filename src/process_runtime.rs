use crate::*;

pub(crate) fn daemonize(options: &ServerOptions) -> Result<(), String> {
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
    let args = vec![
        "start".to_string(),
        "--host".to_string(),
        options.host.clone(),
        "--port".to_string(),
        options.port.to_string(),
    ];
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

pub(crate) fn configure_detached_process(command: &mut Command) {
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

pub(crate) fn connector_health(options: &ServerOptions) -> Result<Value, String> {
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

pub(crate) fn verified_connector_pid(health: &Value) -> Result<u32, String> {
    if health
        .pointer("/status/connector/name")
        .and_then(Value::as_str)
        != Some("@baijimu/codex-desktop")
    {
        return Err("health endpoint does not belong to the Codex desktop manager".to_string());
    }
    health
        .pointer("/status/connector/pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| "connector health response does not contain a valid pid".to_string())
}

pub(crate) fn wait_for_connector_health(
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

pub(crate) fn connector_home() -> PathBuf {
    env::var_os("BAIJIMU_CONNECTOR_DATA_DIR")
        .or_else(|| env::var_os("CODEX_DESKTOP_HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".baijimu-connector-codex"))
}

pub(crate) fn management_token_path() -> PathBuf {
    connector_home().join(MANAGEMENT_TOKEN_FILE)
}

pub(crate) fn load_or_create_management_token() -> Result<String, String> {
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

pub(crate) fn pid_path() -> PathBuf {
    connector_home().join("connector.pid")
}

pub(crate) fn log_path() -> PathBuf {
    connector_home().join("connector.log")
}

pub(crate) fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}

pub(crate) fn to_camel_case(value: &str) -> String {
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

pub(crate) fn terminate_process(pid: u32) -> Result<(), String> {
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
