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
  assert.match(workflow, /BAIJIMU_CLI_VERSION: "0\.1\.41"/);
  assert.match(workflow, /796a706b00e163429d2915010342e8569c6e5224466764a1e8efe3fbd772518b/);
  assert.match(workflow, /managed-tool-artifacts\/baijimu-cli\/releases\/v0\.1\.41/);
  assert.doesNotMatch(workflow, /bridge-agent\/releases/);
  assert.match(workflow, /git merge-base --is-ancestor "\$sha" origin\/main/);
  assert.match(workflow, /needs\.validate\.outputs\.verify == 'true'/);
  assert.match(workflow, /published-release\.json/);
  assert.match(workflow, /\.assets\[\] \| \[\.name, \.url\]/);
  assert.match(workflow, /RUSTFLAGS: "-C target-feature=\+crt-static -D warnings"/);
  assert.match(workflow, /\$PSNativeCommandUseErrorActionPreference = \$true/);
  assert.match(workflow, /Verify Windows binaries are self-contained/);
  assert.match(workflow, /dumpbin\.exe/);
  assert.match(workflow, /VCRUNTIME\|MSVCP/);

  for (const action of ["actions/checkout", "actions/upload-artifact", "actions/download-artifact"]) {
    const pattern = new RegExp(`${action.replace("/", "\\/")}@[0-9a-f]{40}`);
    assert.match(workflow, pattern);
  }
});

test("upstream sync is release-side, complete, immutable, and independently scheduled", async () => {
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
  assert.match(workflow, /OpenAI\\\.\(ChatGPT\|Codex\)/);
  assert.match(workflow, /needs: \[verify-macos-apps, verify-windows-apps\]/);
  assert.match(workflow, /sync-codex-artifacts\.sh/);
  assert.match(wrapper, /Customer installers read the published/);
  assert.match(synchronizer, /schema_version": 2/);
  assert.match(synchronizer, /assets\/sha256/);
  assert.match(synchronizer, /latest\.json/);
  assert.match(synchronizer, /Publishing this pointer last/);
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
        "platform": "macos",
        "arch": "aarch64" if "aarch64" in name else "x86_64",
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
    })
release = {"tag_name": "rust-v-test", "published_at": "2026-01-01T00:00:00Z"}
manifest = module.manifest_for(release, sources, "https://oss.example", "codex-artifacts")
module.validate_manifest(manifest)
assert manifest["schema_version"] == 2
assert len(manifest["assets"]) == 8
assert all("/assets/sha256/" in item["mirror_url"] for item in manifest["assets"])
assert not any("preserved_from_manifest" in item for item in manifest["assets"])
`;
  const result = spawnSync("python3", ["-c", script], {
    cwd: root,
    encoding: "utf8",
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

test("all package identities agree with the GitHub source tag", async () => {
  const cargo = await readFile(join(root, "Cargo.toml"), "utf8");
  const cargoLock = await readFile(join(root, "Cargo.lock"), "utf8");
  const packageJson = JSON.parse(await readFile(join(root, "package.json"), "utf8"));
  const packageLock = JSON.parse(await readFile(join(root, "package-lock.json"), "utf8"));
  const manifest = JSON.parse(await readFile(join(root, "connector.json"), "utf8"));
  const version = cargo.match(/^version = "([^"]+)"$/m)?.[1];
  assert.ok(version);
  assert.equal(cargoLock.match(/^name = "baijimu-connector-codex"\nversion = "([^"]+)"$/m)?.[1], version);
  assert.equal(packageJson.version, version);
  assert.equal(packageLock.version, version);
  assert.equal(packageLock.packages[""].version, version);
  assert.equal(manifest.version, version);
  assert.deepEqual(manifest.source, {
    type: "github",
    repo: "momoplan/baijimu-connector-codex",
    revision: `v${version}`,
  });
});
