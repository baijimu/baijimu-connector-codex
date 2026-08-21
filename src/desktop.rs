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
pub fn launch() -> Result<()> {
    platform::launch()
}

impl DesktopSwitch {
    pub fn restart_if_needed(&self) -> Result<bool> {
        platform::restart_if_needed(self)
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
        current_version: String,
        minimum_version: String,
        activation_accepted: bool,
    }

    const POWERSHELL_PREAMBLE: &str = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$OutputEncoding = [Console]::OutputEncoding
$codexDesktopProtocol = $env:CODEX_DESKTOP_PROTOCOL
if ([string]::IsNullOrWhiteSpace($codexDesktopProtocol)) { throw '缺少 Windows 桌面协议配置' }
$codexDesktopTrustedPublishers = @($env:CODEX_DESKTOP_TRUSTED_PUBLISHERS -split '\r?\n' | ForEach-Object { $_.Trim() } | Where-Object { $_ })
if ($codexDesktopTrustedPublishers.Count -eq 0) { throw 'Windows 桌面可信 Publisher 配置为空' }

function Get-CodexDesktopEntries {
  $packages = @(Get-AppxPackage -ErrorAction SilentlyContinue | Where-Object { $_.InstallLocation })
  $entries = @($packages | ForEach-Object {
    $package = $_
    try {
      $manifestPath = Join-Path $package.InstallLocation 'AppxManifest.xml'
      if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) { return }
      [xml]$manifest = Get-Content -Raw -LiteralPath $manifestPath
      $identity = @($manifest.SelectNodes("/*[local-name()='Package']/*[local-name()='Identity']") | Select-Object -First 1)
      if ($identity.Count -eq 0 -or $codexDesktopTrustedPublishers -notcontains ([string]$identity[0].Publisher)) { return }
      $applications = @($manifest.SelectNodes("/*[local-name()='Package']/*[local-name()='Applications']/*[local-name()='Application']") | Where-Object {
        if (-not $_.Executable) { return $false }
        $entryPoint = [string]$_.EntryPoint
        $fullTrust = [string]::IsNullOrWhiteSpace($entryPoint) -or $entryPoint -eq 'Windows.FullTrustApplication'
        $protocol = @($_.SelectNodes(".//*[local-name()='Protocol']") | Where-Object { ([string]$_.Name) -eq $codexDesktopProtocol }).Count -gt 0
        return $fullTrust -and $protocol
      })
      @($applications | Select-Object -First 1) | ForEach-Object {
        $relativeExecutable = [string]$_.Executable
        if ([System.IO.Path]::IsPathRooted($relativeExecutable)) { return }
        $packageRoot = [System.IO.Path]::GetFullPath($package.InstallLocation).TrimEnd('\') + '\'
        $executable = [System.IO.Path]::GetFullPath((Join-Path $packageRoot $relativeExecutable))
        if (-not $executable.StartsWith($packageRoot, [System.StringComparison]::OrdinalIgnoreCase)) { return }
        if ([System.IO.Path]::GetExtension($executable) -ne '.exe' -or -not (Test-Path -LiteralPath $executable -PathType Leaf)) { return }
        [pscustomobject]@{ package = $package; packageRoot = $packageRoot; executable = $executable }
      }
    } catch { return }
  })
  @($entries | Sort-Object @{ Expression = { [string]$_.package.PackageFullName } })
}

function New-CodexDesktopPackageNotFoundMessage {
  $visiblePackageCount = @(Get-AppxPackage -ErrorAction SilentlyContinue).Count
  return "当前 Windows 账户未发现可信 Publisher 签名、声明 $codexDesktopProtocol 协议且具有 FullTrust 可执行入口的桌面应用包（可见 AppX 包：$visiblePackageCount）。请确认百积木与 ChatGPT/Codex 在同一 Windows 账户下运行"
}
"#;

    const STOP_SCRIPT: &str = r#"
$entries = @(Get-CodexDesktopEntries)
$roots = @($entries | ForEach-Object { $_.packageRoot } | Where-Object { $_ })
$targets = @(Get-Process -ErrorAction SilentlyContinue | Where-Object {
  try { $path = $_.Path; $path -and ($roots | Where-Object { $path.StartsWith($_, [System.StringComparison]::OrdinalIgnoreCase) }).Count -gt 0 } catch { $false }
})
$wasRunning = $targets.Count -gt 0
if ($wasRunning) {
  $targets | Stop-Process -Force -ErrorAction Stop
  $deadline = (Get-Date).AddSeconds(15)
  do {
    $remaining = @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $targets.Id -contains $_.Id })
    if ($remaining.Count -gt 0) { Start-Sleep -Milliseconds 250 }
  } while ($remaining.Count -gt 0 -and (Get-Date) -lt $deadline)
  if ($remaining.Count -gt 0) { throw 'ChatGPT/Codex 桌面应用进程未在 15 秒内停止' }
}
[pscustomobject]@{ wasRunning = $wasRunning } | ConvertTo-Json -Compress
"#;

    const LAUNCH_SCRIPT: &str = r#"
