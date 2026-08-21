import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const read = (path) => readFile(join(root, path), "utf8");

test("desktop manager exposes only its required read-only status method", async () => {
  const [manifest, packageManifest] = await Promise.all([
    read("connector.json").then(JSON.parse),
    read("package.json").then(JSON.parse),
  ]);
  assert.equal(manifest.id, "com.baijimu.connector.codex");
  assert.equal(manifest.name, "Codex 桌面管理器");
  assert.equal(manifest.version, packageManifest.version);
  assert.equal(manifest.source.revision, `v${packageManifest.version}`);
  assert.equal(manifest.source.repo, "baijimu/baijimu-connector-codex");
  assert.equal(manifest.runtime.healthCheck.url, "http://127.0.0.1:18110/readyz");
  assert.equal(manifest.transport.baseUrl, "http://127.0.0.1:18110");
  for (const field of ["remoteCapabilities", "events"]) {
    assert.equal(manifest[field], undefined);
  }
  assert.deepEqual(
    manifest.methods.map(({ name, path, httpMethod }) => ({ name, path, httpMethod })),
    [{ name: "status", path: "/healthz", httpMethod: "GET" }],
  );
  assert.ok(manifest.management.operations.launchCodex);
  assert.deepEqual(manifest.management.operations.initializeWorkspace, {
    method: "POST",
    path: "/management/v1/codex/initialize",
  });
  assert.deepEqual(manifest.management.operations.reauthorizeWorkspace, {
    method: "POST",
    path: "/management/v1/codex/reauthorize",
  });
  assert.deepEqual(manifest.management.operations.verifyRouter, {
    method: "POST",
    path: "/management/v1/setup/verify-router",
  });
});

test("desktop manager neither installs CLI nor exposes invoke routes", async () => {
  const [main, setup, macos, contract, windows, macosScript, source] = await Promise.all([
    read("src/main.rs"),
    read("src/setup.rs"),
    read("src/setup/macos.rs"),
    read("src/setup/contract.rs"),
    read("installers/windows-configure-terminal-and-login.ps1"),
    read("installers/macos-configure-terminal-and-login.sh"),
    read("installers/upstream-artifact-source.json"),
  ]);
  assert.doesNotMatch(main, /mod app_server/);
  assert.doesNotMatch(main, /path\.starts_with\("\/invoke\/"\)/);
  assert.doesNotMatch(setup, /codex_binary::resolve/);
  const execute = macos.slice(macos.indexOf("fn execute"), macos.indexOf("fn ensure_desktop_app"));
  assert.match(execute, /ensure_desktop_app/);
  assert.doesNotMatch(execute, /ensure_codex_cli/);
  assert.match(source, /codex-artifacts\/v4\/desktop\/latest\.json/);
  assert.doesNotMatch(contract, /cli_installed|cli_install_method|cli_path|cli_version|cli_smoke/);
  assert.doesNotMatch(windows, /Resolve-CodexCli|Invoke-AppServerProfileSetup|Test-CodexCli|CODEX_DESKTOP_ONLY|CODEX_CLI_BIN|cliInstallMethod|cliArtifact|codexExe/);
  assert.doesNotMatch(windows, /Invoke-WebRequest[^\n]+\/responses|function Test-Router/);
  assert.doesNotMatch(windows, /RouterBaseUrl|New-BaijimuLlmCredential|ConvertTo-CodexConfigContent|Write-CodexConfig|CODEX_LLM_CREDENTIAL_FILE/);
  assert.doesNotMatch(windows, /gpt-5\.6-sol|baijimu-router|base_url\s*=/);
  assert.match(windows, /function Confirm-CodexConfiguration/);
  assert.match(windows, /Rust 凭证管理器未生成 Codex 授权文件/);
  assert.match(windows, /Rust 凭证管理器未生成 Codex 配置文件/);
  assert.doesNotMatch(setup, /CODEX_LLM_CREDENTIAL_FILE|credential-\{unique\}/);
  assert.match(setup, /credential::initialize_workspace_profile\(workspace_id\)/);
  assert.match(setup, /prepared\.credential/);
  assert.match(setup, /mod router;/);
  assert.doesNotMatch(macosScript, /install_cli|install-cli/);
});

