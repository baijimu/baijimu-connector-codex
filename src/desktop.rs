use anyhow::Result;

#[derive(Clone, Debug, Default)]
pub struct DesktopSwitch {
    #[cfg(any(windows, target_os = "macos"))]
    was_running: bool,
}

pub fn stop_for_workspace_switch() -> Result<DesktopSwitch> {
    platform::stop_for_workspace_switch()
}

#[cfg(target_os = "macos")]
pub fn verify_system_compatibility() -> Result<()> {
    platform::verify_system_compatibility()
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn launch_workspace(codex_home: &std::path::Path) -> Result<()> {
    platform::launch_workspace(codex_home)
}

impl DesktopSwitch {
    pub fn restart_workspace_if_needed(&self, codex_home: &std::path::Path) -> Result<bool> {
        platform::restart_workspace_if_needed(self, codex_home)
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use anyhow::Context;
    use serde::Deserialize;
    use std::process::Command;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct StopResult {
        was_running: bool,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LaunchResult {
        activation_accepted: bool,
    }

    const POWERSHELL_PREAMBLE: &str = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$OutputEncoding = [Console]::OutputEncoding
$codexDesktopProtocol = $env:CODEX_DESKTOP_PROTOCOL
if ([string]::IsNullOrWhiteSpace($codexDesktopProtocol)) { throw '缺少 Windows 桌面协议配置' }
$codexDesktopProcessNames = @($env:CODEX_DESKTOP_PROCESS_NAMES -split '\r?\n' | ForEach-Object { $_.Trim() } | Where-Object { $_ })
if ($codexDesktopProcessNames.Count -eq 0) { throw 'Windows 桌面进程名配置为空' }
$codexDesktopTrustedSignerSubjects = @($env:CODEX_DESKTOP_TRUSTED_SIGNER_SUBJECTS -split '\r?\n' | ForEach-Object { $_.Trim() } | Where-Object { $_ })
if ($codexDesktopTrustedSignerSubjects.Count -eq 0) { throw 'Windows 桌面可信签名主体配置为空' }

function Get-CodexDesktopProcesses {
  $candidates = @($codexDesktopProcessNames | ForEach-Object {
    Get-Process -Name $_ -ErrorAction SilentlyContinue
  } | Sort-Object Id -Unique)
  $trustedPaths = @{}
  foreach ($candidate in $candidates) {
    $selected = $false
    try {
      # Keep the original process handle: a PID lookup is not proof of liveness.
      [void]$candidate.Handle
      if ($candidate.HasExited) { continue }
      $path = $candidate.Path
      if ([string]::IsNullOrWhiteSpace($path) -or [System.IO.Path]::GetExtension($path) -ne '.exe') { continue }
      if (-not $trustedPaths.ContainsKey($path)) {
        $signature = Get-AuthenticodeSignature -LiteralPath $path
        $trustedPaths[$path] = (
          $signature.Status -eq [System.Management.Automation.SignatureStatus]::Valid -and
          $signature.SignerCertificate -and
          $codexDesktopTrustedSignerSubjects -contains ([string]$signature.SignerCertificate.Subject)
        )
      }
      if ($trustedPaths[$path] -and -not $candidate.HasExited) {
        $selected = $true
        $candidate
      }
    } catch {
      # Ignore a process that exited during discovery, not access/inspection
      # failures for a live candidate: those must not look like "not running".
      if (-not $candidate.HasExited) { throw }
    }
    finally {
      if (-not $selected) { $candidate.Dispose() }
    }
  }
}
"#;

    const STOP_SCRIPT: &str = r#"
$targets = @(Get-CodexDesktopProcesses)
$wasRunning = $false
$stopClock = [System.Diagnostics.Stopwatch]::StartNew()
try {
  foreach ($target in $targets) {
    if ($target.HasExited) { continue }
    $wasRunning = $true
    try { $target.Kill() }
    catch {
      # Exiting between the liveness check and Kill is already success.
      if (-not $target.HasExited) { throw }
    }
  }
  foreach ($target in $targets) {
    if ($target.HasExited) { continue }
    $remainingMs = [Math]::Max(0, 15000 - $stopClock.ElapsedMilliseconds)
    if (-not $target.WaitForExit([int]$remainingMs)) {
      throw "ChatGPT/Codex 桌面应用进程未在 15 秒内停止 (PID $($target.Id))"
    }
  }
} finally {
  foreach ($target in $targets) { $target.Dispose() }
  $stopClock.Stop()
}
[pscustomobject]@{ wasRunning = $wasRunning } | ConvertTo-Json -Compress
"#;

    const LAUNCH_SCRIPT: &str = r#"
Start-Process -FilePath "${codexDesktopProtocol}:"
$deadline = (Get-Date).AddSeconds(10)
do {
  $targets = @(Get-CodexDesktopProcesses)
  $processCount = $targets.Count
  foreach ($target in $targets) { $target.Dispose() }
  if ($processCount -eq 0) { Start-Sleep -Milliseconds 100 }
} while ($processCount -eq 0 -and (Get-Date) -lt $deadline)
if ($processCount -eq 0) { throw 'Windows 已接受 codex: 协议请求，但可信 ChatGPT/Codex 桌面进程未在 10 秒内出现' }
[pscustomobject]@{
  activationAccepted = $true
  processCount = $processCount
} | ConvertTo-Json -Compress
"#;

    pub fn stop_for_workspace_switch() -> Result<DesktopSwitch> {
        let result: StopResult = crate::json_compat::from_slice(&run_powershell(STOP_SCRIPT)?)
            .context("解析 ChatGPT/Codex 桌面停止结果失败")?;
        Ok(DesktopSwitch {
            was_running: result.was_running,
        })
    }

    pub fn launch_workspace(codex_home: &std::path::Path) -> Result<()> {
        match crate::user_environment::read_codex_home()? {
            Some(projected_home) => anyhow::ensure!(
                crate::user_environment::codex_homes_match(&projected_home, codex_home),
                "用户级 CODEX_HOME 尚未切换到活动工作区：当前 {}，目标 {}",
                projected_home.display(),
                codex_home.display()
            ),
            None => anyhow::ensure!(
                crate::user_environment::codex_homes_match(
                    &crate::credential::default_codex_home(),
                    codex_home,
                ),
                "用户级 CODEX_HOME 尚未切换到活动工作区，请先打开该工作区"
            ),
        }
        let result: LaunchResult = crate::json_compat::from_slice(&run_powershell(LAUNCH_SCRIPT)?)
            .context("解析 ChatGPT/Codex Windows 启动结果失败")?;
        anyhow::ensure!(
            result.activation_accepted,
            "Windows 未接受 ChatGPT/Codex 桌面应用启动请求"
        );
        Ok(())
    }

    pub fn restart_workspace_if_needed(
        state: &DesktopSwitch,
        codex_home: &std::path::Path,
    ) -> Result<bool> {
        if !state.was_running {
            return Ok(false);
        }
        launch_workspace(codex_home)?;
        Ok(true)
    }

    fn run_powershell(script: &str) -> Result<Vec<u8>> {
        let mut command = Command::new("powershell.exe");
        crate::child_process::isolate_from_connector_environment(&mut command);
        let product = crate::product_config::get();
        let complete = format!("{POWERSHELL_PREAMBLE}\n{script}");
        command
            .env("CODEX_DESKTOP_PROTOCOL", &product.windows_desktop_protocol)
            .env(
                "CODEX_DESKTOP_PROCESS_NAMES",
                product.windows_desktop_process_names.join("\n"),
            )
            .env(
                "CODEX_DESKTOP_TRUSTED_SIGNER_SUBJECTS",
                product.windows_desktop_trusted_signer_subjects.join("\n"),
            );
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &complete,
        ]);
        let output = crate::child_process::output(&mut command, std::time::Duration::from_secs(25))
            .context("启动 PowerShell 管理 ChatGPT/Codex 桌面进程失败")?;
        if !output.status.success() {
            anyhow::bail!(
                "管理 ChatGPT/Codex 桌面进程失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(output.stdout)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const STOP_FIXTURE: &str = r#"
$fixture = Start-Process -FilePath (Join-Path $PSHOME 'powershell.exe') -ArgumentList @('-NoProfile', '-NonInteractive', '-Command', 'Start-Sleep -Seconds 60') -PassThru -WindowStyle Hidden
[void]$fixture.Handle
$retained = [System.Diagnostics.Process]::GetProcessById($fixture.Id)
[void]$retained.Handle
function Get-CodexDesktopProcesses { @($fixture) }
# Reproduce the device's stale PID enumeration deterministically while retaining
# a real native process handle. Kernel enumeration after exit varies by host.
function Get-Process {
  param([int[]]$Id)
  if ($Id -contains $retained.Id) { $retained }
}
"#;

        fn verify_stop_with_retained_handle(already_exited: bool) {
            let before_stop = if already_exited {
                r#"
$fixture.Kill()
if (-not $fixture.WaitForExit(5000)) { throw 'Fixture did not exit' }
$enumerated = @(Get-Process -Id $fixture.Id -ErrorAction SilentlyContinue)
if ($enumerated.Count -ne 1 -or -not $enumerated[0].HasExited) {
  throw 'Fixture must reproduce an enumerated but exited process'
}
"#
            } else {
                ""
            };
            let script = format!(
                r#"{STOP_FIXTURE}
try {{
  {before_stop}
  {STOP_SCRIPT}
  if (-not $retained.HasExited) {{ throw 'Original process is still running' }}
}} finally {{
  if (-not $retained.HasExited) {{ $retained.Kill(); [void]$retained.WaitForExit(5000) }}
  $retained.Dispose()
  $fixture.Dispose()
}}
"#
            );
            let result: StopResult =
                crate::json_compat::from_slice(&run_powershell(&script).unwrap()).unwrap();
            assert_eq!(result.was_running, !already_exited);
        }

        #[test]
        fn stopping_a_process_does_not_wait_for_its_retained_pid_record() {
            verify_stop_with_retained_handle(false);
        }

        #[test]
        fn an_already_exited_process_is_not_reported_as_running() {
            verify_stop_with_retained_handle(true);
        }

        #[test]
        fn launch_only_activates_the_preselected_windows_workspace() {
            let source = format!("{POWERSHELL_PREAMBLE}\n{STOP_SCRIPT}\n{LAUNCH_SCRIPT}");
            assert!(source.contains("Start-Process -FilePath \"${codexDesktopProtocol}:\""));
            assert!(source.contains("Get-Process -Name"));
            assert!(source.contains("Get-AuthenticodeSignature"));
            assert!(source.contains("SignatureStatus]::Valid"));
            assert!(!source.contains("Get-AppxPackage"));
            assert!(!source.contains("AppxManifest.xml"));
            assert!(!source.contains("Get-Process -ErrorAction"));
            for forbidden in [
                concat!("CurrentUser", ".CreateSubKey"),
                concat!("SendMessage", "Timeout"),
                concat!("WM_", "SETTINGCHANGE"),
                concat!("Activate", "Application"),
            ] {
                assert!(!source.contains(forbidden), "unexpected {forbidden}");
            }
            assert!(!source.contains("CODEX_HOME"));
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use anyhow::Context;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::thread;
    use std::time::{Duration, Instant};

    const APPLICATION_PATHS: [&str; 2] = ["/Applications/ChatGPT.app", "/Applications/Codex.app"];

    pub fn stop_for_workspace_switch() -> Result<DesktopSwitch> {
        let Some(path) = installed_application_path() else {
            return Ok(DesktopSwitch::default());
        };
        let bundle_id = plist_value(&path, "CFBundleIdentifier")?;
        if !is_running(&bundle_id)? {
            return Ok(DesktopSwitch::default());
        }
        let script = format!("tell application id \"{bundle_id}\" to quit");
        let mut command = Command::new("/usr/bin/osascript");
        command.args(["-e", &script]);
        run_checked(&mut command, "退出 ChatGPT/Codex 桌面应用失败")?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if !is_running(&bundle_id)? {
                return Ok(DesktopSwitch { was_running: true });
            }
            thread::sleep(Duration::from_millis(500));
        }
        anyhow::bail!("ChatGPT/Codex 桌面应用未在 15 秒内退出")
    }

    pub fn restart_workspace_if_needed(state: &DesktopSwitch, codex_home: &Path) -> Result<bool> {
        if !state.was_running {
            return Ok(false);
        }
        launch_workspace(codex_home)?;
        Ok(true)
    }

    pub fn verify_system_compatibility() -> Result<()> {
        let path =
            installed_application_path().context("没有找到已安装的 ChatGPT/Codex 桌面应用")?;
        let minimum = plist_value(&path, "LSMinimumSystemVersion")?;
        let current = crate::system_compatibility::current_macos_version()?;
        crate::system_compatibility::ensure_supported("macOS", &current, &minimum, "ChatGPT/Codex")
    }

    pub fn launch_workspace(codex_home: &Path) -> Result<()> {
        let path =
            installed_application_path().context("没有找到已安装的 ChatGPT/Codex 桌面应用")?;
        verify_system_compatibility()?;
        let mut command = open_application_command(&path, codex_home);
        run_checked(&mut command, "打开 ChatGPT/Codex 桌面应用失败")
    }

    fn open_application_command(app_path: &Path, codex_home: &Path) -> Command {
        let mut command = Command::new("/usr/bin/open");
        crate::child_process::isolate_from_connector_environment(&mut command);
        let mut assignment = std::ffi::OsString::from("CODEX_HOME=");
        assignment.push(codex_home);
        command.arg("--env").arg(assignment);
        command.arg(app_path);
        command
    }

    fn installed_application_path() -> Option<PathBuf> {
        APPLICATION_PATHS
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_dir())
    }

    fn plist_value(path: &Path, key: &str) -> Result<String> {
        let output = crate::child_process::output(
            Command::new("/usr/libexec/PlistBuddy")
                .args(["-c", &format!("Print :{key}")])
                .arg(path.join("Contents/Info.plist")),
            Duration::from_secs(5),
        )?;
        if !output.status.success() {
            anyhow::bail!("读取桌面应用 {key} 失败：{}", command_error(&output));
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        anyhow::ensure!(!value.is_empty(), "桌面应用 {key} 为空");
        Ok(value)
    }

    fn is_running(bundle_id: &str) -> Result<bool> {
        let output = crate::child_process::output(
            Command::new("/usr/bin/lsappinfo").args(["info", "-only", "pid", bundle_id]),
            Duration::from_secs(5),
        )?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim().starts_with("\"pid\"=") && !line.contains("[ NULL ]")))
    }

    fn run_checked(command: &mut Command, context: &str) -> Result<()> {
        let output = crate::child_process::output(command, Duration::from_secs(20))
            .with_context(|| context.to_string())?;
        if !output.status.success() {
            anyhow::bail!("{context}：{}", command_error(&output));
        }
        Ok(())
    }

    fn command_error(output: &Output) -> String {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            format!("exit={}", output.status)
        } else {
            stderr
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn workspace_launch_passes_only_the_selected_codex_home() {
            let command = open_application_command(
                Path::new("/Applications/Codex.app"),
                Path::new("/private/codex/workspace-a"),
            );
            let args = command
                .get_args()
                .map(|v| v.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert_eq!(
                args,
                vec![
                    "--env",
                    "CODEX_HOME=/private/codex/workspace-a",
                    "/Applications/Codex.app"
                ]
            );
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;
    pub fn stop_for_workspace_switch() -> Result<DesktopSwitch> {
        Ok(DesktopSwitch::default())
    }
    pub fn restart_workspace_if_needed(
        _state: &DesktopSwitch,
        _codex_home: &std::path::Path,
    ) -> Result<bool> {
        Ok(false)
    }
}
