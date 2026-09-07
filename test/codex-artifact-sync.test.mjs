import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { test } from "node:test";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

test("artifact sync promotes complete macOS Codex packages and deprecates legacy archives", () => {
  const program = String.raw`
import importlib.util
import json
import os
from pathlib import Path

path = Path(os.environ["SYNC_MODULE"])
spec = importlib.util.spec_from_file_location("sync_codex_artifacts", path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
release = {
    "assets": [
        {
            "name": definition["name"],
            "digest": "sha256:" + "a" * 64,
            "browser_download_url": "https://example.invalid/" + definition["name"],
            "size": 1,
            "content_type": "application/gzip",
        }
        for definition in module.CLI_ASSETS
    ]
}
selected = module.select_assets(release)
print(json.dumps([
    {
        "name": asset["name"],
        "platform": asset["platform"],
        "layout": asset.get("install_layout"),
        "deprecated": asset.get("deprecated"),
    }
    for asset in selected
    if asset["component"] == "codex_cli"
]))
`;
  const digest = "b".repeat(64);
  const result = spawnSync("python3", ["-c", program], {
    cwd: root,
    encoding: "utf8",
    env: {
      ...process.env,
      SYNC_MODULE: join(root, "tools/codex-artifacts/sync_codex_artifacts.py"),
      CODEX_APP_MACOS_ARM64_SHA256: digest,
      CODEX_APP_MACOS_X64_SHA256: digest,
      CODEX_APP_WINDOWS_ARM64_SHA256: digest,
      CODEX_APP_WINDOWS_X64_SHA256: digest,
      CODEX_APP_MACOS_ARM64_MINIMUM_OS_VERSION: "13.0",
      CODEX_APP_MACOS_X64_MINIMUM_OS_VERSION: "13.0",
      CODEX_APP_WINDOWS_ARM64_MINIMUM_OS_VERSION: "10.0.19041.0",
      CODEX_APP_WINDOWS_X64_MINIMUM_OS_VERSION: "10.0.19041.0",
    },
  });
  assert.equal(result.status, 0, result.stderr);
  const assets = JSON.parse(result.stdout);
  for (const arch of ["aarch64", "x86_64"]) {
    assert.deepEqual(
      assets.filter((asset) => asset.platform === "macos" && asset.name.includes(arch)),
      [
        {
          name: `codex-${arch}-apple-darwin.tar.gz`,
          platform: "macos",
          layout: "legacy_single_binary_archive",
          deprecated: true,
        },
        {
          name: `codex-package-${arch}-apple-darwin.tar.gz`,
          platform: "macos",
          layout: "codex_package_v1",
          deprecated: false,
        },
      ],
    );
  }
});
