$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
. ([scriptblock]::Create([IO.File]::ReadAllText((Join-Path $root 'installers/windows-msix-install.ps1'))))
$realElevatedInstall = ${function:Install-MsixElevated}
function Assert([bool]$condition, [string]$message) {
  if (-not $condition) { throw $message }
}
function Assert-Throws([scriptblock]$action, [string]$pattern) {
  $failure = $null
  try { & $action } catch { $failure = $_.Exception.Message }
  Assert ($failure -and $failure -match $pattern) "Expected error matching $pattern; got $failure"
}
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('msix-recovery-test-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $testRoot | Out-Null
try {
  $tokens = $null
  $errors = $null
  $installer = Join-Path $root 'installers/windows-configure-terminal-and-login.ps1'
  $ast = [Management.Automation.Language.Parser]::ParseInput([IO.File]::ReadAllText($installer), [ref]$tokens, [ref]$errors)
  Assert ($errors.Count -eq 0) 'Installer must parse'
  # Load functions without executing setup or changing any real credentials.
  foreach ($name in @('Write-Utf8NoBomFile', 'Install-CodexAppFromBaijimuCache')) {
    $definition = $ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name}, $true)
    . ([scriptblock]::Create($definition.Extent.Text))
  }
  $script:Utf8NoBomEncoding = New-Object Text.UTF8Encoding($false)
  $installStateDir = $testRoot
  $script:result = @{}
  $script:steps = @{}
  $script:downloads = 0
  $script:catalogReads = 0
  $script:installs = 0
  $script:elevations = 0
  function Set-InstallStep($index, $state, $detail) { $script:steps[$index] = $state }
  function Get-CodexWindowsAppAssetName { return 'test.msix' }
  $seed = Join-Path $testRoot 'seed'
  [IO.File]::WriteAllText($seed, 'verified artifact')
  $hash = (Get-FileHash -LiteralPath $seed -Algorithm SHA256).Hash
  function Get-CodexCacheAsset($name) {
    $script:catalogReads++
    return @{ name = $name; sha256 = $hash; mirror_url = 'https://example.invalid/test.msix' }
  }
  function Save-WebFileWithProgress($url, $path) { $script:downloads++; Copy-Item -LiteralPath $seed -Destination $path }
  function Unblock-File { param($LiteralPath, $ErrorAction) }
  function Add-AppxPackage { param($Path, $ErrorAction) $script:installs++; throw 'Deployment failed HRESULT 0x80073D28' }
  function Install-MsixElevated { param($packagePath, $sha256, $stateDirectory) $script:elevations++ }
  $env:CODEX_INSTALL_ELEVATE = '0'
  Assert-Throws { Install-CodexAppFromBaijimuCache } 'MSIX_ELEVATION_REQUIRED'
  Assert ($script:PackageRecovery.requiresElevation) 'Permission error must enable elevation'
  Assert ($script:downloads -eq 1 -and $script:installs -eq 1) 'Initial attempt must download and install once'
  $env:CODEX_INSTALL_ELEVATE = '1'
  Install-CodexAppFromBaijimuCache
  Assert ($script:downloads -eq 1 -and $script:catalogReads -eq 1) 'Elevation must use exact prior artifact without network'
  Assert ($script:elevations -eq 1) 'Only explicit elevation may invoke UAC'
  $package = $script:PackageRecovery.packagePath
  [IO.File]::WriteAllText($package, 'tampered')
  Assert-Throws { Install-CodexAppFromBaijimuCache } 'SHA256'
  Assert ($script:elevations -eq 1) 'Tampered package must never reach elevation'
  $env:CODEX_INSTALL_ELEVATE = '0'
  Assert-Throws { Install-CodexAppFromBaijimuCache } 'MSIX_ELEVATION_REQUIRED'
  Assert ($script:downloads -eq 2) 'Ordinary retry must replace the tampered package'
  Remove-Item -LiteralPath $package
  $env:CODEX_INSTALL_ELEVATE = '1'
  Assert-Throws { Install-CodexAppFromBaijimuCache } '.*'
  Assert ($script:elevations -eq 1) 'Missing package must never reach elevation'

  # Run the actual elevation coordinator with a fake Windows process boundary.
  function Get-MsixInstallerUserSid { return 'S-1-5-21-123' }
  $script:processMode = 'success'
  function Start-Process {
    param($FilePath, $Verb, $ArgumentList, [switch]$Wait, [switch]$PassThru)
    Assert ($Verb -eq 'RunAs' -and $Wait -and $PassThru) 'Elevation must wait for the UAC process result'
    Assert ($ArgumentList -contains '-NoProfile') 'Elevated process must not load user profiles'
    if ($script:processMode -eq 'cancel') { throw (New-Object ComponentModel.Win32Exception(1223)) }
    if ($script:processMode -eq 'different-user') { return @{ ExitCode = 13 } }
    $decoded = [Text.Encoding]::Unicode.GetString([Convert]::FromBase64String($ArgumentList[-1]))
    $parseErrors = $null
    $child = [Management.Automation.Language.Parser]::ParseInput($decoded, [ref]$null, [ref]$parseErrors)
    Assert ($parseErrors.Count -eq 0) 'Elevated script must parse with quoted and Unicode paths'
    $assignments = $child.FindAll({param($node) $node -is [Management.Automation.Language.AssignmentStatementAst]}, $true)
    $resultAssignment = $assignments | Where-Object { $_.Left.Extent.Text -eq '$resultPath' } | Select-Object -First 1
    $pathAssignment = $assignments | Where-Object { $_.Left.Extent.Text -eq '$packagePath' } | Select-Object -First 1
    Assert ($pathAssignment.Right.Find({param($node) $node -is [Management.Automation.Language.StringConstantExpressionAst]}, $true).Value -eq $script:unusualPath) 'Package path must round trip as literal data'
    $responsePath = $resultAssignment.Right.Find({param($node) $node -is [Management.Automation.Language.StringConstantExpressionAst]}, $true).Value
    $ok = $script:processMode -eq 'success'
    [IO.File]::WriteAllText($responsePath, (@{ok=$ok; error='test installation error'} | ConvertTo-Json))
    return @{ ExitCode = $(if ($ok) { 0 } else { 1 }) }
  }
  $script:unusualPath = Join-Path $testRoot "用户's folder, __SID__ `$(Get-Item ignored).msix"
  & $realElevatedInstall $script:unusualPath $hash $testRoot
  $script:processMode = 'cancel'
  Assert-Throws { & $realElevatedInstall $script:unusualPath $hash $testRoot } 'MSIX_ELEVATION_CANCELLED'
  $script:processMode = 'different-user'
  Assert-Throws { & $realElevatedInstall $script:unusualPath $hash $testRoot } 'MSIX_ELEVATION_USER_MISMATCH'
  $script:processMode = 'failed'
  Assert-Throws { & $realElevatedInstall $script:unusualPath $hash $testRoot } 'test installation error'
  Assert (@(Get-ChildItem -LiteralPath $testRoot -Filter 'msix-elevation-*.json').Count -eq 0) 'Temporary results must be cleaned up'
  if ($env:OS -eq 'Windows_NT') {
    # Exercise the generated child in Windows PowerShell 5.1, mocking only AppX itself.
    # No UAC or real installation is triggered on the CI runner.
    $nativePackage = Join-Path $testRoot "测试 用户's.msix"
    Copy-Item -LiteralPath $seed -Destination $nativePackage
    $nativeResult = Join-Path $testRoot 'native-result.json'
    $nativeSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $childScript = Get-MsixElevationScript $nativePackage $hash $nativeSid $nativeResult
    $mockAppx = @'
function Add-AppxPackage {
  param($Path, $ErrorAction)
  $locked = $false
  try { [IO.File]::WriteAllText($Path, 'must not overwrite') } catch [IO.IOException] { $locked = $true }
  if (-not $locked) { throw 'Package was writable during installation' }
}
'@
    $nativeEncoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($mockAppx + "`n" + $childScript))
    & powershell.exe -NoProfile -NonInteractive -EncodedCommand $nativeEncoded
    Assert ($LASTEXITCODE -eq 0) 'Verified package child must succeed'
    $nativeResponse = Get-Content -Raw -Encoding UTF8 -LiteralPath $nativeResult | ConvertFrom-Json
    Assert ($nativeResponse.ok) 'Child must write a successful result'
    [IO.File]::WriteAllText($nativePackage, 'changed after parent verification')
    & powershell.exe -NoProfile -NonInteractive -EncodedCommand $nativeEncoded
    Assert ($LASTEXITCODE -eq 1) 'Child must reject changed package before AppX'
    $nativeResponse = Get-Content -Raw -Encoding UTF8 -LiteralPath $nativeResult | ConvertFrom-Json
    Assert (-not $nativeResponse.ok) 'Child must persist checksum failure'
    Remove-Item -LiteralPath $nativeResult
    $wrongUserScript = Get-MsixElevationScript $nativePackage $hash 'S-1-5-21-0' $nativeResult
    $wrongUserEncoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($wrongUserScript))
    & powershell.exe -NoProfile -NonInteractive -EncodedCommand $wrongUserEncoded
    Assert ($LASTEXITCODE -eq 13) 'Different-user elevation must stop before package access'
    Assert (-not (Test-Path -LiteralPath $nativeResult)) 'Different-user elevation must not write user data'
    $global:LASTEXITCODE = 0
  }
  Write-Host 'Windows MSIX recovery tests passed'
} finally {
  Remove-Item -LiteralPath $testRoot -Recurse -Force
}