$entry = @(Get-CodexDesktopEntries | Select-Object -First 1)
if ($entry.Count -eq 0) { throw (New-CodexDesktopPackageNotFoundMessage) }
$manifestPath = Join-Path $entry[0].package.InstallLocation 'AppxManifest.xml'
[xml]$manifest = Get-Content -Raw -LiteralPath $manifestPath
$minimumVersions = @($manifest.SelectNodes("/*[local-name()='Package']/*[local-name()='Dependencies']/*[local-name()='TargetDeviceFamily']") | ForEach-Object { [string]$_.MinVersion } | Where-Object { $_ })
if ($minimumVersions.Count -eq 0) { throw 'ChatGPT/Codex 应用包未声明最低 Windows 版本' }
$minimum = @($minimumVersions | ForEach-Object { [version]$_ } | Sort-Object -Descending | Select-Object -First 1)[0]
$current = [System.Environment]::OSVersion.Version
if ($current -lt $minimum) {
  [pscustomobject]@{ currentVersion = $current.ToString(); minimumVersion = $minimum.ToString(); activationAccepted = $false } | ConvertTo-Json -Compress
  return
}
$process = Start-Process -FilePath $entry[0].executable -PassThru
[pscustomobject]@{
  currentVersion = $current.ToString()
  minimumVersion = $minimum.ToString()
  activationAccepted = $true
  packageFullName = [string]$entry[0].package.PackageFullName
  processId = $process.Id
  executable = $entry[0].executable
} | ConvertTo-Json -Compress
"#;

    pub fn stop_for_workspace_switch() -> Result<DesktopSwitch> {
        let result: StopResult = crate::json_compat::from_slice(&run_powershell(STOP_SCRIPT)?)
            .context("解析 ChatGPT/Codex 桌面停止结果失败")?;
        Ok(DesktopSwitch {
            was_running: result.was_running,
        })
    }

    pub fn launch() -> Result<()> {
        let result: LaunchResult = crate::json_compat::from_slice(&run_powershell(LAUNCH_SCRIPT)?)
            .context("解析 ChatGPT/Codex Windows 启动结果失败")?;
        crate::system_compatibility::ensure_supported(
            "Windows",
            &result.current_version,
            &result.minimum_version,
            "ChatGPT/Codex",
        )?;
        anyhow::ensure!(
            result.activation_accepted,
            "Windows 未接受 ChatGPT/Codex 桌面应用启动请求"
        );
        Ok(())
    }

    pub fn restart_if_needed(state: &DesktopSwitch) -> Result<bool> {
        if !state.was_running {
            return Ok(false);
        }
        launch()?;
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
                "CODEX_DESKTOP_TRUSTED_PUBLISHERS",
                product.windows_desktop_trusted_publishers.join("\n"),
            )
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                &complete,
            ]);
        let output = command
            .output()
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
        #[test]
        fn launch_has_no_environment_write_or_broadcast() {
            let source = format!("{POWERSHELL_PREAMBLE}\n{LAUNCH_SCRIPT}");
            assert!(source.contains("Start-Process"));
            for forbidden in [
                concat!("CODEX", "_HOME"),
                concat!("CurrentUser", ".CreateSubKey"),
                concat!("SendMessage", "Timeout"),
                concat!("WM_", "SETTINGCHANGE"),
                concat!("Activate", "Application"),
            ] {
                assert!(!source.contains(forbidden), "unexpected {forbidden}");
            }
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

    pub fn restart_if_needed(state: &DesktopSwitch) -> Result<bool> {
        if !state.was_running {
            return Ok(false);
        }
        launch()?;
        Ok(true)
    }

    pub fn verify_system_compatibility() -> Result<()> {
        let path =
            installed_application_path().context("没有找到已安装的 ChatGPT/Codex 桌面应用")?;
        let minimum = plist_value(&path, "LSMinimumSystemVersion")?;
        let current = crate::system_compatibility::current_macos_version()?;
        crate::system_compatibility::ensure_supported("macOS", &current, &minimum, "ChatGPT/Codex")
    }

    pub fn launch() -> Result<()> {
        let path =
            installed_application_path().context("没有找到已安装的 ChatGPT/Codex 桌面应用")?;
        verify_system_compatibility()?;
        let mut command = Command::new("/usr/bin/open");
        crate::child_process::isolate_from_connector_environment(&mut command);
        command.arg(&path);
        run_checked(&mut command, "打开 ChatGPT/Codex 桌面应用失败")
    }

    fn installed_application_path() -> Option<PathBuf> {
        APPLICATION_PATHS
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_dir())
    }

    fn plist_value(path: &Path, key: &str) -> Result<String> {
        let output = Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", &format!("Print :{key}")])
            .arg(path.join("Contents/Info.plist"))
            .output()?;
        if !output.status.success() {
            anyhow::bail!("读取桌面应用 {key} 失败：{}", command_error(&output));
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        anyhow::ensure!(!value.is_empty(), "桌面应用 {key} 为空");
        Ok(value)
    }

    fn is_running(bundle_id: &str) -> Result<bool> {
        let output = Command::new("/usr/bin/lsappinfo")
            .args(["info", "-only", "pid", bundle_id])
            .output()?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.trim().starts_with("\"pid\"=") && !line.contains("[ NULL ]")))
    }

    fn run_checked(command: &mut Command, context: &str) -> Result<()> {
        let output = command.output().with_context(|| context.to_string())?;
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
        fn launch_does_not_override_codex_home() {
            let mut command = Command::new("/usr/bin/open");
            crate::child_process::isolate_from_connector_environment(&mut command);
            command.arg("/Applications/Codex.app");
            let args = command
                .get_args()
                .map(|v| v.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            assert_eq!(args, vec!["/Applications/Codex.app"]);
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;
    pub fn stop_for_workspace_switch() -> Result<DesktopSwitch> {
        Ok(DesktopSwitch::default())
    }
    pub fn restart_if_needed(_state: &DesktopSwitch) -> Result<bool> {
        Ok(false)
    }
}
