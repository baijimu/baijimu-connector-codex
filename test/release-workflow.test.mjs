import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readdir, readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

test("connector owns its application release and upstream artifact sync workflows", async () => {
  const workflowDirectory = join(root, ".github", "workflows");
  const workflowFiles = (await readdir(workflowDirectory)).filter((name) =>
    /\.ya?ml$/.test(name),
  );
  assert.deepEqual(workflowFiles, [
    "release.yml",
    "sync-codex-upstream-artifacts.yml",
  ]);

  const workflow = await readFile(join(workflowDirectory, "release.yml"), "utf8");
  assert.match(workflow, /tags:\s*\n\s*- "v\*"/);
  assert.match(workflow, /github\.event_name == 'push' \|\| inputs\.publish/);
  assert.match(workflow, /jobs:\s*\n\s*validate:/);
  for (const job of [
    "build",
    "prepare-release",
    "publish-oss",
    "publish-release",
    "publish-market",
    "verify-published",
  ]) {
    assert.match(workflow, new RegExp(`\\n  ${job}:`));
  }
  for (const secret of [
    "APPLE_CERTIFICATE",
    "APPLE_CERTIFICATE_PASSWORD",
    "SSL_COM_USERNAME",
    "SSL_COM_PASSWORD",
    "SSL_COM_CREDENTIAL_ID",
    "SSL_COM_TOTP_SECRET",
    "OSS_ACCESS_KEY_ID",
    "OSS_ACCESS_KEY_SECRET",
    "LOCAL_APP_MARKET_PUBLISH_TOKEN",
  ]) {
    assert.match(workflow, new RegExp(`secrets\\.${secret}`));
  }
  assert.doesNotMatch(workflow, /codex-local-app-v/);
  assert.doesNotMatch(workflow, /release-codex-local-app/);
  assert.doesNotMatch(workflow, /Jenkins/i);
  assert.doesNotMatch(workflow, /gitee\.com|zxflimit_admin/);
  assert.match(workflow, /BAIJIMU_CLI_VERSION: "0\.1\.50"/);
  assert.match(workflow, /41b688119de97f48be68a7a33e5c60134464a32a19ec19bd5c120976cbdd5b14/);
  assert.match(workflow, /managed-tool-artifacts\/baijimu-cli\/releases\/v0\.1\.50/);
  assert.doesNotMatch(workflow, /bridge-agent\/releases/);
  assert.match(workflow, /git merge-base --is-ancestor "\$sha" origin\/main/);
  assert.match(workflow, /needs\.validate\.outputs\.verify == 'true'/);
  assert.match(workflow, /published-release\.json/);
  assert.match(workflow, /\.assets\[\] \| \[\.name, \.url\]/);
  assert.match(workflow, /RUSTFLAGS: "-C target-feature=\+crt-static -D warnings"/);
  assert.match(workflow, /\$PSNativeCommandUseErrorActionPreference = \$true/);
  assert.match(workflow, /Verify Windows binaries are self-contained/);
  assert.match(workflow, /Verify packaged Windows Connector lifecycle/);
  assert.match(workflow, /test-windows-connector-lifecycle\.ps1/);
  assert.match(workflow, /Verify installer atomic writes with Windows PowerShell 5\.1/);
  assert.match(workflow, /shell: powershell/);
  assert.match(
    workflow,
    /Verify installer atomic writes with Windows PowerShell 5\.1[\s\S]*?timeout-minutes: 5/,
  );
  assert.match(workflow, /test-windows-installer-atomic-write\.ps1/);
  assert.match(
    workflow,
    /Verify installer app-server profile protocol with Windows PowerShell 5\.1[\s\S]*?timeout-minutes: 5/,
  );
  assert.match(workflow, /test-windows-installer-app-server-login\.ps1/);
  assert.match(
    workflow,
    /Verify official Codex package layout with Windows PowerShell 5\.1[\s\S]*?timeout-minutes: 5/,
  );
  assert.match(workflow, /test-windows-installer-package-layout\.ps1/);
  assert.doesNotMatch(workflow, /Upload validated installer scripts/);
  assert.doesNotMatch(workflow, /Download validated installer scripts/);
  assert.match(workflow, /installers\\windows-configure-terminal-and-login\.ps1/);
  const windowsInstallerTest = await readFile(
    join(root, ".github", "scripts", "test-windows-installer-atomic-write.ps1"),
    "utf8",
  );
  assert.doesNotMatch(windowsInstallerTest, /Invoke-WebRequest/);
  assert.doesNotMatch(windowsInstallerTest, /curl\.exe|https?:\/\//);
  assert.match(windowsInstallerTest, /Parameter\(Mandatory = \$true\)/);
  assert.match(windowsInstallerTest, /Test-Path -LiteralPath \$ScriptPath -PathType Leaf/);
  const windowsLoginTest = await readFile(
    join(root, ".github", "scripts", "test-windows-installer-app-server-login.ps1"),
    "utf8",
  );
  const windowsPackageTest = await readFile(
    join(root, ".github", "scripts", "test-windows-installer-package-layout.ps1"),
    "utf8",
  );
  const windowsCliResolutionTest = await readFile(
    join(root, ".github", "scripts", "test-windows-installer-cli-resolution.ps1"),
    "utf8",
  );
  for (const windowsInstallerCheck of [
    windowsInstallerTest,
    windowsLoginTest,
    windowsPackageTest,
    windowsCliResolutionTest,
  ]) {
    assert.match(windowsInstallerCheck, /ReadAllText\(\$ScriptPath, \[System\.Text\.Encoding\]::UTF8\)/);
    assert.match(windowsInstallerCheck, /Parser\]::ParseInput\(/);
    assert.doesNotMatch(windowsInstallerCheck, /Parser\]::ParseFile\(/);
  }
  assert.doesNotMatch(windowsLoginTest, /Invoke-WebRequest|curl\.exe|https?:\/\//);
  assert.match(windowsLoginTest, /delayed-success/);
  assert.match(windowsLoginTest, /desktop\.localeOverride/);
  assert.match(windowsLoginTest, /locale-mismatch/);
  assert.doesNotMatch(windowsLoginTest, /windowsSandbox\//);
  assert.match(windowsLoginTest, /Start-Sleep -Seconds 3/);
  assert.match(windowsLoginTest, /denied by fake server/);
  assert.match(windowsLoginTest, /exposed the API key/);
  assert.match(windowsLoginTest, /\$script:Warnings\[0\] -notmatch "JSON"/);
  assert.match(windowsLoginTest, /\$script:Warnings\[0\] -notmatch "app-server"/);
  assert.doesNotMatch(windowsLoginTest, /[^\x00-\x7F]/);
  assert.doesNotMatch(windowsLoginTest, /-notmatch "non-JSON"/);
  assert.doesNotMatch(windowsPackageTest, /Invoke-WebRequest|curl\.exe|https?:\/\//);
  assert.match(windowsPackageTest, /Resolve-CodexPackageContents/);
  assert.match(windowsPackageTest, /codex-command-runner\.exe/);
  assert.match(windowsPackageTest, /incompleteError -notmatch "codex-command-runner/);
  assert.match(windowsPackageTest, /Legacy flat Windows Codex cache was not removed/);
  assert.match(windowsCliResolutionTest, /Warnings\[0\] -notmatch "codex --version"/);
  assert.doesNotMatch(windowsCliResolutionTest, /codex --version failed/);
  assert.doesNotMatch(windowsInstallerTest, /Get-FileHash -LiteralPath \$ScriptPath/);
  assert.match(workflow, /Verify embedded installer scripts/);
  assert.match(workflow, /installers\/macos-configure-terminal-and-login\.sh/);
  assert.match(workflow, /installers\/windows-configure-terminal-and-login\.ps1/);
  assert.match(workflow, /macOS native installer must remain a stateless action adapter/);
  assert.match(workflow, /installers\/upstream-artifact-source\.json/);
  assert.match(workflow, /New-Object System\.Text\.UTF8Encoding\(\$false\)/);
  assert.match(workflow, /Write-Utf8NoBomFile \$authPath/);
  assert.match(workflow, /Write-Utf8NoBomFile \$configPath/);
  assert.match(workflow, /Write-Utf8NoBomFile \$statePath/);
  assert.match(workflow, /ReadLineAsync\(\)/);
  assert.match(workflow, /account\/read API 密钥状态/);
  assert.match(workflow, /ConvertFrom-Json -ErrorAction Stop/);
  assert.match(workflow, /Set-Content\[\^\[:cntrl:\]\]\*\-Encoding/);
  assert.match(workflow, /dumpbin\.exe/);
  assert.match(workflow, /VCRUNTIME\|MSVCP/);
  const windowsLifecycleTest = await readFile(
    join(root, ".github", "scripts", "test-windows-connector-lifecycle.ps1"),
    "utf8",
  );
  assert.match(windowsLifecycleTest, /\/healthz/);
  assert.match(windowsLifecycleTest, /\/readyz/);
  assert.match(windowsLifecycleTest, /connector_initializing/);
  assert.match(windowsLifecycleTest, /connector_initialization_failed/);
  assert.match(windowsLifecycleTest, /CODEX_CONNECTOR_TEST_STARTUP_DELAY_MS/);
  assert.match(windowsLifecycleTest, /CODEX_CONNECTOR_TEST_STARTUP_FAILURE/);

  for (const action of ["actions/checkout", "actions/upload-artifact", "actions/download-artifact"]) {
    const pattern = new RegExp(`${action.replace("/", "\\/")}@[0-9a-f]{40}`);
    assert.match(workflow, pattern);
  }
});

test("connector compiles platform installers instead of downloading executable scripts", async () => {
  const setupSource = await readFile(join(root, "src", "setup.rs"), "utf8");
  const macosInstaller = await readFile(
    join(root, "installers", "macos-configure-terminal-and-login.sh"),
    "utf8",
  );
  const windowsInstaller = await readFile(
    join(root, "installers", "windows-configure-terminal-and-login.ps1"),
    "utf8",
  );

  assert.match(
    setupSource,
    /include_bytes!\("\.\.\/installers\/macos-configure-terminal-and-login\.sh"\)/,
  );
  assert.match(
    setupSource,
    /include_bytes!\("\.\.\/installers\/windows-configure-terminal-and-login\.ps1"\)/,
  );
  const artifactSource = await readFile(join(root, "src", "setup", "source.rs"), "utf8");
  assert.match(
    artifactSource,
    /include_bytes!\(\s*"\.\.\/\.\.\/installers\/upstream-artifact-source\.json"/,
  );
  assert.doesNotMatch(setupSource, /CODEX_CONNECTOR_INSTALL_SCRIPT_URL/);
  assert.doesNotMatch(setupSource, /SCRIPT_URL|SCRIPT_SHA256|download_script/);
  assert.ok(macosInstaller.length > 1_000);
  assert.ok(windowsInstaller.length > 1_000);
  assert.match(windowsInstaller, /desktop\.localeOverride/);
  assert.match(windowsInstaller, /Assert-CurrentWindowsVersion/);
  assert.match(windowsInstaller, /host_requirements\.minimum_os_version/);
  assert.match(windowsInstaller, /UNSUPPORTED_OS_VERSION/);
  assert.doesNotMatch(windowsInstaller, /windowsSandbox\//);
});

test("upstream sync is release-side, complete, latest-only, and independently scheduled", async () => {
  const workflow = await readFile(
    join(root, ".github", "workflows", "sync-codex-upstream-artifacts.yml"),
    "utf8",
  );
  const wrapper = await readFile(
    join(root, "tools", "codex-artifacts", "sync-codex-artifacts.sh"),
    "utf8",
  );
  const synchronizer = await readFile(
    join(root, "tools", "codex-artifacts", "sync_codex_artifacts.py"),
    "utf8",
  );

  assert.match(workflow, /schedule:/);
  assert.match(workflow, /workflow_dispatch:/);
  assert.doesNotMatch(workflow, /push:\s*\n\s*tags:/);
  assert.match(workflow, /concurrency:\s*\n\s*group: codex-upstream-artifact-sync/);
  assert.match(workflow, /verify-macos-apps:/);
  assert.match(workflow, /codesign --verify --deep --strict/);
  assert.match(workflow, /spctl --assess --type execute/);
  assert.match(workflow, /verify-windows-apps:/);
  assert.match(workflow, /signtool\.exe/);
  assert.match(workflow, /verify \/pa \/all \/v/);
  assert.match(workflow, /AppxManifest\.xml/);
  assert.match(workflow, /LSMinimumSystemVersion/);
  assert.match(workflow, /minimum_os_version/);
  assert.match(workflow, /OpenAI\\\.\(ChatGPT\|Codex\)/);
  assert.match(workflow, /needs: \[verify-macos-apps, verify-windows-apps\]/);
  assert.match(workflow, /sync-codex-artifacts\.sh/);
  assert.match(wrapper, /Customer installers read the published/);
  assert.match(synchronizer, /schema_version": 3/);
  assert.match(synchronizer, /manifest_v4_for/);
  assert.match(synchronizer, /host_requirements/);
  assert.match(synchronizer, /minimum_os_version/);
  assert.match(synchronizer, /assets\/sha256/);
  assert.match(synchronizer, /latest\.json/);
  assert.match(synchronizer, /Publishing this pointer last/);
  assert.match(synchronizer, /DEFAULT_BUCKET = "baijimu-lowcode-public-20260420"/);
  assert.match(synchronizer, /DEFAULT_PUBLIC_BASE = "https:\/\/download\.baijimu\.com"/);
  assert.match(
    synchronizer,
    /def public_asset_is_exact[\s\S]*?"--retry-all-errors"[\s\S]*?"--connect-timeout"[\s\S]*?"--max-time"/,
  );
  assert.match(workflow, /--connect-timeout 15 --max-time 900/);
  assert.match(synchronizer, /previous_keys - current_keys/);
  assert.doesNotMatch(synchronizer, /manifests\/sha256/);
  assert.doesNotMatch(synchronizer, /PRESERVE_EXISTING_MANIFEST/);
  for (const name of [
    "codex-app-aarch64-apple-darwin.dmg",
    "codex-app-x86_64-apple-darwin.dmg",
    "codex-app-windows-x64.msix",
    "codex-app-windows-arm64.msix",
    "codex-aarch64-apple-darwin.tar.gz",
    "codex-x86_64-apple-darwin.tar.gz",
    "codex-aarch64-pc-windows-msvc.exe.zip",
    "codex-x86_64-pc-windows-msvc.exe.zip",
    "codex-package-aarch64-pc-windows-msvc.tar.gz",
    "codex-package-x86_64-pc-windows-msvc.tar.gz",
  ]) {
    assert.match(synchronizer, new RegExp(name.replaceAll(".", "\\.")));
  }
});

test("upstream manifest builder produces one complete content-addressed snapshot", () => {
  const script = String.raw`
import importlib.util
from pathlib import Path

path = Path("tools/codex-artifacts/sync_codex_artifacts.py")
spec = importlib.util.spec_from_file_location("sync_codex_artifacts", path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

sources = []
for name in module.CLI_ASSET_NAMES:
    sources.append({
        "name": name,
        "component": "codex_cli",
        "platform": "windows" if "pc-windows" in name else "macos",
        "arch": "aarch64" if "aarch64" in name else "x86_64",
        "install_layout": "codex_package_v1" if "codex-package-" in name else ("legacy_flat_windows_archive" if "pc-windows" in name else "legacy_single_binary_archive"),
        "deprecated": name.endswith(".exe.zip"),
        "source_kind": "official_openai_github_release",
        "upstream_url": "https://example.invalid/" + name,
        "effective_upstream_url": "https://example.invalid/" + name,
        "upstream_sha256": "a" * 64,
        "sha256": "a" * 64,
        "size": 10,
        "content_type": "application/gzip",
    })
for source in module.APP_ASSETS:
    sources.append({
        **source,
        "component": "codex_desktop_app",
        "effective_upstream_url": source["upstream_url"],
        "upstream_sha256": "b" * 64,
        "sha256": "b" * 64,
        "size": 20,
        "signature_verification": "native-platform",
        "host_requirements": {"minimum_os_version": "14.0" if source["platform"] == "macos" else "10.0.19041.0"},
    })
release = {"tag_name": "rust-v-test", "published_at": "2026-01-01T00:00:00Z"}
legacy_manifest = module.manifest_for(release, sources, "https://oss.example", "codex-artifacts")
manifest = module.manifest_v4_for(release, sources, "https://oss.example", "codex-artifacts")
module.validate_manifest(legacy_manifest)
module.validate_manifest(manifest)
assert legacy_manifest["schema_version"] == 3
assert manifest["schema_version"] == 4
assert len(manifest["assets"]) == 10
assert len([item for item in manifest["assets"] if item["install_layout"] == "codex_package_v1"]) == 2
assert all("/assets/sha256/" in item["mirror_url"] for item in manifest["assets"])
assert all(item["host_requirements"]["minimum_os_version"] for item in manifest["assets"] if item["component"] == "codex_desktop_app")
assert all(item["host_requirements"] is None for item in manifest["assets"] if item["component"] == "codex_cli")
assert not any("preserved_from_manifest" in item for item in manifest["assets"])
desktop = next(item for item in manifest["assets"] if item["component"] == "codex_desktop_app")
desktop["host_requirements"]["minimum_os_version"] = "14 beta"
try:
    module.validate_manifest(manifest)
except RuntimeError:
    pass
else:
    raise AssertionError("malformed minimum OS versions must fail closed")
`;
  const result = spawnSync("python3", ["-c", script], {
    cwd: root,
    encoding: "utf8",
    env: { ...process.env, PYTHONDONTWRITEBYTECODE: "1" },
  });
  assert.equal(result.status, 0, result.stderr || result.stdout);
});

test("market publisher uses explicit immutable version creation and review submission", async () => {
  const script = await readFile(
    join(root, "tools", "release", "publish-market.sh"),
    "utf8",
  );
  assert.match(script, /local-app version create codex/);
  assert.match(script, /local-app submit codex "\$version"/);
  assert.doesNotMatch(script, /local-app publish codex/);
  assert.match(script, /momoplan\/baijimu-connector-codex/);
  assert.doesNotMatch(script, /codex-local-app-v|gitee\.com|zxflimit_admin/);
});

test("setup treats desktop launch as a post-configuration convenience", async () => {
  const setup = await readFile(join(root, "src", "setup.rs"), "utf8");
  const desktop = await readFile(join(root, "src", "desktop.rs"), "utf8");
  const main = await readFile(join(root, "src", "main.rs"), "utf8");
  const installers = await Promise.all([
    readFile(join(root, "installers", "macos-configure-terminal-and-login.sh"), "utf8"),
    readFile(join(root, "installers", "windows-configure-terminal-and-login.ps1"), "utf8"),
  ]);
  const userEnvironment = await readFile(join(root, "src", "user_environment.rs"), "utf8");
  const activationIndex = setup.indexOf("credential::finalize_workspace_setup");
  const launchIndex = setup.indexOf("crate::desktop::launch_and_verify(profile_home)");
  const compatibilityModule = await readFile(
    join(root, "src", "system_compatibility.rs"),
    "utf8",
  );
  const macosSetup = await readFile(join(root, "src", "setup", "macos.rs"), "utf8");

  assert.ok(activationIndex >= 0, "setup must finalize the selected profile");
  assert.ok(launchIndex > activationIndex, "desktop launch must follow profile finalization");
  assert.match(setup, /SetupOutcome::from_desktop_launch/);
  assert.match(setup, /UNSUPPORTED_OS_VERSION/);
  assert.match(setup, /retryable: false/);
  assert.match(compatibilityModule, /pub const ERROR_CODE_UNSUPPORTED_OS_VERSION/);
  assert.match(macosSetup, /ensure_current_macos_supported\(\)\?/);
  assert.match(setup, /手动找到并打开 ChatGPT/);
  assert.doesNotMatch(setup, /launch_desktop_after_setup\(&profile_home\)\?/);
  assert.match(setup, /active_workspace_id == Some\(workspace_id\)/);
  assert.match(setup, /workspace_profile_is_active\.then_some\(profile_home\)/);
  assert.match(setup, /Ok\(match activated_profile_home/);
  assert.doesNotMatch([setup, ...installers].join("\n"), /CODEX_INSTALL_[A-Z_]*DESKTOP/);
  assert.doesNotMatch(installers.join("\n"), /Test-VisibleWindow|verify_codex_window/);
  assert.doesNotMatch(main, /reconcile_active_user_codex_home/);
  assert.match(main, /desktop::launch_and_verify\(&selected_home\)/);
  assert.match(main, /restart_and_verify\(&previous_home\)/);
  assert.doesNotMatch(main, /user_codex_home_synchronized/);
  assert.match(setup, /"-EncodedCommand"/);
  assert.match(setup, /"-OutputFormat"/);
  assert.match(setup, /"Text"/);
  assert.match(setup, /\[Console\]::OutputEncoding/);
  assert.match(desktop, /Start-Process -FilePath \$entry\[0\]\.executable/);
  assert.doesNotMatch(desktop, /shell:AppsFolder/);
  assert.match(desktop, /Get-StartApps/);
  assert.match(desktop, /PackageFamilyName/);
  assert.match(desktop, /Windows\.FullTrustApplication/);
  assert.match(desktop, /BaijimuCodexVisibleWindowProbe/);
  assert.match(desktop, /visibleWindowCount/);
  assert.match(desktop, /隔离启动桌面应用时必须显式提供 CODEX_HOME/);
  assert.match(desktop, /isolate_from_connector_environment/);
  assert.match(userEnvironment, /pub fn restore_codex_home/);
  assert.doesNotMatch(userEnvironment, /pub fn activate_codex_home/);
  assert.match(desktop, /未在 45 秒内启动进程/);
  assert.match(desktop, /未在 45 秒内显示可见窗口/);
  assert.doesNotMatch(desktop, /MainWindowHandle/);
  assert.match(desktop, /\/Applications\/ChatGPT\.app/);
  assert.match(desktop, /Command::new\("\/usr\/bin\/open"\)/);
  assert.match(desktop, /command\.arg\("--env"\)\.arg\(assignment\)/);
  assert.match(desktop, /OsString::from\("CODEX_HOME="\)/);
  assert.match(desktop, /Command::new\("\/usr\/bin\/lsappinfo"\)/);
  assert.match(desktop, /has_running_process\(&info\)/);
  assert.match(desktop, /tell application id .* to quit/);
  assert.match(desktop, /Command::new\("\/bin\/ps"\)/);
  assert.match(desktop, /没有使用所选工作区状态目录/);
  assert.doesNotMatch(desktop, /PROJECT_REOPEN_DELAY|reopen_with_project|Documents.*Codex.*default/);
  assert.doesNotMatch(desktop, /pkill/);
});

test("default Codex home has one workspace binding and other profiles stay safely isolated", async () => {
  const credential = await readFile(join(root, "src", "credential.rs"), "utf8");
  const main = await readFile(join(root, "src", "main.rs"), "utf8");
  assert.match(credential, /home_dir\(\)\.join\("\.baijimu"\)\.join\("codex"\)\.join\("p"\)/);
  assert.match(credential, /Sha256::digest\(profile_id\.as_bytes\(\)\)/);
  assert.match(credential, /digest\[\.\.24\]\.to_string\(\)/);
  assert.match(credential, /migrate_legacy_profile_homes/);
  assert.match(credential, /fs::rename\(&source, &target\)/);
  assert.match(credential, /源目录和目标目录同时存在/);
  assert.match(credential, /a_new_user_binds_the_first_workspace_to_the_default_codex_home/);
  assert.match(credential, /an_existing_unowned_default_home_keeps_new_workspace_isolated/);
  assert.match(credential, /a_second_workspace_cannot_take_the_default_home_binding/);
  assert.match(credential, /switching_back_to_bound_workspace_restores_default_codex_home/);
  assert.match(credential, /v5_active_private_profile_migrates_to_default_home_and_commits_binding/);
  assert.match(credential, /v4_active_legacy_profile_is_atomically_migrated_to_the_default_home/);
  assert.match(credential, /legacy_profile_migration_recovers_after_rename_before_metadata_save/);
  assert.match(credential, /legacy_profile_migration_preserves_both_directories_on_collision/);
  const migrationDetection = main.indexOf("credential::pending_profile_home_migration()");
  const desktopStop = main.indexOf("desktop::stop_for_codex_home_switch()", migrationDetection);
  const metadataMigration = main.indexOf("match credential::state()", desktopStop);
  assert.ok(migrationDetection >= 0);
  assert.ok(desktopStop > migrationDetection);
  assert.ok(metadataMigration > desktopStop);
  assert.match(main, /thread::spawn\(move \|\| match initialize_server\(\)/);
  assert.match(credential, /default_home_can_bind_profile/);
  assert.match(credential, /OWNERSHIP_MARKER_FILE: &str = "\.baijimu-owner\.json"/);
  assert.match(credential, /OWNERSHIP_RESERVATION_FILE: &str = "\.baijimu-owner\.pending\.json"/);
  assert.match(credential, /OWNERSHIP_OWNER: &str = "baijimu-connector-codex"/);
  assert.match(credential, /read_valid_ownership/);
  assert.match(credential, /commit_default_home_ownership/);
  assert.match(credential, /managed_files: vec!\[OWNED_AUTH_FILE\.to_string\(\), OWNED_CONFIG_FILE\.to_string\(\)\]/);
  assert.match(credential, /"workspaceId",[\s\S]*?"workspaceName",[\s\S]*?"userId",[\s\S]*?"clientId",[\s\S]*?"environment",[\s\S]*?"profileId"/);
  assert.match(credential, /assert!\(!marker_content\.contains\(&profile\.profile_id\)\)/);
  assert.match(credential, /assert!\(!marker_content\.contains\("workspace-token"\)\)/);
  assert.match(credential, /default_home_ownership_marker_binds_a_profile_without_business_identifiers/);
});

test("all package identities agree with the GitHub source tag", async () => {
  const cargo = await readFile(join(root, "Cargo.toml"), "utf8");
  const cargoLock = await readFile(join(root, "Cargo.lock"), "utf8");
  const packageJson = JSON.parse(await readFile(join(root, "package.json"), "utf8"));
  const packageLock = JSON.parse(await readFile(join(root, "package-lock.json"), "utf8"));
  const nodeLauncher = await readFile(
    join(root, "bin", "baijimu-connector-codex.js"),
    "utf8",
  );
  const manifest = JSON.parse(await readFile(join(root, "connector.json"), "utf8"));
  const version = cargo.match(/^version = "([^"]+)"$/m)?.[1];
  assert.ok(version);
  assert.equal(cargoLock.match(/^name = "baijimu-connector-codex"\nversion = "([^"]+)"$/m)?.[1], version);
  assert.equal(packageJson.version, version);
  assert.equal(packageLock.version, version);
  assert.equal(packageLock.packages[""].version, version);
  assert.match(nodeLauncher, new RegExp(`const VERSION = "${version.replaceAll(".", "\\.")}";`));
  assert.equal(manifest.version, version);
  assert.deepEqual(manifest.source, {
    type: "github",
    repo: "momoplan/baijimu-connector-codex",
    revision: `v${version}`,
  });
});
