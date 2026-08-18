$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$CodexUiLocale = $env:CODEX_UI_LOCALE
if ($CodexUiLocale -notmatch '^[A-Za-z]{2,3}(?:-[A-Za-z0-9]{2,8})*$') {
  throw "缺少或无效的 CODEX_UI_LOCALE"
}
$WorkspaceId = if ($env:CODEX_WORKSPACE_ID) { $env:CODEX_WORKSPACE_ID } elseif ($env:BAIJIMU_WORKSPACE_ID) { $env:BAIJIMU_WORKSPACE_ID } else { $env:WORKSPACE_ID }
if (-not $WorkspaceId -or $WorkspaceId -notmatch '^\d+$') {
  throw "必须提供 CODEX_WORKSPACE_ID 或 BAIJIMU_WORKSPACE_ID"
}

$startedAt = Get-Date
$stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
$codexDir = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $env:USERPROFILE ".codex" }
$configPath = Join-Path $codexDir "config.toml"
$authPath = Join-Path $codexDir "auth.json"
$installStateDir = if ($env:CODEX_INSTALL_STATE_DIR) { $env:CODEX_INSTALL_STATE_DIR } else { Join-Path $env:TEMP "baijimu-codex-install" }
$statusPath = Join-Path $installStateDir "status.json"
$resultPath = Join-Path $installStateDir "result.json"
New-Item -ItemType Directory -Force -Path $installStateDir | Out-Null

$script:Utf8NoBomEncoding = New-Object System.Text.UTF8Encoding($false)

function Write-Utf8NoBomFile([string]$path, [AllowEmptyString()][string]$content) {
  $fullPath = [System.IO.Path]::GetFullPath($path)
  $directory = [System.IO.Path]::GetDirectoryName($fullPath)
  if (-not [string]::IsNullOrWhiteSpace($directory)) {
    [System.IO.Directory]::CreateDirectory($directory) | Out-Null
  }
  $temporaryPath = Join-Path $directory (".{0}.tmp-{1}-{2}" -f [System.IO.Path]::GetFileName($fullPath), $PID, [Guid]::NewGuid().ToString('N'))
  try {
    [System.IO.File]::WriteAllText($temporaryPath, $content, $script:Utf8NoBomEncoding)
    if (Test-Path -LiteralPath $fullPath -PathType Leaf) {
      [System.IO.File]::Replace(
        $temporaryPath,
        $fullPath,
        [System.Management.Automation.Language.NullString]::Value,
        $true
      )
    } else {
      [System.IO.File]::Move($temporaryPath, $fullPath)
    }
  } finally {
    if (Test-Path -LiteralPath $temporaryPath) {
      Remove-Item -LiteralPath $temporaryPath -Force -ErrorAction SilentlyContinue
    }
  }
}

$script:CurrentStepIndex = 0
$script:InstallSteps = @(
  [pscustomobject]@{ index = 1; name = "检查 ChatGPT 桌面应用"; state = "pending"; detail = ""; downloadedBytes = $null; totalBytes = $null },
  [pscustomobject]@{ index = 2; name = "读取安装包清单"; state = "pending"; detail = ""; downloadedBytes = $null; totalBytes = $null },
  [pscustomobject]@{ index = 3; name = "下载 ChatGPT 安装包"; state = "pending"; detail = ""; downloadedBytes = $null; totalBytes = $null },
  [pscustomobject]@{ index = 4; name = "校验安装包"; state = "pending"; detail = ""; downloadedBytes = $null; totalBytes = $null },
  [pscustomobject]@{ index = 5; name = "安装 ChatGPT 桌面应用"; state = "pending"; detail = ""; downloadedBytes = $null; totalBytes = $null },
  [pscustomobject]@{ index = 6; name = "确认百积木 LLM 凭证和配置"; state = "pending"; detail = ""; downloadedBytes = $null; totalBytes = $null },
  [pscustomobject]@{ index = 7; name = "验证百积木路由"; state = "pending"; detail = ""; downloadedBytes = $null; totalBytes = $null },
  [pscustomobject]@{ index = 8; name = "完成安装配置"; state = "pending"; detail = ""; downloadedBytes = $null; totalBytes = $null }
)

