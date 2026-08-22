import assert from "node:assert/strict";
import { test } from "node:test";

import {
  codexWorkspaceMeta,
  codexCapabilityMeta,
  defaultWorkspaceMeta,
  normalizeCredentialState,
  primaryViewMeta,
  profileBadgeMeta,
  setupActionMeta,
  setupStatusMeta,
} from "../ui/state.mjs";

test("Codex workspaces keep independent authentication channel selections", () => {
  const state = normalizeCredentialState({
    activeCodexWorkspaceId: "workspace-a",
    codexWorkspaces: [{
      workspaceId: "workspace-a",
      name: "研发",
      codexHome: "/private/codex/a",
      authProfileId: "personal:chatgpt",
      active: true,
    }, {
      workspaceId: "workspace-b",
      name: "生产排障",
      codexHome: "/private/codex/b",
      authProfileId: "workspace:42",
    }],
    profiles: [{
      profileId: "personal:chatgpt",
      kind: "personal",
      credentialStatus: "configured",
    }, {
      profileId: "workspace:42",
      kind: "baijimu",
      name: "平台授权",
      workspaceId: 42,
      credentialStatus: "verified",
    }],
  });

  assert.equal(state.codexWorkspaces.length, 2);
  assert.equal(state.activeCodexWorkspace.workspaceId, "workspace-a");
  assert.equal(codexWorkspaceMeta(state.codexWorkspaces[0], state.profiles).channelName, "ChatGPT 登录");
  assert.deepEqual(codexWorkspaceMeta(state.codexWorkspaces[1], state.profiles), {
    profile: state.profiles[1],
    channelName: "平台授权",
    channelDetail: "百积木工作区 42 模型授权",
    channelAvailable: true,
  });
});

test("personal auth profiles do not require a workspace dimension", () => {
  const state = normalizeCredentialState({
    activeMode: "chatgpt",
    activeProfile: {
      profileId: "personal:installation-backup",
      kind: "personal",
      name: "安装前授权备份",
      credentialStatus: "verified",
    },
    profiles: [{
      profileId: "personal:installation-backup",
      kind: "personal",
      name: "安装前授权备份",
      credentialStatus: "verified",
    }],
  });
  assert.equal(state.activeMode, "chatgpt");
  assert.equal(state.profiles.length, 1);
  assert.equal(state.profiles[0].workspaceId, null);
  assert.equal(state.profiles[0].kind, "personal");
  assert.equal(Object.hasOwn(state.profiles[0], "model"), false);
});

test("authorization profiles expose source and compact status badges", () => {
  assert.deepEqual(
    profileBadgeMeta({ active: true, kind: "personal" }),
    [
      { label: "当前", tone: "current" },
      { label: "ChatGPT 登录", tone: "default" },
    ],
  );
  assert.deepEqual(
    profileBadgeMeta({ disabled: true }),
    [
      { label: "百积木授权", tone: "default" },
      { label: "未授权", tone: "warning" },
    ],
  );
  assert.deepEqual(
    profileBadgeMeta({ configured: false }),
    [
      { label: "百积木授权", tone: "default" },
      { label: "未初始化", tone: "warning" },
    ],
  );
  assert.deepEqual(
    profileBadgeMeta({ kind: "personal", credentialStatus: "login_required" }),
    [
      { label: "ChatGPT 登录", tone: "default" },
      { label: "需登录", tone: "warning" },
    ],
  );
  assert.deepEqual(
    profileBadgeMeta({ credentialStatus: "missing" }),
    [
      { label: "百积木授权", tone: "default" },
      { label: "需重新授权", tone: "danger" },
    ],
  );
});

test("the default Codex workspace exposes its current authentication channel", () => {
  const state = normalizeCredentialState({
    activeMode: "baijimu",
    activeCodexHome: "/Users/example/.codex",
    activeProfile: {
      profileId: "workspace:42",
      kind: "baijimu",
      name: "产品工作区",
      workspaceId: 42,
      credentialStatus: "verified",
    },
    profiles: [{
      profileId: "workspace:42",
      kind: "baijimu",
      name: "产品工作区",
      workspaceId: 42,
      credentialStatus: "verified",
    }, {
      profileId: "personal:chatgpt",
      kind: "personal",
      credentialStatus: "login_required",
    }],
  });

  assert.deepEqual(defaultWorkspaceMeta(state), {
    name: "产品工作区",
    detail: "百积木工作区 42 提供模型访问授权。",
    badge: { label: "百积木授权", tone: "success" },
    codexHome: "/Users/example/.codex",
    selectableChannelCount: 2,
  });
});

test("workspace authentication data stays independent from installation readiness", () => {
  const chatgptWorkspace = defaultWorkspaceMeta(normalizeCredentialState({
    activeMode: "chatgpt",
    activeProfile: {
      profileId: "personal:chatgpt",
      kind: "personal",
      credentialStatus: "login_required",
    },
  }));
  const failedInstallation = codexCapabilityMeta({ status: "failed" });

  assert.equal(chatgptWorkspace.name, "ChatGPT 登录");
  assert.deepEqual(chatgptWorkspace.badge, { label: "ChatGPT · 需登录", tone: "warning" });
  assert.equal(failedInstallation.label, "安装失败");
  assert.equal(failedInstallation.available, false);
});

test("installation and default workspace are mutually exclusive primary views", () => {
  for (const status of ["pending", "running", "failed", "interrupted", "needs_retry"]) {
    assert.deepEqual(primaryViewMeta({ status }), {
      workspaceVisible: false,
      setupVisible: true,
    });
  }
  assert.deepEqual(primaryViewMeta({ status: "succeeded" }), {
    workspaceVisible: true,
    setupVisible: false,
  });
  assert.deepEqual(
    primaryViewMeta({ status: "succeeded", completedAtEpochSeconds: null, retryable: true }),
    { workspaceVisible: true, setupVisible: false },
  );
});

test("a running route verification keeps Codex available", () => {
  const setup = {
    status: "succeeded",
    completedAtEpochSeconds: null,
    retryable: false,
  };

  assert.deepEqual(setupStatusMeta(setup), {
    status: "succeeded",
    label: "路由验证中",
    retryable: false,
    showCurrentError: false,
  });
  assert.equal(codexCapabilityMeta(setup).available, true);
  assert.equal(codexCapabilityMeta(setup).tone, "warning");
  assert.equal(setupActionMeta(setup).visible, false);
});

test("a failed route verification is a retryable warning, not an installation failure", () => {
  const setup = {
    status: "succeeded",
    completedAtEpochSeconds: 1_787_000_000,
    retryable: true,
    lastError: "route timed out",
  };

  assert.equal(setupStatusMeta(setup).label, "路由验证失败");
  assert.equal(codexCapabilityMeta(setup).available, true);
  assert.equal(codexCapabilityMeta(setup).label, "可打开 · 验证失败");
  assert.deepEqual(setupActionMeta(setup), {
    visible: true,
    operation: "verify",
    label: "重新验证路由",
  });
});

test("an actual installation failure still requires installation repair", () => {
  const setup = {
    status: "failed",
    completedAtEpochSeconds: 1_787_000_000,
    retryable: true,
    error: "installer failed",
  };

  assert.equal(codexCapabilityMeta(setup).available, false);
  assert.equal(codexCapabilityMeta(setup).label, "安装失败");
  assert.deepEqual(setupActionMeta(setup), {
    visible: true,
    operation: "retry",
    label: "重新安装并修复",
  });
});
