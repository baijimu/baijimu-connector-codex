use crate::*;

pub(crate) fn run(args: Vec<String>) -> Result<(), String> {
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
        other => Err(format!("unknown command: {other}")),
    }
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
    Ok(ServerOptions {
        host: value("host")
            .map(str::to_string)
            .or_else(|| env::var("CODEX_DESKTOP_HOST").ok())
            .unwrap_or_else(|| DEFAULT_HOST.to_string()),
        port: value("port")
            .and_then(|value| value.parse().ok())
            .or_else(|| {
                env::var("CODEX_DESKTOP_PORT")
                    .ok()
                    .and_then(|value| value.parse().ok())
            })
            .unwrap_or(DEFAULT_PORT),
        daemon: parsed.flags.get("daemon").and_then(Value::as_bool) == Some(true),
    })
}