test("desktop manager upgrades its inherited metadata in place and omits redundant cards", async () => {
  const [store, html] = await Promise.all([
    read("src/credential/store.rs"),
    read("ui/index.html"),
  ]);
  assert.doesNotMatch(store, /legacy_connector_metadata_path/);
  assert.doesNotMatch(html, /百积木接入/);
  assert.doesNotMatch(html, /当前生效/);
});

test("Windows runtime uses codex protocol activation without AppX discovery", async () => {
  const [desktop, installer] = await Promise.all([
    read("src/desktop.rs"),
    read("installers/windows-configure-terminal-and-login.ps1"),
  ]);
  const desktopPreamble = desktop.slice(
    desktop.indexOf("const POWERSHELL_PREAMBLE"),
    desktop.indexOf("const STOP_SCRIPT"),
  );
  const desktopLaunch = desktop.slice(
    desktop.indexOf("const LAUNCH_SCRIPT"),
    desktop.indexOf("pub fn stop_for_workspace_switch", desktop.indexOf("const LAUNCH_SCRIPT")),
  );
  const desktopWindowsRuntime = desktop.slice(
    desktop.indexOf("const POWERSHELL_PREAMBLE"),
    desktop.indexOf("#[cfg(test)]", desktop.indexOf("const POWERSHELL_PREAMBLE")),
  );
  assert.match(installer, /local-name\(\)='Protocol'/);
  assert.match(installer, /CODEX_DESKTOP_PROTOCOL/);
  assert.match(installer, /CODEX_DESKTOP_TRUSTED_PUBLISHERS/);
  assert.doesNotMatch(installer, /CODEX_DESKTOP_TRUSTED_PUBLISHERS_JSON/);
  assert.match(installer, /\$identity\[0\]\.Publisher/);
  assert.match(installer, /Windows\.FullTrustApplication/);
  assert.doesNotMatch(installer, /OpenAI\.ChatGPT-Desktop/);
  assert.doesNotMatch(installer, /Get-AppxPackage -Name/);
  assert.match(desktopPreamble, /System\.Text\.UTF8Encoding\(\$false\)/);
  assert.match(desktopPreamble, /CODEX_DESKTOP_PROCESS_NAMES/);
  assert.match(desktopPreamble, /CODEX_DESKTOP_TRUSTED_SIGNER_SUBJECTS/);
  assert.match(desktopPreamble, /Get-Process -Name \$_/);
  assert.match(desktopPreamble, /Get-AuthenticodeSignature/);
  assert.match(desktopPreamble, /SignatureStatus\]::Valid/);
  assert.doesNotMatch(desktopWindowsRuntime, /Get-AppxPackage|AppxManifest\.xml|PackageFullName/);
  assert.match(installer, /Get-AppxPackage -Publisher \$_/);
  assert.doesNotMatch(installer, /Get-AppxPackage -ErrorAction/);
  assert.doesNotMatch(desktopPreamble, /IApplicationActivationManager|ActivateApplication/);
  assert.doesNotMatch(desktopPreamble, /CurrentUser\.CreateSubKey|SetValue\(|DeleteValue\(/);
  assert.doesNotMatch(desktop, /SendMessageTimeout|WM_SETTINGCHANGE|BroadcastEnvironmentChange/);
  assert.doesNotMatch(desktopPreamble, /BAIJIMU_AUTH_FILE/);
  assert.doesNotMatch(desktopPreamble, /BAIJIMU_CURRENT_WORKSPACE_ID/);
  assert.doesNotMatch(desktopPreamble, /Grant-BaijimuCliAuthStoreReadAccess/);
  assert.doesNotMatch(desktopPreamble, /CodexSandbox|icacls\.exe/);
  assert.match(desktopLaunch, /Start-Process -FilePath "\$\{codexDesktopProtocol\}:"/);
  assert.match(desktopLaunch, /Get-CodexDesktopProcesses/);
  assert.match(desktopLaunch, /activationAccepted = \$true/);
  assert.doesNotMatch(desktopLaunch, /VisibleWindow|EnumWindows|IsWindowVisible|AddSeconds\(45\)/);
  assert.doesNotMatch(desktopLaunch, /CODEX_HOME|appUserModelId/);
});

test("desktop launch only activates an existing profile and starts the desktop app", async () => {
  const [main, desktop, credential] = await Promise.all([
    read("src/main.rs"),
    read("src/desktop.rs"),
    read("src/credential.rs"),
  ]);
  assert.doesNotMatch(desktop, /launch_and_verify|restart_and_verify|has_visible_window/);
  assert.doesNotMatch(main, /active_home_snapshot|restore_active_home|启动验证失败|状态指针回滚/);
  assert.doesNotMatch(credential, /pub fn active_home_snapshot|pub fn restore_active_home/);
  assert.match(main, /desktop::launch\(\)/);
  const launchRouteStart = main.indexOf('(\"POST\", \"/management/v1/codex/launch\")');
  const launchRoute = main.slice(
    launchRouteStart,
    main.indexOf('_ => Err(HttpError::new(404', launchRouteStart),
  );
  assert.doesNotMatch(launchRoute, /verify_system_compatibility|stop_for_codex_home_switch/);
  assert.match(launchRoute, /stop_for_workspace_switch/);
  assert.match(launchRoute, /prepare_workspace_activation/);
  assert.match(launchRoute, /activate_prepared_workspace_profile/);
  assert.doesNotMatch(
    launchRoute,
    /initialize_workspace_profile|prepare_workspace_reauthorization|create_llm_credential|write_workspace_auth|write_workspace_config/,
  );
  const stopIndex = launchRoute.indexOf("desktop::stop_for_workspace_switch()");
  const credentialIndex = launchRoute.indexOf("credential::activate_prepared_workspace_profile(&prepared)");
  const launchIndex = launchRoute.indexOf("desktop::launch()");
  assert.ok(stopIndex >= 0 && credentialIndex > stopIndex && launchIndex > credentialIndex);
  assert.doesNotMatch(credential, /PERMISSION_MODE_VISIBILITY_KEY|desktop_defaults_version/);
});

test("initialization preserves shared state and reauthorization only rotates credentials", async () => {
  const [main, credential, app] = await Promise.all([
    read("src/main.rs"),
    read("src/credential.rs"),
    read("ui/app.js"),
  ]);
  assert.match(credential, /initialize_workspace_files/);
  assert.match(credential, /ensure_workspace_config/);
  assert.match(credential, /profile_credential_path/);
  assert.match(credential, /sync_credential_to_shared_home/);
  assert.match(credential, /migrate_profiles_to_shared_home/);
  assert.doesNotMatch(credential.slice(0, credential.indexOf("mod legacy_profile_home_tests")), /fs::rename\(&source, &target\)/);
  assert.match(credential, /commit_workspace_reauthorization/);
  const reauthorizeStart = credential.indexOf("pub fn commit_workspace_reauthorization");
  const reauthorize = credential.slice(
    reauthorizeStart,
    credential.indexOf("fn authorized_workspace", reauthorizeStart),
  );
  assert.match(reauthorize, /write_workspace_auth/);
  assert.doesNotMatch(reauthorize, /ensure_workspace_config/);
  assert.match(main, /\/management\/v1\/codex\/initialize/);
  assert.match(main, /\/management\/v1\/codex\/reauthorize/);
  assert.match(app, /label: "重新授权"/);
  assert.match(app, /label: "初始化"/);
});
