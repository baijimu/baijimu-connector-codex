import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import {
  credentialStatusMeta,
  normalizeCredentialState,
  normalizeCodexSessions,
  normalizeSetupProgress,
  shouldShowSetupProgress,
  codexTurnMessages,
} from "../ui/state.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");

test("connector manifest declares the packaged embedded UI", async () => {
  const manifest = JSON.parse(await readFile(join(root, "connector.json"), "utf8"));
  const packageManifest = JSON.parse(
    await readFile(join(root, "package.json"), "utf8"),
  );
  assert.equal(manifest.schemaVersion, "2.0");
  assert.equal(manifest.version, packageManifest.version);
  assert.equal(manifest.version, "1.2.20");
  assert.equal(manifest.source.type, "github");
  assert.equal(manifest.source.repo, "momoplan/baijimu-connector-codex");
  assert.equal(manifest.source.revision, `v${manifest.version}`);
  assert.equal(manifest.transport.type, "http");
  assert.ok(manifest.methods.some((method) => method.name === "status"));
  assert.ok(manifest.events.some((event) => event.name === "codexNotification"));
  assert.equal(manifest.services, undefined);
  assert.equal(manifest.serviceRegistrationFiles, undefined);
  assert.deepEqual(manifest.runtime.args, ["start"]);
  assert.deepEqual(manifest.runtime.stopArgs, ["stop"]);
  assert.equal(manifest.runtime.processOwnership, "host");
  assert.deepEqual(manifest.ui, {
    type: "embedded",
    entry: "ui/index.html",
    title: "Codex 远程开发",
    defaultView: true,
  });
  assert.deepEqual(Object.keys(manifest.management.operations).sort(), [
    "checkoutPlatformProject",
    "credentialState",
    "ensureCodexReady",
    "interruptCodexTurn",
    "listCodexProjects",
    "listCodexSessions",
    "listCodexTurns",
    "readCodexSession",
    "recentCodexEvents",
    "setupRetry",
    "setupState",
    "startCodexSession",
    "startCodexTurn",
    "switchAuthProfile",
  ]);
  assert.equal(manifest.setup, undefined);
  assert.deepEqual(manifest.hostRequirements, {
    minimumVersion: "0.2.40",
    capabilities: ["connector.process.host-managed.v1"],
  });
  assert.equal(manifest.configSchema.properties.codexBinary.default, undefined);
  assert.match(manifest.configSchema.properties.codexBinary.description, /Advanced override only/);
  const html = await readFile(join(root, manifest.ui.entry), "utf8");
  assert.match(html, /src="\.\/app\.js"/);
  assert.match(html, /href="\.\/styles\.css"/);
  assert.doesNotMatch(html, /<script(?![^>]*\bsrc=)[^>]*>/i);
  assert.doesNotMatch(html, /项目 ID|确认设备/);
  assert.match(html, /切换账号与工作区/);
  assert.match(html, /auth-switch-modal/);
  assert.match(html, /原有 CODEX_HOME/);
  const app = await readFile(join(root, "ui", "app.js"), "utf8");
  assert.match(app, /恢复原有 Codex 环境/);
  assert.match(app, /不会删除任何工作区目录/);
  assert.doesNotMatch(app, /window\.confirm/);
  assert.match(app, /openAuthSwitchModal/);
  assert.doesNotMatch(app, /请关闭后重新打开/);
  assert.match(app, /bridge\(\)\.invoke\("ensureCodexReady"/);
  assert.match(app, /重新安装并修复/);
  assert.doesNotMatch(app, /Promise\.all\(\[loadSessions\(\), loadState\(\)\]\)/);
  await readFile(join(root, "ui", "state.mjs"), "utf8");
  await readFile(join(root, "ui", "styles.css"), "utf8");
});

test("UI derives Codex initialization progress from installer steps and download bytes", () => {
  const progress = normalizeSetupProgress({
    status: "running",
    installerStatus: {
      currentStep: 3,
      steps: [
        { index: 1, name: "Check App", state: "completed", detail: "Ready" },
        { index: 2, name: "Read manifest", state: "skipped", detail: "Not needed" },
        {
          index: 3,
          name: "Download App",
          state: "running",
          detail: "Downloading",
          downloadedBytes: 50,
          totalBytes: 100,
        },
        { index: 4, name: "Install App", state: "pending" },
      ],
    },
  });
  assert.equal(progress.status, "running");
  assert.equal(progress.currentStep, 3);
  assert.equal(progress.percent, 63);
  assert.equal(progress.steps[2].downloadedBytes, 50);
  assert.equal(progress.steps[2].totalBytes, 100);
  assert.equal(normalizeSetupProgress({ status: "succeeded", installerStatus: { steps: [] } }).percent, 100);
  assert.equal(shouldShowSetupProgress({ status: "running", installerStatus: { steps: [{ state: "running" }] } }), true);
  assert.equal(shouldShowSetupProgress({ status: "succeeded", installerStatus: { steps: [{ state: "completed" }] } }), false);
});

test("UI keeps Codex sessions newest-first and extracts conversation messages", () => {
  const sessions = normalizeCodexSessions([
    { id: "old", name: "Old", updated_at: 100 },
    { id: "new", name: "New", updated_at: 200 },
  ]);
  assert.deepEqual(sessions.map((session) => session.id), ["new", "old"]);
  assert.deepEqual(codexTurnMessages([{
    id: "turn-1",
    items: [
      { id: "user-1", type: "user_message", text: "实现功能" },
      { id: "agent-1", type: "agent_message", content: [{ text: "已经完成" }] },
    ],
  }]), [
    { id: "user-1", role: "user", text: "实现功能" },
    { id: "agent-1", role: "assistant", text: "已经完成" },
  ]);
});

test("UI state normalizes management responses and keeps the active workspace", () => {
  const state = normalizeCredentialState({
    activeMode: "baijimu",
    codexConfigured: true,
    currentWorkspaceId: 642,
    activeWorkspaceId: 642,
    credentialStatus: "verified",
    activeProfile: {
      workspaceId: 642,
      workspaceName: "研发",
      model: "gpt-5.6-sol",
      activatedAtEpochSeconds: 123,
    },
    profiles: [],
    workspaces: [
      { workspaceId: 100, name: "其他", authorized: false },
      { workspaceId: 642, name: "研发", authorized: true, configured: true, userIds: [25] },
    ],
    chatgpt: { configured: true, authMode: "chatgpt", accountId: "acct-1" },
    originalCodexHome: "/users/test/.codex",
    originalCodexHomeState: { captured: true, value: null, captureSource: "user-environment" },
    activeCodexHome: "/isolated/workspace-642",
    userCodexHome: "/isolated/workspace-642",
    userCodexHomeSynchronized: true,
    desktopEnvironmentManaged: true,
  });
  assert.equal(state.codexConfigured, true);
  assert.equal(state.currentWorkspaceId, 642);
  assert.equal(state.activeMode, "baijimu");
  assert.equal(state.activeWorkspaceId, 642);
  assert.equal(state.workspaces[1].authorized, true);
  assert.deepEqual(state.workspaces[1].userIds, [25]);
  assert.equal(state.chatgpt.accountId, "acct-1");
  assert.equal(state.originalCodexHome, "/users/test/.codex");
  assert.equal(state.originalCodexHomeState.captured, true);
  assert.equal(state.originalCodexHomeState.wasSet, false);
  assert.equal(state.userCodexHomeSynchronized, true);
  assert.equal(state.desktopEnvironmentManaged, true);
  assert.equal(state.activeProfile.workspaceId, 642);
  assert.deepEqual(credentialStatusMeta(state.credentialStatus), { label: "已验证", tone: "success" });
});