$result = [ordered]@{
  ok = $false
  platform = "windows"
  startedAt = $startedAt.ToString("o")
  codexHome = $codexDir
  appInstalled = $false
  appInstallMethod = $null
  appId = $null
  workspaceId = [int64]$WorkspaceId
  llmCredentialCreated = $false
  codexAuthWritten = $false
  configWritten = $false
  authWritten = $false
  routerHttpStatus = $null
  elapsedMs = 0
  warnings = @()
  errors = @()
}

function Add-Warning([string]$message) {
  $script:result.warnings += $message
}

function Add-Error([string]$message) {
  $script:result.errors += $message
}

function Write-InstallConsole([string]$message) {
  if ($env:CODEX_INSTALL_QUIET -eq "1") { return }
  [Console]::Error.WriteLine($message)
}

function Write-InstallStatus {
  $status = [ordered]@{
    title = "百积木正在安装 ChatGPT 桌面应用"
    locale = $CodexUiLocale
    platform = "windows"
    startedAt = $startedAt.ToString("o")
    updatedAt = (Get-Date).ToString("o")
    currentStep = $script:CurrentStepIndex
    statusPath = $statusPath
    resultPath = $resultPath
    steps = $script:InstallSteps
  }
  Write-Utf8NoBomFile $statusPath (($status | ConvertTo-Json -Depth 8) + "`n")
}

function Set-InstallStep([int]$index, [string]$state, [string]$detail = "", [Nullable[Int64]]$downloadedBytes = $null, [Nullable[Int64]]$totalBytes = $null) {
  $step = $script:InstallSteps | Where-Object { $_.index -eq $index } | Select-Object -First 1
  if (-not $step) { return }
  $script:CurrentStepIndex = $index
  $step.state = $state
  $step.detail = $detail
  $step.downloadedBytes = $downloadedBytes
  $step.totalBytes = $totalBytes
  Write-InstallStatus

  $label = "[{0}/{1}] {2}" -f $index, $script:InstallSteps.Count, $step.name
  if ($downloadedBytes -ne $null -and $totalBytes -ne $null -and $totalBytes -gt 0) {
    $downloadedMb = [math]::Round($downloadedBytes / 1MB, 1)
    $totalMb = [math]::Round($totalBytes / 1MB, 1)
    Write-InstallConsole ("{0}  {1}  {2}MB / {3}MB" -f $label, $state, $downloadedMb, $totalMb)
  } elseif ($detail) {
    Write-InstallConsole ("{0}  {1}  {2}" -f $label, $state, $detail)
  } else {
    Write-InstallConsole ("{0}  {1}" -f $label, $state)
  }
}

function Complete-PendingInstallSteps([string]$state, [string]$detail) {
  foreach ($step in $script:InstallSteps) {
    if ($step.state -eq "pending") {
      $step.state = $state
      $step.detail = $detail
    }
  }
  Write-InstallStatus
}

