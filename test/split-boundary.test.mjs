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
  assert.equal(manifest.source.repo, "momoplan/baijimu-connector-codex");
  assert.equal(manifest.runtime.healthCheck.url, "http://127.0.0.1:18110/healthz");
  assert.equal(manifest.transport.baseUrl, "http://127.0.0.1:18110");
  for (const field of ["remoteCapabilities", "events"]) {
    assert.equal(manifest[field], undefined);
  }
  assert.deepEqual(
    manifest.methods.map(({ name, path, httpMethod }) => ({ name, path, httpMethod })),
    [{ name: "status", path: "/healthz", httpMethod: "GET" }],
  );
  assert.ok(manifest.management.operations.launchCodex);
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
  assert.match(setup, /credential::prepare_workspace_profile\(workspace_id\)/);
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

test("Windows desktop discovery follows the codex protocol instead of package names", async () => {
  const [desktop, installer] = await Promise.all([
    read("src/desktop.rs"),
    read("installers/windows-configure-terminal-and-login.ps1"),
  ]);
  const desktopPreamble = desktop.slice(
    desktop.indexOf("const POWERSHELL_PREAMBLE"),
    desktop.indexOf("const STOP_SCRIPT"),
  );
  const desktopLaunch = desktop.slice(
    desktop.indexOf("const LAUNCH_AND_VERIFY_SCRIPT"),
    desktop.indexOf("const COMPATIBILITY_SCRIPT"),
  );
  for (const source of [desktopPreamble, installer]) {
    assert.match(source, /local-name\(\)='Protocol'/);
    assert.match(source, /CODEX_DESKTOP_PROTOCOL/);
    assert.match(source, /CODEX_DESKTOP_TRUSTED_PUBLISHERS/);
    assert.doesNotMatch(source, /CODEX_DESKTOP_TRUSTED_PUBLISHERS_JSON/);
    assert.match(source, /publisher = \[string\]\$identity\[0\]\.Publisher/);
    assert.match(source, /Windows\.FullTrustApplication/);
    assert.doesNotMatch(source, /OpenAI\.ChatGPT-Desktop/);
    assert.doesNotMatch(source, /Get-AppxPackage -Name/);
  }
  assert.match(desktopPreamble, /System\.Text\.UTF8Encoding\(\$false\)/);
  assert.match(desktopPreamble, /IApplicationActivationManager/);
  assert.match(desktopPreamble, /ActivateApplication/);
  assert.match(desktopPreamble, /DoNotExpandEnvironmentNames/);
  assert.match(desktopPreamble, /GetValueKind\('CODEX_HOME'\)/);
  assert.match(desktopPreamble, /DeleteValue\('CODEX_HOME', \$false\)/);
  assert.match(desktopPreamble, /BroadcastEnvironmentChange/);
  assert.match(desktopLaunch, /Invoke-CodexDesktopActivation/);
  assert.doesNotMatch(desktopLaunch, /Start-Process/);
});
