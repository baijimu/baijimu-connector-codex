use anyhow::Result;
use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct DesktopSwitch {
    #[cfg(any(windows, target_os = "macos"))]
    was_running: bool,
}

pub fn stop_for_codex_home_switch() -> Result<DesktopSwitch> {
    platform::stop_for_codex_home_switch()
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn verify_system_compatibility() -> Result<()> {
    platform::verify_system_compatibility()
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn launch_and_verify(codex_home: &Path) -> Result<()> {
    platform::launch_and_verify(codex_home)
}

impl DesktopSwitch {
    pub fn restart_and_verify(&self, codex_home: &Path) -> Result<bool> {
        platform::restart_and_verify(self, codex_home)
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
    struct CompatibilityResult {
        current_version: String,
        minimum_version: String,
    }

    const STOP_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$packages = @('OpenAI.Codex', 'OpenAI.ChatGPT') | ForEach-Object { Get-AppxPackage -Name $_ -ErrorAction SilentlyContinue } | Where-Object { $_ }
if (-not $packages) {
  $packages = @(Get-AppxPackage -ErrorAction SilentlyContinue | Where-Object { $_.Name -like 'OpenAI.Codex*' -or ($_.Name -like 'OpenAI.ChatGPT*' -and $_.Name -notlike 'OpenAI.ChatGPT-Desktop*') })
}
$roots = @($packages | ForEach-Object { $_.InstallLocation } | Where-Object { $_ })
$targets = @(Get-Process -ErrorAction SilentlyContinue | Where-Object {
  try {
    $path = $_.Path
    if (-not $path) { return $false }
    return ($roots | Where-Object { $path.StartsWith($_, [System.StringComparison]::OrdinalIgnoreCase) }).Count -gt 0
  } catch { return $false }
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

    const LAUNCH_AND_VERIFY_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$codexHome = $env:CODEX_HOME
if (-not $codexHome) { throw '隔离启动桌面应用时必须显式提供 CODEX_HOME' }
$packages = @('OpenAI.Codex', 'OpenAI.ChatGPT') | ForEach-Object { Get-AppxPackage -Name $_ -ErrorAction SilentlyContinue } | Where-Object { $_ }
if (-not $packages) {
  $packages = @(Get-AppxPackage -ErrorAction SilentlyContinue | Where-Object { $_.Name -like 'OpenAI.Codex*' -or ($_.Name -like 'OpenAI.ChatGPT*' -and $_.Name -notlike 'OpenAI.ChatGPT-Desktop*') })
}
if (-not $packages) { throw '当前用户尚未安装 ChatGPT/Codex 桌面应用包' }

$startApps = @(Get-StartApps -ErrorAction SilentlyContinue)
$entry = @($packages | ForEach-Object {
  $package = $_
  if (-not $package.InstallLocation) { return }
  $manifestPath = Join-Path $package.InstallLocation 'AppxManifest.xml'
  if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) { return }
  [xml]$manifest = Get-Content -LiteralPath $manifestPath
  $startApp = @($startApps | Where-Object { $_.AppID -like "$($package.PackageFamilyName)!*" } | Select-Object -First 1)
  $applicationId = if ($startApp.Count -gt 0) {
    ([string]$startApp[0].AppID).Substring(([string]$startApp[0].AppID).LastIndexOf('!') + 1)
  } else {
    $null
  }
  $applications = @($manifest.Package.Applications.Application | Where-Object {
    if (-not $_.Executable) { return $false }
    $entryPoint = [string]$_.EntryPoint
    $isFullTrust = [string]::IsNullOrWhiteSpace($entryPoint) -or $entryPoint -eq 'Windows.FullTrustApplication'
    return $isFullTrust -and (-not $applicationId -or ([string]$_.Id) -eq $applicationId)
  })
  if ($applications.Count -eq 0 -and $applicationId) { return }
  @($applications | Select-Object -First 1) | ForEach-Object {
    $relativeExecutable = [string]$_.Executable
    if ([System.IO.Path]::IsPathRooted($relativeExecutable)) {
      throw "ChatGPT/Codex 应用清单包含绝对可执行文件路径：$relativeExecutable"
    }
    $packageRoot = [System.IO.Path]::GetFullPath($package.InstallLocation).TrimEnd('\') + '\'
    $executable = [System.IO.Path]::GetFullPath((Join-Path $packageRoot $relativeExecutable))
    if (-not $executable.StartsWith($packageRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
      throw "ChatGPT/Codex 应用清单入口超出包目录：$relativeExecutable"
    }
    if ([System.IO.Path]::GetExtension($executable) -ne '.exe' -or -not (Test-Path -LiteralPath $executable -PathType Leaf)) {
      throw "ChatGPT/Codex 应用清单入口不可用：$executable"
    }
    [pscustomobject]@{
      package = $package
      packageRoot = $packageRoot
      applicationId = [string]$_.Id
      appUserModelId = if ($startApp.Count -gt 0) { [string]$startApp[0].AppID } else { $null }
      executable = $executable
    }
  }
} | Select-Object -First 1)
if (-not $entry) { throw 'ChatGPT/Codex 桌面应用包中没有与开始菜单匹配的 FullTrust 可执行入口' }

$selectedRoot = $entry[0].packageRoot
$existing = @(Get-Process -ErrorAction SilentlyContinue | Where-Object {
  try {
    $path = $_.Path
    if (-not $path) { return $false }
    return $path.StartsWith($selectedRoot, [System.StringComparison]::OrdinalIgnoreCase)
  } catch { return $false }
})
if ($existing.Count -gt 0) {
  $existing | Stop-Process -Force -ErrorAction Stop
  $deadline = (Get-Date).AddSeconds(15)
  do {
    $remaining = @(Get-Process -ErrorAction SilentlyContinue | Where-Object { $existing.Id -contains $_.Id })
    if ($remaining.Count -gt 0) { Start-Sleep -Milliseconds 250 }
  } while ($remaining.Count -gt 0 -and (Get-Date) -lt $deadline)
  if ($remaining.Count -gt 0) { throw 'ChatGPT/Codex 桌面应用进程未在 15 秒内停止' }
}

if (-not ('BaijimuCodexVisibleWindowProbe' -as [type])) {
  Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class BaijimuCodexVisibleWindowProbe {
    private delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")]
    private static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);
    [DllImport("user32.dll")]
    private static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);

    public static uint[] ProcessIds() {
        var processIds = new HashSet<uint>();
        EnumWindows(delegate(IntPtr hWnd, IntPtr lParam) {
            if (IsWindowVisible(hWnd)) {
                uint processId;
                GetWindowThreadProcessId(hWnd, out processId);
                if (processId != 0) processIds.Add(processId);
            }
            return true;
        }, IntPtr.Zero);
        var result = new uint[processIds.Count];
        processIds.CopyTo(result);
        return result;
    }
}
'@
}

Start-Process -FilePath $entry[0].executable -WorkingDirectory ([System.IO.Path]::GetDirectoryName($entry[0].executable)) -ErrorAction Stop
$deadline = (Get-Date).AddSeconds(45)
do {
  $running = @(Get-Process -ErrorAction SilentlyContinue | Where-Object {
    try {
      $path = $_.Path
      if (-not $path) { return $false }
      return $path.StartsWith($selectedRoot, [System.StringComparison]::OrdinalIgnoreCase)
    } catch { return $false }
  })
  $runningIds = @($running | ForEach-Object { [uint32]$_.Id })
  $visibleIds = @([BaijimuCodexVisibleWindowProbe]::ProcessIds())
  $visible = @($runningIds | Where-Object { $visibleIds -contains $_ })
  if ($visible.Count -eq 0) { Start-Sleep -Milliseconds 500 }
} while ($visible.Count -eq 0 -and (Get-Date) -lt $deadline)
if ($running.Count -eq 0) { throw 'ChatGPT/Codex 桌面应用未在 45 秒内启动进程' }
if ($visible.Count -eq 0) { throw 'ChatGPT/Codex 桌面应用已启动进程，但未在 45 秒内显示可见窗口' }
[pscustomobject]@{
  running = $true
  visibleWindow = $true
  processCount = $running.Count
  visibleWindowCount = $visible.Count
  packageFullName = [string]$entry[0].package.PackageFullName
  applicationId = $entry[0].applicationId
  appUserModelId = $entry[0].appUserModelId
  executable = $entry[0].executable
  codexHome = $codexHome
} | ConvertTo-Json -Compress
"#;

    const COMPATIBILITY_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$packages = @('OpenAI.Codex', 'OpenAI.ChatGPT') | ForEach-Object { Get-AppxPackage -Name $_ -ErrorAction SilentlyContinue } | Where-Object { $_ }
if (-not $packages) {
  $packages = @(Get-AppxPackage -ErrorAction SilentlyContinue | Where-Object { $_.Name -like 'OpenAI.Codex*' -or ($_.Name -like 'OpenAI.ChatGPT*' -and $_.Name -notlike 'OpenAI.ChatGPT-Desktop*') })
}
$package = @($packages | Where-Object { $_.InstallLocation } | Select-Object -First 1)
if ($package.Count -eq 0) { throw '当前用户尚未安装 ChatGPT/Codex 桌面应用包' }
$manifestPath = Join-Path $package[0].InstallLocation 'AppxManifest.xml'
if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) { throw 'ChatGPT/Codex 应用包缺少 AppxManifest.xml' }
[xml]$manifest = Get-Content -LiteralPath $manifestPath
$minimumVersions = @($manifest.Package.Dependencies.TargetDeviceFamily | ForEach-Object { [string]$_.MinVersion } | Where-Object { $_ })
if ($minimumVersions.Count -eq 0) { throw 'ChatGPT/Codex 应用包未声明最低 Windows 版本' }
$minimum = @($minimumVersions | ForEach-Object { [version]$_ } | Sort-Object -Descending | Select-Object -First 1)[0]
[pscustomobject]@{
  currentVersion = [System.Environment]::OSVersion.Version.ToString()
  minimumVersion = $minimum.ToString()
} | ConvertTo-Json -Compress
"#;

    pub fn stop_for_codex_home_switch() -> Result<DesktopSwitch> {
        let output = run_powershell(STOP_SCRIPT, None)?;
        let result: StopResult = crate::json_compat::from_slice(&output)
            .context("解析 ChatGPT/Codex 桌面停止结果失败")?;
        Ok(DesktopSwitch {
            was_running: result.was_running,
        })
    }

    pub fn verify_system_compatibility() -> Result<()> {
        let output = run_powershell(COMPATIBILITY_SCRIPT, None)?;
        let compatibility: CompatibilityResult = crate::json_compat::from_slice(&output)
            .context("解析 ChatGPT/Codex Windows 系统兼容性失败")?;
        crate::system_compatibility::ensure_supported(
            "Windows",
            &compatibility.current_version,
            &compatibility.minimum_version,
            "ChatGPT/Codex",
        )
    }

    pub fn launch_and_verify(codex_home: &Path) -> Result<()> {
        verify_system_compatibility()?;
        run_powershell(LAUNCH_AND_VERIFY_SCRIPT, Some(codex_home))?;
        Ok(())
    }

    pub fn restart_and_verify(state: &DesktopSwitch, codex_home: &Path) -> Result<bool> {
        if !state.was_running {
            return Ok(false);
        }
        launch_and_verify(codex_home)?;
        Ok(true)
    }

    fn run_powershell(script: &str, codex_home: Option<&Path>) -> Result<Vec<u8>> {
        let mut command = Command::new("powershell.exe");
        crate::child_process::isolate_from_connector_environment(&mut command);
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            script,
        ]);
        if let Some(codex_home) = codex_home {
            command.env("CODEX_HOME", codex_home);
        }
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
        use std::io::Write;
        use std::process::Stdio;

        #[test]
        fn desktop_management_scripts_parse_in_windows_powershell() {
            for script in [STOP_SCRIPT, COMPATIBILITY_SCRIPT, LAUNCH_AND_VERIFY_SCRIPT] {
                let mut child = Command::new("powershell.exe")
                    .args([
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "[scriptblock]::Create([Console]::In.ReadToEnd()) | Out-Null",
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(script.as_bytes())
                    .unwrap();
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
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
    const LAUNCH_TIMEOUT: Duration = Duration::from_secs(45);
    const POLL_INTERVAL: Duration = Duration::from_millis(500);

    pub fn stop_for_codex_home_switch() -> Result<DesktopSwitch> {
        let Some(app_path) = installed_application_path() else {
            return Ok(DesktopSwitch::default());
        };
        let bundle_id = application_bundle_id(&app_path)?;
        let info = application_info(&bundle_id)?;
        if !has_running_process(&info) {
            return Ok(DesktopSwitch::default());
        }

        let script = format!("tell application id \"{bundle_id}\" to quit");
        run_checked(
            {
                let mut command = Command::new("/usr/bin/osascript");
                command.args(["-e", &script]);
                command
            },
            "退出 ChatGPT/Codex 桌面应用失败",
        )?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if !has_running_process(&application_info(&bundle_id)?) {
                return Ok(DesktopSwitch { was_running: true });
            }
            thread::sleep(POLL_INTERVAL);
        }
        anyhow::bail!("ChatGPT/Codex 桌面应用未在 15 秒内退出")
    }

    pub fn restart_and_verify(state: &DesktopSwitch, codex_home: &Path) -> Result<bool> {
        if !state.was_running {
            return Ok(false);
        }
        launch_and_verify(codex_home)?;
        Ok(true)
    }

    pub fn verify_system_compatibility() -> Result<()> {
        let app_path =
            installed_application_path().context("没有找到已安装的 ChatGPT/Codex 桌面应用")?;
        verify_application_compatibility(&app_path)
    }

    pub fn launch_and_verify(codex_home: &Path) -> Result<()> {
        let app_path =
            installed_application_path().context("没有找到已安装的 ChatGPT/Codex 桌面应用")?;
        verify_application_compatibility(&app_path)?;
        let bundle_id = application_bundle_id(&app_path)?;

        run_checked(
            open_application_command(&app_path, codex_home),
            "打开 ChatGPT/Codex 桌面应用失败",
        )?;

        let started = Instant::now();
        while started.elapsed() < LAUNCH_TIMEOUT {
            let info = application_info(&bundle_id)?;
            if has_running_process(&info) {
                verify_application_codex_home(&info, codex_home)?;
                return Ok(());
            }
            thread::sleep(POLL_INTERVAL);
        }

        anyhow::bail!("ChatGPT/Codex 桌面应用未在 45 秒内启动");
    }

    fn installed_application_path() -> Option<PathBuf> {
        APPLICATION_PATHS
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_dir())
    }

    fn application_bundle_id(app_path: &Path) -> Result<String> {
        let plist = app_path.join("Contents/Info.plist");
        let output = Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", "Print :CFBundleIdentifier"])
            .arg(&plist)
            .output()
            .with_context(|| format!("读取桌面应用标识失败: {}", plist.display()))?;
        if !output.status.success() {
            anyhow::bail!("读取桌面应用标识失败：{}", command_error(&output));
        }
        let bundle_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if bundle_id.is_empty() {
            anyhow::bail!("桌面应用标识为空: {}", plist.display());
        }
        Ok(bundle_id)
    }

    fn verify_application_compatibility(app_path: &Path) -> Result<()> {
        let minimum = application_plist_value(app_path, "LSMinimumSystemVersion")
            .context("ChatGPT/Codex 桌面应用未声明最低 macOS 版本")?;
        let current = crate::system_compatibility::current_macos_version()?;
        crate::system_compatibility::ensure_supported("macOS", &current, &minimum, "ChatGPT/Codex")
    }

    fn application_plist_value(app_path: &Path, key: &str) -> Result<String> {
        let plist = app_path.join("Contents/Info.plist");
        let output = Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", &format!("Print :{key}")])
            .arg(&plist)
            .output()
            .with_context(|| format!("读取桌面应用 {key} 失败: {}", plist.display()))?;
        if !output.status.success() {
            anyhow::bail!("读取桌面应用 {key} 失败：{}", command_error(&output));
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if value.is_empty() {
            anyhow::bail!("桌面应用 {key} 为空: {}", plist.display());
        }
        Ok(value)
    }

    fn application_info(bundle_id: &str) -> Result<String> {
        let output = Command::new("/usr/bin/lsappinfo")
            .args(["info", "-only", "pid", bundle_id])
            .output()
            .context("检查 ChatGPT/Codex 桌面进程失败")?;
        let mut info = String::from_utf8_lossy(&output.stdout).into_owned();
        info.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(info)
    }

    fn has_running_process(info: &str) -> bool {
        info.lines().any(|line| {
            let line = line.trim();
            line.starts_with("\"pid\"=") && !line.contains("[ NULL ]")
        })
    }

    fn application_pid(info: &str) -> Option<u32> {
        info.lines().find_map(|line| {
            let line = line.trim();
            let value = line.strip_prefix("\"pid\"=")?.trim();
            value.parse().ok()
        })
    }

    fn verify_application_codex_home(info: &str, codex_home: &Path) -> Result<()> {
        let pid = application_pid(info).context("无法读取 ChatGPT/Codex 桌面进程 PID")?;
        let output = Command::new("/bin/ps")
            .args(["eww", "-p", &pid.to_string()])
            .output()
            .context("读取 ChatGPT/Codex 桌面进程环境失败")?;
        if !output.status.success() {
            anyhow::bail!(
                "读取 ChatGPT/Codex 桌面进程环境失败：{}",
                command_error(&output)
            );
        }
        let process = String::from_utf8_lossy(&output.stdout);
        let expected = format!("CODEX_HOME={}", codex_home.to_string_lossy());
        if !process.contains(&expected) {
            anyhow::bail!("ChatGPT/Codex 已启动，但没有使用所选工作区状态目录");
        }
        Ok(())
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

    fn run_checked(mut command: Command, context: &str) -> Result<()> {
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
        fn parses_lsappinfo_process_state_without_requiring_a_window() {
            let hidden = "\"pid\"=682\n\"visible\"=[ NULL ]\n\"windows\"=[ NULL ]\n";
            assert!(has_running_process(hidden));
            assert_eq!(application_pid(hidden), Some(682));

            let missing = "Application not found\n";
            assert!(!has_running_process(missing));
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;

    pub fn stop_for_codex_home_switch() -> Result<DesktopSwitch> {
        Ok(DesktopSwitch::default())
    }

    pub fn verify_system_compatibility() -> Result<()> {
        anyhow::bail!("当前平台不支持 ChatGPT/Codex 桌面应用")
    }

    pub fn restart_and_verify(_state: &DesktopSwitch, _codex_home: &Path) -> Result<bool> {
        Ok(false)
    }
}
