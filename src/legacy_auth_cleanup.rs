use anyhow::{Context, Result};
use std::process::Command;

const REVOKE_LEGACY_SHARED_AUTH_ACL_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$OutputEncoding = [Console]::OutputEncoding

$authFile = $env:CODEX_DESKTOP_LEGACY_AUTH_FILE
if ([string]::IsNullOrWhiteSpace($authFile) -or -not [System.IO.Path]::IsPathRooted($authFile)) {
  throw '旧版共享授权清理要求绝对授权文件路径'
}
$authDirectory = Split-Path -Parent $authFile
if (-not (Test-Path -LiteralPath $authDirectory -PathType Container)) { exit 0 }

$sandboxPrincipalNames = [System.Collections.Generic.HashSet[string]]::new([System.StringComparer]::OrdinalIgnoreCase)
@('CodexSandboxOffline', 'CodexSandboxOnline') | ForEach-Object {
  [void]$sandboxPrincipalNames.Add($_)
}
$codexHomes = @($env:CODEX_DESKTOP_MANAGED_HOMES_JSON | ConvertFrom-Json)
foreach ($codexHome in $codexHomes) {
  $markerPath = Join-Path ([string]$codexHome) '.sandbox\setup_marker.json'
  if (-not (Test-Path -LiteralPath $markerPath -PathType Leaf)) { continue }
  try {
    $marker = Get-Content -Raw -LiteralPath $markerPath | ConvertFrom-Json
    @($marker.offline_username, $marker.online_username) | Where-Object {
      -not [string]::IsNullOrWhiteSpace([string]$_)
    } | ForEach-Object {
      [void]$sandboxPrincipalNames.Add(([string]$_).Trim())
    }
  } catch {
    throw "读取旧版 Codex Windows 沙箱主体登记失败：$markerPath"
  }
}

$sandboxSids = @($sandboxPrincipalNames | ForEach-Object {
  try {
    ([System.Security.Principal.NTAccount]::new($env:COMPUTERNAME, $_)).Translate([System.Security.Principal.SecurityIdentifier]).Value
  } catch { $null }
} | Where-Object { $_ } | Sort-Object -Unique)

foreach ($sidValue in $sandboxSids) {
  $sid = [System.Security.Principal.SecurityIdentifier]::new($sidValue)
  $directoryAcl = Get-Acl -LiteralPath $authDirectory
  $directoryRule = [System.Security.AccessControl.FileSystemAccessRule]::new(
    $sid,
    [System.Security.AccessControl.FileSystemRights]::ReadAndExecute,
    [System.Security.AccessControl.InheritanceFlags]::ContainerInherit -bor [System.Security.AccessControl.InheritanceFlags]::ObjectInherit,
    [System.Security.AccessControl.PropagationFlags]::None,
    [System.Security.AccessControl.AccessControlType]::Allow
  )
  $directoryAcl.RemoveAccessRuleSpecific($directoryRule)
  Set-Acl -LiteralPath $authDirectory -AclObject $directoryAcl

  if (Test-Path -LiteralPath $authFile -PathType Leaf) {
    $fileAcl = Get-Acl -LiteralPath $authFile
    $fileRule = [System.Security.AccessControl.FileSystemAccessRule]::new(
      $sid,
      [System.Security.AccessControl.FileSystemRights]::Read,
      [System.Security.AccessControl.AccessControlType]::Allow
    )
    $fileAcl.RemoveAccessRuleSpecific($fileRule)
    Set-Acl -LiteralPath $authFile -AclObject $fileAcl
  }
}
"#;

pub fn run_once() -> Result<()> {
    if !crate::credential::legacy_shared_auth_acl_cleanup_required()? {
        return Ok(());
    }
    let auth_file = crate::baijimu_cli::legacy_shared_auth_path_for_acl_cleanup()
        .context("读取旧版共享授权路径失败")?;
    let managed_homes = crate::credential::managed_codex_homes_for_legacy_acl_cleanup()?;
    let managed_homes_json = serde_json::to_string(&managed_homes)?;

    let mut command = Command::new("powershell.exe");
    crate::child_process::isolate_from_connector_environment(&mut command);
    command
        .env("CODEX_DESKTOP_LEGACY_AUTH_FILE", &auth_file)
        .env("CODEX_DESKTOP_MANAGED_HOMES_JSON", managed_homes_json)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            REVOKE_LEGACY_SHARED_AUTH_ACL_SCRIPT,
        ]);
    let output = command
        .output()
        .context("启动 PowerShell 回收旧版 Codex 沙箱全局授权失败")?;
    if !output.status.success() {
        anyhow::bail!(
            "回收旧版 Codex 沙箱全局授权失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    crate::credential::record_legacy_shared_auth_acl_cleanup()
        .context("旧版 Codex 沙箱全局授权已回收，但记录清理状态失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Stdio;

    #[test]
    fn cleanup_script_parses_and_only_removes_the_exact_legacy_rules() {
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
            .write_all(REVOKE_LEGACY_SHARED_AUTH_ACL_SCRIPT.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(REVOKE_LEGACY_SHARED_AUTH_ACL_SCRIPT.contains("RemoveAccessRuleSpecific"));
        assert!(REVOKE_LEGACY_SHARED_AUTH_ACL_SCRIPT.contains("ReadAndExecute"));
        assert!(REVOKE_LEGACY_SHARED_AUTH_ACL_SCRIPT.contains("ContainerInherit"));
        assert!(REVOKE_LEGACY_SHARED_AUTH_ACL_SCRIPT.contains("ObjectInherit"));
        assert!(!REVOKE_LEGACY_SHARED_AUTH_ACL_SCRIPT.contains("AddAccessRule"));
        assert!(!REVOKE_LEGACY_SHARED_AUTH_ACL_SCRIPT.contains("SetAccessRule"));
    }
}
