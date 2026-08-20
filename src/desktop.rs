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

#[cfg(target_os = "macos")]
pub fn verify_system_compatibility() -> Result<()> {
    platform::verify_system_compatibility()
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn launch(codex_home: &Path) -> Result<()> {
    platform::stop_for_codex_home_switch()?;
    platform::launch(codex_home)
}

impl DesktopSwitch {
    pub fn restart_if_needed(&self, codex_home: &Path) -> Result<bool> {
        platform::restart_if_needed(self, codex_home)
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
  $startApps = @(Get-StartApps -ErrorAction SilentlyContinue)
  $packages = @(Get-AppxPackage -ErrorAction SilentlyContinue | Where-Object { $_.InstallLocation })
  $entries = @($packages | ForEach-Object {
    $package = $_
    try {
      $manifestPath = Join-Path $package.InstallLocation 'AppxManifest.xml'
      if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) { return }
      [xml]$manifest = Get-Content -Raw -LiteralPath $manifestPath
      $identity = @($manifest.SelectNodes("/*[local-name()='Package']/*[local-name()='Identity']") | Select-Object -First 1)
      if ($identity.Count -eq 0) { return }
      $publisher = [string]$identity[0].Publisher
      if ($codexDesktopTrustedPublishers -notcontains $publisher) { return }
      $startApp = @($startApps | Where-Object { $_.AppID -like "$($package.PackageFamilyName)!*" } | Select-Object -First 1)
      $applicationId = if ($startApp.Count -gt 0) {
        ([string]$startApp[0].AppID).Substring(([string]$startApp[0].AppID).LastIndexOf('!') + 1)
      } else {
        $null
      }
      $applications = @($manifest.SelectNodes("/*[local-name()='Package']/*[local-name()='Applications']/*[local-name()='Application']") | Where-Object {
        if (-not $_.Executable) { return $false }
        $entryPoint = [string]$_.EntryPoint
        $isFullTrust = [string]::IsNullOrWhiteSpace($entryPoint) -or $entryPoint -eq 'Windows.FullTrustApplication'
        $declaresCodexProtocol = @($_.SelectNodes(".//*[local-name()='Protocol']") | Where-Object { ([string]$_.Name) -eq $codexDesktopProtocol }).Count -gt 0
        return $isFullTrust -and $declaresCodexProtocol -and (-not $applicationId -or ([string]$_.Id) -eq $applicationId)
      })
      if ($applications.Count -eq 0 -and $applicationId) { return }
      @($applications | Select-Object -First 1) | ForEach-Object {
        $relativeExecutable = [string]$_.Executable
        if ([System.IO.Path]::IsPathRooted($relativeExecutable)) { return }
        $packageRoot = [System.IO.Path]::GetFullPath($package.InstallLocation).TrimEnd('\') + '\'
        $executable = [System.IO.Path]::GetFullPath((Join-Path $packageRoot $relativeExecutable))
        if (-not $executable.StartsWith($packageRoot, [System.StringComparison]::OrdinalIgnoreCase)) { return }
        if ([System.IO.Path]::GetExtension($executable) -ne '.exe' -or -not (Test-Path -LiteralPath $executable -PathType Leaf)) { return }
        [pscustomobject]@{
          package = $package
          packageRoot = $packageRoot
          applicationId = [string]$_.Id
          appUserModelId = if ($startApp.Count -gt 0) { [string]$startApp[0].AppID } else { $null }
          executable = $executable
        }
      }
    } catch { return }
  })
  @($entries | Sort-Object @{ Expression = { if ($_.appUserModelId) { 0 } else { 1 } } }, @{ Expression = { [string]$_.appUserModelId } }, @{ Expression = { [string]$_.package.PackageFullName } })
}

function New-CodexDesktopPackageNotFoundMessage {
  $visiblePackageCount = @(Get-AppxPackage -ErrorAction SilentlyContinue).Count
  $startAppCount = @(Get-StartApps -ErrorAction SilentlyContinue).Count
  return "当前 Windows 账户未发现可信 Publisher 签名、声明 $codexDesktopProtocol 协议且具有 FullTrust 可执行入口的桌面应用包（可见 AppX 包：$visiblePackageCount，开始菜单应用：$startAppCount）。请确认百积木与 ChatGPT/Codex 在同一 Windows 账户下运行，然后重试安装"
}

if (-not ('BaijimuCodexPackageActivator' -as [type])) {
  Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

[ComImport]
[Guid("2e941141-7f97-4756-ba1d-9decde894a3d")]
[InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IApplicationActivationManager {
    [PreserveSig]
    int ActivateApplication(
        [MarshalAs(UnmanagedType.LPWStr)] string appUserModelId,
        [MarshalAs(UnmanagedType.LPWStr)] string arguments,
        uint options,
        out uint processId);

    [PreserveSig]
    int ActivateForFile(
        [MarshalAs(UnmanagedType.LPWStr)] string appUserModelId,
        IntPtr itemArray,
        [MarshalAs(UnmanagedType.LPWStr)] string verb,
        out uint processId);

    [PreserveSig]
    int ActivateForProtocol(
        [MarshalAs(UnmanagedType.LPWStr)] string appUserModelId,
        IntPtr itemArray,
        out uint processId);
}

[ComImport]
[Guid("45BA127D-10A8-46EA-8AB7-56EA9078943C")]
class ApplicationActivationManager {}

public static class BaijimuCodexPackageActivator {
    [DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern IntPtr SendMessageTimeout(
        IntPtr window,
        uint message,
        IntPtr wParam,
        string lParam,
        uint flags,
        uint timeout,
        out IntPtr result);

    public static uint Activate(string appUserModelId) {
        var manager = (IApplicationActivationManager)new ApplicationActivationManager();
        uint processId;
        int result = manager.ActivateApplication(appUserModelId, null, 0, out processId);
        Marshal.ThrowExceptionForHR(result);
        return processId;
    }

    public static void BroadcastEnvironmentChange() {
        IntPtr result;
        IntPtr sent = SendMessageTimeout(
            new IntPtr(0xffff),
            0x001A,
            IntPtr.Zero,
            "Environment",
            0x0002,
            5000,
            out result);
        if (sent == IntPtr.Zero) {
            throw new Win32Exception(Marshal.GetLastWin32Error());
        }
    }
}
'@
}

function Invoke-CodexDesktopActivation {
  param(
    [Parameter(Mandatory = $true)][string]$AppUserModelId,
    [Parameter(Mandatory = $true)][string]$CodexHome
  )
  if ([string]::IsNullOrWhiteSpace($AppUserModelId)) {
    throw 'ChatGPT/Codex 桌面应用包未登记 AUMID，无法通过 Windows 应用激活器启动'
  }

  $environmentKey = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment', $true)
  if (-not $environmentKey) { throw '无法打开当前用户环境变量注册表' }
  $launchEnvironment = [ordered]@{ CODEX_HOME = $CodexHome }
  $originalEnvironment = @{}
  $knownNames = @($environmentKey.GetValueNames())
  foreach ($name in $launchEnvironment.Keys) {
    $hadOriginal = $knownNames -contains $name
    $originalEnvironment[$name] = [pscustomobject]@{
      hadOriginal = $hadOriginal
      value = if ($hadOriginal) {
        $environmentKey.GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
      } else { $null }
      kind = if ($hadOriginal) { $environmentKey.GetValueKind($name) } else { $null }
    }
  }
  $activatedProcessId = $null
  try {
    foreach ($name in $launchEnvironment.Keys) {
      $environmentKey.SetValue($name, $launchEnvironment[$name], [Microsoft.Win32.RegistryValueKind]::String)
    }
    [BaijimuCodexPackageActivator]::BroadcastEnvironmentChange()
    $activatedProcessId = [BaijimuCodexPackageActivator]::Activate($AppUserModelId)
  } finally {
    try {
      foreach ($name in $launchEnvironment.Keys) {
        $original = $originalEnvironment[$name]
        if ($original.hadOriginal) {
          $environmentKey.SetValue($name, $original.value, $original.kind)
        } else {
          $environmentKey.DeleteValue($name, $false)
        }
      }
      [BaijimuCodexPackageActivator]::BroadcastEnvironmentChange()
    } finally {
      $environmentKey.Dispose()
    }
  }
  return $activatedProcessId
}
"#;

    const STOP_SCRIPT: &str = r#"
$entries = @(Get-CodexDesktopEntries)
$roots = @($entries | ForEach-Object { $_.packageRoot } | Where-Object { $_ })
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

    const LAUNCH_SCRIPT: &str = r#"
$codexHome = $env:CODEX_HOME
if (-not $codexHome) { throw '隔离启动桌面应用时必须显式提供 CODEX_HOME' }
$entry = @(Get-CodexDesktopEntries | Select-Object -First 1)
if ($entry.Count -eq 0) { throw (New-CodexDesktopPackageNotFoundMessage) }
$package = $entry[0].package
$manifestPath = Join-Path $package.InstallLocation 'AppxManifest.xml'
if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) { throw 'ChatGPT/Codex 应用包缺少 AppxManifest.xml' }
[xml]$manifest = Get-Content -LiteralPath $manifestPath
$minimumVersions = @($manifest.SelectNodes("/*[local-name()='Package']/*[local-name()='Dependencies']/*[local-name()='TargetDeviceFamily']") | ForEach-Object { [string]$_.MinVersion } | Where-Object { $_ })
if ($minimumVersions.Count -eq 0) { throw 'ChatGPT/Codex 应用包未声明最低 Windows 版本' }
$minimum = @($minimumVersions | ForEach-Object { [version]$_ } | Sort-Object -Descending | Select-Object -First 1)[0]
$current = [System.Environment]::OSVersion.Version
if ($current -lt $minimum) {
  [pscustomobject]@{
    currentVersion = $current.ToString()
    minimumVersion = $minimum.ToString()
    activationAccepted = $false
  } | ConvertTo-Json -Compress
  return
}

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

$activatedProcessId = Invoke-CodexDesktopActivation -AppUserModelId $entry[0].appUserModelId -CodexHome $codexHome
[pscustomobject]@{
  currentVersion = $current.ToString()
  minimumVersion = $minimum.ToString()
  activationAccepted = $true
  packageFullName = [string]$entry[0].package.PackageFullName
  applicationId = $entry[0].applicationId
  appUserModelId = $entry[0].appUserModelId
  activatedProcessId = $activatedProcessId
  executable = $entry[0].executable
  codexHome = $codexHome
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

    pub fn launch(codex_home: &Path) -> Result<()> {
        let output = run_powershell(LAUNCH_SCRIPT, Some(codex_home))?;
        let result: LaunchResult = crate::json_compat::from_slice(&output)
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

    pub fn restart_if_needed(state: &DesktopSwitch, codex_home: &Path) -> Result<bool> {
        if !state.was_running {
            return Ok(false);
        }
        launch(codex_home)?;
        Ok(true)
    }

    fn run_powershell(script: &str, codex_home: Option<&Path>) -> Result<Vec<u8>> {
        let mut command = Command::new("powershell.exe");
        crate::child_process::isolate_from_connector_environment(&mut command);
        if let Some(codex_home) = codex_home {
            command.env("CODEX_HOME", codex_home);
        }
        let product_config = crate::product_config::get();
        let trusted_publishers = product_config.windows_desktop_trusted_publishers.join("\n");
        command
            .env(
                "CODEX_DESKTOP_PROTOCOL",
                &product_config.windows_desktop_protocol,
            )
            .env("CODEX_DESKTOP_TRUSTED_PUBLISHERS", trusted_publishers);
        let complete_script = format!("{POWERSHELL_PREAMBLE}\n{script}");
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &complete_script,
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
        use std::io::Write;
        use std::process::Stdio;

        #[test]
        fn desktop_management_scripts_parse_in_windows_powershell() {
            for script in [STOP_SCRIPT, LAUNCH_SCRIPT] {
                let complete_script = format!("{POWERSHELL_PREAMBLE}\n{script}");
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
                    .write_all(complete_script.as_bytes())
                    .unwrap();
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }

        #[test]
        fn packaged_app_launch_uses_aumid_and_restores_the_user_environment() {
            assert!(POWERSHELL_PREAMBLE.contains("IApplicationActivationManager"));
            assert!(POWERSHELL_PREAMBLE.contains("ActivateApplication"));
            assert!(POWERSHELL_PREAMBLE.contains("DoNotExpandEnvironmentNames"));
            assert!(POWERSHELL_PREAMBLE.contains("GetValueKind($name)"));
            assert!(POWERSHELL_PREAMBLE.contains("DeleteValue($name, $false)"));
            assert!(POWERSHELL_PREAMBLE.contains("BroadcastEnvironmentChange"));
            assert!(!POWERSHELL_PREAMBLE.contains("BAIJIMU_AUTH_FILE"));
            assert!(!POWERSHELL_PREAMBLE.contains("BAIJIMU_CURRENT_WORKSPACE_ID"));
            assert!(!POWERSHELL_PREAMBLE.contains("BaijimuAuthFile"));
            assert!(!POWERSHELL_PREAMBLE.contains("BaijimuCurrentWorkspaceId"));
            assert!(!POWERSHELL_PREAMBLE.contains("icacls.exe"));
            assert!(!POWERSHELL_PREAMBLE.contains("CodexSandbox"));
            assert!(LAUNCH_SCRIPT.contains(
                "Invoke-CodexDesktopActivation -AppUserModelId $entry[0].appUserModelId"
            ));
            assert!(LAUNCH_SCRIPT.contains("activationAccepted = $true"));
            assert!(LAUNCH_SCRIPT.contains("activationAccepted = $false"));
            assert!(!LAUNCH_SCRIPT.contains("VisibleWindow"));
            assert!(!LAUNCH_SCRIPT.contains("AddSeconds(45)"));
            assert!(!LAUNCH_SCRIPT.contains("Start-Process"));
            assert!(!LAUNCH_SCRIPT.contains("-WorkingDirectory"));
        }

        #[test]
        fn package_discovery_is_capability_based_and_errors_are_utf8() {
            assert!(POWERSHELL_PREAMBLE.contains("CODEX_DESKTOP_PROTOCOL"));
            assert!(POWERSHELL_PREAMBLE.contains("CODEX_DESKTOP_TRUSTED_PUBLISHERS"));
            assert!(!POWERSHELL_PREAMBLE.contains("CODEX_DESKTOP_TRUSTED_PUBLISHERS_JSON"));
            assert!(POWERSHELL_PREAMBLE.contains("Windows.FullTrustApplication"));
            assert!(!POWERSHELL_PREAMBLE.contains("OpenAI.ChatGPT-Desktop"));
            assert!(!POWERSHELL_PREAMBLE.contains("Get-AppxPackage -Name"));

            let error = run_powershell("throw '当前账户未发现桌面应用包'", None).unwrap_err();
            let message = error.to_string();
            assert!(message.contains("当前账户未发现桌面应用包"), "{message}");
            assert!(!message.contains('\u{fffd}'), "{message}");
        }

        #[test]
        fn package_discovery_accepts_a_renamed_package_with_the_codex_protocol() {
            let root = std::env::temp_dir().join(format!(
                "codex-desktop-package-discovery-{}",
                std::process::id()
            ));
            let app_dir = root.join("app");
            std::fs::create_dir_all(&app_dir).unwrap();
            std::fs::write(app_dir.join("Desktop.exe"), []).unwrap();
            std::fs::write(
                root.join("AppxManifest.xml"),
                r#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10" xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10">
  <Identity Name="Example.RenamedDesktop" Version="1.0.0.0" Publisher="CN=50BDFD77-8903-4850-9FFE-6E8522F64D5B" />
  <Applications>
    <Application Id="Desktop" Executable="app\Desktop.exe" EntryPoint="Windows.FullTrustApplication">
      <Extensions><uap:Extension Category="windows.protocol"><uap:Protocol Name="codex" /></uap:Extension></Extensions>
    </Application>
  </Applications>
</Package>"#,
            )
            .unwrap();
            let script = r#"
function Get-AppxPackage {
  [pscustomobject]@{
    InstallLocation = $env:CODEX_HOME
    PackageFamilyName = 'Example.RenamedDesktop_family'
    PackageFullName = 'Example.RenamedDesktop_1.0.0.0_x64__family'
  }
}
function Get-StartApps {
  [pscustomobject]@{ Name = 'Renamed desktop'; AppID = 'Example.RenamedDesktop_family!Desktop' }
}
$entry = @(Get-CodexDesktopEntries)
if ($entry.Count -ne 1) { throw "expected one entry, got $($entry.Count)" }
[pscustomobject]@{
  packageFullName = [string]$entry[0].package.PackageFullName
  applicationId = [string]$entry[0].applicationId
  appUserModelId = [string]$entry[0].appUserModelId
} | ConvertTo-Json -Compress
"#;

            let output = run_powershell(script, Some(&root)).unwrap();
            let value: serde_json::Value = crate::json_compat::from_slice(&output).unwrap();
            assert_eq!(
                value["packageFullName"],
                "Example.RenamedDesktop_1.0.0.0_x64__family"
            );
            assert_eq!(value["applicationId"], "Desktop");
            assert_eq!(
                value["appUserModelId"],
                "Example.RenamedDesktop_family!Desktop"
            );
            std::fs::remove_dir_all(root).unwrap();
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
    const POLL_INTERVAL: Duration = Duration::from_millis(500);

    pub fn stop_for_codex_home_switch() -> Result<DesktopSwitch> {
        let Some(app_path) = installed_application_path() else {
            return Ok(DesktopSwitch::default());
        };
        let bundle_id = application_bundle_id(&app_path)?;
        stop_application(&bundle_id)
    }

    fn stop_application(bundle_id: &str) -> Result<DesktopSwitch> {
        let info = application_info(bundle_id)?;
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
            if !has_running_process(&application_info(bundle_id)?) {
                return Ok(DesktopSwitch { was_running: true });
            }
            thread::sleep(POLL_INTERVAL);
        }
        anyhow::bail!("ChatGPT/Codex 桌面应用未在 15 秒内退出")
    }

    pub fn restart_if_needed(state: &DesktopSwitch, codex_home: &Path) -> Result<bool> {
        if !state.was_running {
            return Ok(false);
        }
        launch(codex_home)?;
        Ok(true)
    }

    pub fn verify_system_compatibility() -> Result<()> {
        let app_path =
            installed_application_path().context("没有找到已安装的 ChatGPT/Codex 桌面应用")?;
        verify_application_compatibility(&app_path)
    }

    pub fn launch(codex_home: &Path) -> Result<()> {
        let app_path =
            installed_application_path().context("没有找到已安装的 ChatGPT/Codex 桌面应用")?;
        verify_application_compatibility(&app_path)?;
        run_checked(
            open_application_command(&app_path, codex_home),
            "打开 ChatGPT/Codex 桌面应用失败",
        )
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

    fn open_application_command(app_path: &Path, codex_home: &Path) -> Command {
        let mut command = Command::new("/usr/bin/open");
        crate::child_process::isolate_from_connector_environment(&mut command);
        command
            .arg("--env")
            .arg(environment_assignment("CODEX_HOME", codex_home));
        command.arg(app_path);
        command
    }

    fn environment_assignment(name: &str, value: &Path) -> std::ffi::OsString {
        let mut assignment = std::ffi::OsString::from(format!("{name}="));
        assignment.push(value);
        assignment
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
        fn parses_lsappinfo_process_state() {
            let hidden = "\"pid\"=682\n\"visible\"=[ NULL ]\n\"windows\"=[ NULL ]\n";
            assert!(has_running_process(hidden));

            let missing = "Application not found\n";
            assert!(!has_running_process(missing));
        }

        #[test]
        fn workspace_launch_only_injects_the_selected_codex_home() {
            let app = Path::new("/Applications/Codex.app");
            let codex_home = Path::new("/Users/example/.baijimu/codex/p/profile");
            let command = open_application_command(app, codex_home);
            let args = command
                .get_args()
                .map(|value| value.to_string_lossy().into_owned())
                .collect::<Vec<_>>();

            assert!(args.contains(&"CODEX_HOME=/Users/example/.baijimu/codex/p/profile".into()));
            assert!(!args.iter().any(|arg| arg.starts_with("BAIJIMU_")));
            assert_eq!(
                args.last().map(String::as_str),
                Some("/Applications/Codex.app")
            );
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;

    pub fn stop_for_codex_home_switch() -> Result<DesktopSwitch> {
        Ok(DesktopSwitch::default())
    }

    pub fn restart_if_needed(_state: &DesktopSwitch, _codex_home: &Path) -> Result<bool> {
        Ok(false)
    }
}
