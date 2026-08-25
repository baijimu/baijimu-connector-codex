use std::ffi::OsStr;
use std::process::Command;

pub fn isolate_from_connector_environment(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if is_connector_private_environment(&name) {
            command.env_remove(name);
        }
    }
}

fn is_connector_private_environment(name: &OsStr) -> bool {
    let name = name.to_string_lossy().to_ascii_uppercase();
    name == "CODEX_HOME" || name.starts_with("BAIJIMU_") || name.starts_with("CODEX_CONNECTOR_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_connector_private_environment_without_hiding_normal_user_environment() {
        assert!(is_connector_private_environment(OsStr::new("CODEX_HOME")));
        assert!(is_connector_private_environment(OsStr::new(
            "BAIJIMU_LOCAL_APP_EVENT_TOKEN_FILE"
        )));
        assert!(is_connector_private_environment(OsStr::new(
            "CODEX_CONNECTOR_PORT"
        )));
        assert!(!is_connector_private_environment(OsStr::new("PATH")));
        assert!(!is_connector_private_environment(OsStr::new("HTTPS_PROXY")));
        assert!(!is_connector_private_environment(OsStr::new(
            "OPENAI_API_KEY"
        )));
    }
}