function Save-WebFileWithProgress([string]$uri, [string]$outFile, [int]$stepIndex, [string]$label, [Int64]$totalBytesHint = 0) {
  Set-InstallStep $stepIndex "running" $label
  $request = [System.Net.HttpWebRequest]::Create($uri)
  $request.UserAgent = "Baijimu-ChatGPT-Desktop-Installer/1.0"
  $request.Timeout = 1200000
  $request.ReadWriteTimeout = 1200000
  $response = $request.GetResponse()
  try {
    $totalBytes = [Int64]$response.ContentLength
    if ($totalBytesHint -gt 0) { $totalBytes = $totalBytesHint }
    $inputStream = $response.GetResponseStream()
    $outputStream = [System.IO.File]::Open($outFile, [System.IO.FileMode]::Create, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
    try {
      $buffer = New-Object byte[] 1048576
      [Int64]$downloadedBytes = 0
      $lastUpdate = Get-Date
      while (($read = $inputStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
        $outputStream.Write($buffer, 0, $read)
        $downloadedBytes += $read
        if (((Get-Date) - $lastUpdate).TotalSeconds -ge 1) {
          Set-InstallStep $stepIndex "running" $label $downloadedBytes $totalBytes
          $lastUpdate = Get-Date
        }
      }
      Set-InstallStep $stepIndex "completed" $label $downloadedBytes $totalBytes
    } finally {
      $outputStream.Close()
      $inputStream.Close()
    }
  } finally {
    $response.Close()
  }
}

Write-InstallConsole ""
Write-InstallConsole "百积木正在安装 ChatGPT 桌面应用"
Write-InstallConsole "请保持此窗口打开。"
Write-InstallConsole ""
Write-InstallStatus

function Get-CodexDesktopEntries {
  $codexDesktopProtocol = $env:CODEX_DESKTOP_PROTOCOL
  if ([string]::IsNullOrWhiteSpace($codexDesktopProtocol)) { throw "缺少 Windows 桌面协议配置" }
  $codexDesktopTrustedPublishers = @($env:CODEX_DESKTOP_TRUSTED_PUBLISHERS -split '\r?\n' | ForEach-Object { $_.Trim() } | Where-Object { $_ })
  if ($codexDesktopTrustedPublishers.Count -eq 0) { throw "Windows 桌面可信 Publisher 配置为空" }
  $startApps = @(Get-StartApps -ErrorAction SilentlyContinue)
  $packages = @(Get-AppxPackage -ErrorAction SilentlyContinue | Where-Object { $_.InstallLocation })
  $entries = @($packages | ForEach-Object {
    $package = $_
    try {
      $manifestPath = Join-Path $package.InstallLocation "AppxManifest.xml"
      if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) { return }
      [xml]$manifest = Get-Content -Raw -LiteralPath $manifestPath
      $identity = @($manifest.SelectNodes("/*[local-name()='Package']/*[local-name()='Identity']") | Select-Object -First 1)
      if ($identity.Count -eq 0) { return }
      $publisher = [string]$identity[0].Publisher
      if ($codexDesktopTrustedPublishers -notcontains $publisher) { return }
      $startApp = @($startApps | Where-Object { $_.AppID -like "$($package.PackageFamilyName)!*" } | Select-Object -First 1)
      $applicationId = if ($startApp.Count -gt 0) {
        ([string]$startApp[0].AppID).Substring(([string]$startApp[0].AppID).LastIndexOf("!") + 1)
      } else {
        $null
      }
      $applications = @($manifest.SelectNodes("/*[local-name()='Package']/*[local-name()='Applications']/*[local-name()='Application']") | Where-Object {
        if (-not $_.Executable) { return $false }
        $entryPoint = [string]$_.EntryPoint
        $isFullTrust = [string]::IsNullOrWhiteSpace($entryPoint) -or $entryPoint -eq "Windows.FullTrustApplication"
        $declaresCodexProtocol = @($_.SelectNodes(".//*[local-name()='Protocol']") | Where-Object { ([string]$_.Name) -eq $codexDesktopProtocol }).Count -gt 0
        return $isFullTrust -and $declaresCodexProtocol -and (-not $applicationId -or ([string]$_.Id) -eq $applicationId)
      })
      if ($applications.Count -eq 0 -and $applicationId) { return }
      @($applications | Select-Object -First 1) | ForEach-Object {
        $relativeExecutable = [string]$_.Executable
        if ([System.IO.Path]::IsPathRooted($relativeExecutable)) { return }
        $packageRoot = [System.IO.Path]::GetFullPath($package.InstallLocation).TrimEnd("\") + "\"
        $executable = [System.IO.Path]::GetFullPath((Join-Path $packageRoot $relativeExecutable))
        if (-not $executable.StartsWith($packageRoot, [System.StringComparison]::OrdinalIgnoreCase)) { return }
        if ([System.IO.Path]::GetExtension($executable) -ne ".exe" -or -not (Test-Path -LiteralPath $executable -PathType Leaf)) { return }
        [pscustomobject]@{
          package = $package
          startApp = if ($startApp.Count -gt 0) { $startApp[0] } else { $null }
        }
      }
    } catch { return }
  })
  @($entries | Sort-Object @{ Expression = { if ($_.startApp) { 0 } else { 1 } } }, @{ Expression = { if ($_.startApp) { [string]$_.startApp.AppID } else { "" } } }, @{ Expression = { [string]$_.package.PackageFullName } })
}

function Get-CodexStartApp {
  $entry = @(Get-CodexDesktopEntries | Where-Object { $_.startApp } | Select-Object -First 1)
  if ($entry.Count -gt 0) { return $entry[0].startApp }
  return $null
}

function Get-CodexInstalledPackage {
  $entry = @(Get-CodexDesktopEntries | Select-Object -First 1)
  if ($entry.Count -gt 0) { return $entry[0].package }
  return $null
}

function Assert-CurrentWindowsVersion([string]$minimumVersion) {
  if ([string]::IsNullOrWhiteSpace($minimumVersion)) {
    throw "ChatGPT/Codex 制品未声明最低 Windows 版本"
  }
  try {
    $minimum = [version]$minimumVersion
    $current = [System.Environment]::OSVersion.Version
  } catch {
    throw "ChatGPT/Codex 系统版本要求格式无效：$minimumVersion"
  }
  if ($current -lt $minimum) {
    throw "UNSUPPORTED_OS_VERSION: 当前 Windows $current 低于 ChatGPT/Codex 要求的最低系统版本 $minimum，请先升级操作系统"
  }
}

function Assert-InstalledCodexPackageCompatibility($package) {
  if (-not $package -or -not $package.InstallLocation) {
    throw "无法读取已安装 ChatGPT/Codex 应用包目录"
  }
  $manifestPath = Join-Path $package.InstallLocation "AppxManifest.xml"
  if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) {
    throw "已安装 ChatGPT/Codex 应用包缺少 AppxManifest.xml"
  }
  [xml]$manifest = Get-Content -LiteralPath $manifestPath
  $minimum = @(
    $manifest.Package.Dependencies.TargetDeviceFamily |
      ForEach-Object { [version]([string]$_.MinVersion) } |
      Sort-Object -Descending |
      Select-Object -First 1
  )[0]
  if (-not $minimum) { throw "已安装 ChatGPT/Codex 应用包未声明最低 Windows 版本" }
  Assert-CurrentWindowsVersion $minimum.ToString()
}

function Get-CodexWindowsAppAssetName {
  $arch = (Get-CimInstance Win32_Processor | Select-Object -First 1).Architecture
  if ($arch -eq 12 -or $env:PROCESSOR_ARCHITECTURE -eq "ARM64") {
    return "codex-app-windows-arm64.msix"
  }
  return "codex-app-windows-x64.msix"
}

function Wait-CodexStartApp([int]$timeoutSeconds) {
  $deadline = (Get-Date).AddSeconds($timeoutSeconds)
  do {
    $app = Get-CodexStartApp
    if ($app) { return $app }
    Start-Sleep -Seconds 2
  } while ((Get-Date) -lt $deadline)
  return $null
}

function Get-CodexCacheAsset([string]$assetName) {
  Set-InstallStep 2 "running" "正在读取百积木安装包清单"
  $manifestUrl = $env:CODEX_ARTIFACT_MANIFEST_URL
  if ([string]::IsNullOrWhiteSpace($manifestUrl)) { throw "缺少安装制品清单地址" }
  $manifestPath = Join-Path $env:TEMP "codex-artifacts-latest.json"
  Save-WebFileWithProgress $manifestUrl $manifestPath 2 "正在读取百积木安装包清单"
  $manifest = Get-Content -Raw -Path $manifestPath | ConvertFrom-Json
  $asset = @($manifest.assets | Where-Object { $_.name -eq $assetName } | Select-Object -First 1)
  if (-not $asset) {
    throw "百积木缓存缺少制品：$assetName"
  }
  if (-not $asset.mirror_url -or -not $asset.sha256) {
    throw "百积木缓存中的制品不完整：$assetName"
  }
  if ($asset.component -eq "codex_desktop_app") {
    Assert-CurrentWindowsVersion ([string]$asset.host_requirements.minimum_os_version)
  }
  Set-InstallStep 2 "completed" "已找到制品 $assetName"
  return $asset
}

function Install-CodexAppFromBaijimuCache {
  $assetName = Get-CodexWindowsAppAssetName
  $asset = Get-CodexCacheAsset $assetName
  $packagePath = Join-Path $env:TEMP $assetName
  $assetSize = 0
  if ($asset.size_bytes) { $assetSize = [Int64]$asset.size_bytes }
  elseif ($asset.size) { $assetSize = [Int64]$asset.size }
  elseif ($asset.file_size) { $assetSize = [Int64]$asset.file_size }
  Save-WebFileWithProgress $asset.mirror_url $packagePath 3 "正在下载官方 ChatGPT 桌面应用安装包" $assetSize
  Set-InstallStep 4 "running" "正在校验安装包 SHA256"
  $actual = (Get-FileHash -Algorithm SHA256 -Path $packagePath).Hash.ToLowerInvariant()
  $expected = [string]$asset.sha256
  if ($actual -ne $expected.ToLowerInvariant()) {
    throw "制品 SHA256 不匹配：$assetName"
  }
  Set-InstallStep 4 "completed" "安装包 SHA256 校验通过"
  Unblock-File -Path $packagePath -ErrorAction SilentlyContinue
  $script:result.appInstallMethod = "baijimu-cache-msix"
  Set-InstallStep 5 "running" "正在安装 ChatGPT 桌面应用"
  Add-AppxPackage -Path $packagePath
  Set-InstallStep 5 "completed" "ChatGPT 桌面应用已安装"
}

function Ensure-CodexApp {
  Set-InstallStep 1 "running" "正在检查是否已安装 ChatGPT 桌面应用"
  $app = Get-CodexStartApp
  if ($app) {
    Assert-InstalledCodexPackageCompatibility (Get-CodexInstalledPackage)
    $script:result.appInstalled = $true
    $script:result.appInstallMethod = "already-installed"
    $script:result.appId = $app.AppID
    Set-InstallStep 1 "completed" "ChatGPT 桌面应用已安装"
    Set-InstallStep 2 "skipped" "无需读取安装包清单"
    Set-InstallStep 3 "skipped" "无需下载安装包"
    Set-InstallStep 4 "skipped" "无需校验安装包"
    Set-InstallStep 5 "skipped" "无需重新安装"
    return
  }

  $package = Get-CodexInstalledPackage
  if ($package) {
    Assert-InstalledCodexPackageCompatibility $package
    $script:result.appInstalled = $true
    $script:result.appInstallMethod = "already-installed"
    $app = Wait-CodexStartApp 20
    if ($app) { $script:result.appId = $app.AppID }
    Set-InstallStep 1 "completed" "ChatGPT 桌面应用安装包已安装"
    Set-InstallStep 2 "skipped" "无需读取安装包清单"
    Set-InstallStep 3 "skipped" "无需下载安装包"
    Set-InstallStep 4 "skipped" "无需校验安装包"
    Set-InstallStep 5 "skipped" "无需重新安装"
    return
  }
  Set-InstallStep 1 "completed" "未安装 ChatGPT 桌面应用，正在准备安装"

  try {
    Install-CodexAppFromBaijimuCache
  } catch {
    if ($_.Exception.Message -like "*UNSUPPORTED_OS_VERSION*") { throw }
    Add-Warning "使用百积木缓存安装失败：$($_.Exception.Message)"
    if ($env:CODEX_ALLOW_OFFICIAL_WINDOWS_INSTALLER_FALLBACK -eq "1") {
      $script:result.appInstallMethod = "official-installer"
      $installer = Join-Path $env:TEMP "ChatGPT Installer.exe"
      Save-WebFileWithProgress "https://get.microsoft.com/installer/download/9PLM9XGG6VKS?cid=website_cta_psi" $installer 3 "正在下载官方安装器"
      Set-InstallStep 5 "running" "正在运行官方安装器"
      Start-Process -FilePath $installer -Wait
      Set-InstallStep 5 "completed" "官方安装器已运行完成"
    } elseif ($env:CODEX_ALLOW_WINGET_FALLBACK -eq "1") {
      $winget = Get-Command winget -ErrorAction SilentlyContinue
      if (-not $winget) {
        throw "使用百积木缓存安装失败，并且 winget 不可用"
      }
      $script:result.appInstallMethod = "winget-msstore"
      Set-InstallStep 5 "running" "正在通过 Microsoft Store 安装"
      & winget install --id 9PLM9XGG6VKS -s msstore --accept-package-agreements --accept-source-agreements | Out-Null
      Set-InstallStep 5 "completed" "Microsoft Store 安装已完成"
    } else {
      throw
    }
  }

  $app = Wait-CodexStartApp 60
  if (-not $app) {
    $package = Get-CodexInstalledPackage
    if ($package) {
      throw "ChatGPT 桌面应用安装包已安装，但安装后未找到开始菜单入口"
    }
    throw "安装后未找到 ChatGPT 桌面应用安装包和开始菜单入口"
  }

  $script:result.appInstalled = $true
  $script:result.appId = $app.AppID
  Set-InstallStep 5 "completed" "ChatGPT 桌面应用可以启动"
}

function Confirm-CodexConfiguration {
  Set-InstallStep 6 "running" "正在确认百积木 LLM 凭证和 Codex 配置"
  if (-not (Test-Path -LiteralPath $authPath -PathType Leaf)) {
    throw "Rust 凭证管理器未生成 Codex 授权文件：$authPath"
  }
  if (-not (Test-Path -LiteralPath $configPath -PathType Leaf)) {
    throw "Rust 凭证管理器未生成 Codex 配置文件：$configPath"
  }
  $auth = Get-Content -Raw -LiteralPath $authPath | ConvertFrom-Json
  if (-not $auth.OPENAI_API_KEY) {
    throw "Rust 凭证管理器生成的 Codex 授权文件中不包含 OPENAI_API_KEY"
  }
  $config = Get-Content -Raw -LiteralPath $configPath
  if ([string]::IsNullOrWhiteSpace($config)) {
    throw "Rust 凭证管理器生成的 Codex 配置文件为空"
  }
  Remove-Variable auth, config -ErrorAction SilentlyContinue
  $script:result.llmCredentialCreated = $true
  $script:result.codexAuthWritten = $true
  $script:result.configWritten = $true
  $script:result.authWritten = $true
  Set-InstallStep 6 "completed" "已确认百积木 LLM 凭证和 Codex 配置"
}

try {
  Ensure-CodexApp
  Confirm-CodexConfiguration
  Set-InstallStep 7 "skipped" "安装完成后由桌面管理器在后台验证百积木路由"
  Set-InstallStep 8 "completed" "安装配置已完成，桌面启动由桌面管理器按档案状态处理"
} catch {
  Add-Error $_.Exception.Message
  if ($script:CurrentStepIndex -gt 0) {
    Set-InstallStep $script:CurrentStepIndex "failed" $_.Exception.Message
  }
}

$stopwatch.Stop()
$result.elapsedMs = [int]$stopwatch.ElapsedMilliseconds
$result.ok = ($result.errors.Count -eq 0)
$resultJson = $result | ConvertTo-Json -Depth 6
Write-Utf8NoBomFile $resultPath ($resultJson + "`n")
if ($result.ok) {
  Complete-PendingInstallSteps "skipped" "安装已完成"
  Write-InstallConsole ""
  Write-InstallConsole "ChatGPT 桌面应用和 Codex 配置已完成，可以关闭此窗口。"
} else {
  Complete-PendingInstallSteps "skipped" "安装已停止"
  Write-InstallConsole ""
  Write-InstallConsole "ChatGPT 桌面应用和 Codex 配置失败，请将错误信息发送给百积木。"
}
$resultJson

if (-not $result.ok) {
  exit 1
}
