import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const read = (path) => readFile(join(root, path), "utf8");

test("desktop manager manifest has no Relay capability surface", async () => {
  const manifest = JSON.parse(await read("connector.json"));
  assert.equal(manifest.id, "com.baijimu.connector.codex");
  assert.equal(manifest.version, "1.3.1");
  assert.equal(manifest.source.repo, "momoplan/baijimu-connector-codex");
  assert.equal(manifest.runtime.healthCheck.url, "http://127.0.0.1:18110/healthz");
  assert.equal(manifest.transport.baseUrl, "http://127.0.0.1:18110");
  for (const field of ["remoteCapabilities", "methods", "events"]) {
    assert.equal(manifest[field], undefined);
  }
  assert.ok(manifest.management.operations.launchCodex);
});

test("desktop manager neither installs CLI nor exposes invoke routes", async () => {
  const [main, setup, macos, source] = await Promise.all([
    read("src/main.rs"),
    read("src/setup.rs"),
    read("src/setup/macos.rs"),
    read("installers/upstream-artifact-source.json"),
  ]);
  assert.doesNotMatch(main, /mod app_server/);
  assert.doesNotMatch(main, /path\.starts_with\("\/invoke\/"\)/);
  assert.doesNotMatch(setup, /codex_binary::resolve/);
  const execute = macos.slice(macos.indexOf("fn execute"), macos.indexOf("fn ensure_desktop_app"));
  assert.match(execute, /ensure_desktop_app/);
  assert.doesNotMatch(execute, /ensure_codex_cli/);
  assert.match(source, /codex-artifacts\/v4/);
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
