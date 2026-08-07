import assert from "node:assert/strict";
import { readdir, readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

test("connector owns one self-contained GitHub release workflow", async () => {
  const workflowDirectory = join(root, ".github", "workflows");
  const workflowFiles = (await readdir(workflowDirectory)).filter((name) =>
    /\.ya?ml$/.test(name),
  );
  assert.deepEqual(workflowFiles, ["release.yml"]);

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
  assert.match(workflow, /baijimu-cli-v\$\{BAIJIMU_CLI_VERSION\}/);
  assert.match(workflow, /ea97f240485a2d85bc866d486d2480c71bd22c12d359ad248d2b246ff371499e/);
  assert.match(workflow, /git merge-base --is-ancestor "\$sha" origin\/main/);
  assert.match(workflow, /needs\.validate\.outputs\.verify == 'true'/);
  assert.match(workflow, /published-release\.json/);
  assert.match(workflow, /\.assets\[\] \| \[\.name, \.url\]/);
  assert.match(workflow, /RUSTFLAGS: "-C target-feature=\+crt-static -D warnings"/);
  assert.match(workflow, /Verify Windows binaries are self-contained/);
  assert.match(workflow, /dumpbin\.exe/);
  assert.match(workflow, /VCRUNTIME\|MSVCP/);

  for (const action of ["actions/checkout", "actions/upload-artifact", "actions/download-artifact"]) {
    const pattern = new RegExp(`${action.replace("/", "\\/")}@[0-9a-f]{40}`);
    assert.match(workflow, pattern);
  }
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
