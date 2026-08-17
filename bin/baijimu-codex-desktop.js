#!/usr/bin/env node

import { existsSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const executable = process.platform === "win32" ? "baijimu-codex-desktop.exe" : "baijimu-codex-desktop";
const platform = process.platform === "darwin" ? "macos" : process.platform === "win32" ? "windows" : "linux";
const arch = process.arch === "arm64" ? "arm64" : "x86_64";
const candidates = [
  join(root, "bin", `${platform}-${arch}`, executable),
  join(root, "bin", platform, executable),
  join(root, "target", "release", executable),
  join(root, "target", "debug", executable),
];
const binary = candidates.find(existsSync);
if (!binary) {
  console.error(`未找到 Codex 桌面管理器原生程序；已检查：${candidates.join(", ")}`);
  process.exit(1);
}
const result = spawnSync(binary, process.argv.slice(2), {stdio: "inherit", env: process.env});
if (result.error) {
  console.error(result.error.message);
  process.exit(1);
}
process.exit(result.status ?? 1);
