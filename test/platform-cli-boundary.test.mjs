import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

test("Baijimu CLI exclusively owns platform authentication and Partner API calls", async () => {
  const [credential, platformCli, desktop] = await Promise.all([
    readFile(join(root, "src", "credential.rs"), "utf8"),
    readFile(join(root, "src", "baijimu_cli.rs"), "utf8"),
    readFile(join(root, "src", "desktop.rs"), "utf8"),
  ]);

  for (const forbidden of [
    "load_shared_credential_store",
    "select_local_machine_credential",
    "post_baijimu_json",
    "validateCredential",
    "bearer_auth",
    "machineCredentials",
    "lc_pat_",
  ]) {
    assert.doesNotMatch(credential, new RegExp(forbidden));
  }
  assert.doesNotMatch(
    credential,
    /llm-credential\/partner\/v1|partner\/v1\/workspaces/,
  );

  assert.match(platformCli, /\["auth", "status"\]/);
  assert.match(platformCli, /"workspace",\s*"list"/);
  assert.match(platformCli, /"workspace", "get"/);
  assert.match(platformCli, /"llm-credential",\s*"create"/);
  assert.match(platformCli, /"--json"/);
  assert.match(platformCli, /"--show-secret"/);
  assert.doesNotMatch(platformCli, /pub\s+shared_auth_path\s*:/);
  const platformCliProduction = platformCli.slice(0, platformCli.indexOf("#[cfg(test)]"));
  assert.doesNotMatch(
    platformCliProduction,
    /sharedAuthPath|legacy_shared_auth_path_for_acl_cleanup/,
  );
  const desktopProduction = desktop.slice(0, desktop.indexOf("#[cfg(test)]"));
  assert.doesNotMatch(
    desktopProduction,
    /BAIJIMU_AUTH_FILE|BAIJIMU_CURRENT_WORKSPACE_ID|BaijimuAuthFile|BaijimuCurrentWorkspaceId|\$baijimuAuthFile|\$baijimuCurrentWorkspaceId|icacls\.exe/,
  );
  assert.doesNotMatch(desktopProduction, /CodexSandbox|setup_marker|RemoveAccessRule/);
  assert.doesNotMatch(
    platformCli,
    /reqwest|bearer_auth|fs::read|fs::read_to_string/,
  );
});
