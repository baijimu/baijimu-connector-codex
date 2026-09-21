$ErrorActionPreference = "Stop"
# Only MSIX installation may cross the UAC boundary. Never elevate the setup script.
function ConvertTo-PowerShellLiteral([string]$value) {
  return "'" + $value.Replace("'", "''") + "'"
}

function Get-MsixElevationScript([string]$packagePath, [string]$sha256, [string]$userSid, [string]$resultPath) {
  $template = @'
$ErrorActionPreference = 'Stop'
$packagePath = __PACKAGE__
$expectedHash = __HASH__
$expectedSid = __SID__
$resultPath = __RESULT__
# MSIX registration is per user; do not silently install under another administrator.
if ([Security.Principal.WindowsIdentity]::GetCurrent().User.Value -ne $expectedSid) { exit 13 }
$packageLock = $null
$result = @{ ok = $false; error = $null }
try {
  $packageLock = [IO.File]::Open($packagePath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
  if ((Get-FileHash -LiteralPath $packagePath -Algorithm SHA256).Hash -ine $expectedHash) {
    throw '安装包已改变，校验失败，请重新下载后再安装。'
  }
  Add-AppxPackage -Path $packagePath -ErrorAction Stop
  $result.ok = $true
} catch {
  $result.error = $_.Exception.Message
} finally {
  if ($packageLock) { $packageLock.Dispose() }
}
[IO.File]::WriteAllText($resultPath, ($result | ConvertTo-Json), (New-Object Text.UTF8Encoding($false)))
if (-not $result.ok) { exit 1 }
'@
  # Replace placeholders in one pass; paths are data, never executable PowerShell.
  $values = @{
    '__PACKAGE__' = (ConvertTo-PowerShellLiteral $packagePath)
    '__HASH__' = (ConvertTo-PowerShellLiteral $sha256)
    '__SID__' = (ConvertTo-PowerShellLiteral $userSid)
    '__RESULT__' = (ConvertTo-PowerShellLiteral $resultPath)
  }
  return [regex]::Replace($template, '__PACKAGE__|__HASH__|__SID__|__RESULT__', { param($match) $values[$match.Value] })
}

function Get-MsixInstallerUserSid {
  return [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
}

function Install-MsixElevated([string]$packagePath, [string]$sha256, [string]$stateDirectory) {
  $resultFile = Join-Path $stateDirectory ("msix-elevation-{0}.json" -f [Guid]::NewGuid().ToString('N'))
  $userSid = Get-MsixInstallerUserSid
  $script = Get-MsixElevationScript $packagePath $sha256 $userSid $resultFile
  $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($script))
  try {
    try {
      $process = Start-Process -FilePath (Join-Path $PSHOME 'powershell.exe') -Verb RunAs -ArgumentList @(
        '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-EncodedCommand', $encoded
      ) -Wait -PassThru
    } catch {
      $cause = $_.Exception
      while ($cause -and $cause.NativeErrorCode -ne 1223) { $cause = $cause.InnerException }
      if ($cause) { throw 'MSIX_ELEVATION_CANCELLED: 已取消管理员授权，安装包已保留，可以再次点击重试。' }
      throw
    }
    if ($process.ExitCode -eq 13) {
      throw 'MSIX_ELEVATION_USER_MISMATCH: 授权账号与当前 Windows 用户不同，已停止安装。请联系管理员为当前用户安排安装。'
    }
    if (-not (Test-Path -LiteralPath $resultFile -PathType Leaf)) {
      throw "管理员安装进程未返回结果（退出码 $($process.ExitCode)），可以重试。"
    }
    $result = Get-Content -Raw -Encoding UTF8 -LiteralPath $resultFile | ConvertFrom-Json
    if ($process.ExitCode -ne 0 -or -not $result.ok) { throw "管理员安装失败：$($result.error)" }
  } finally {
    Remove-Item -LiteralPath $resultFile -Force -ErrorAction SilentlyContinue
  }
}
