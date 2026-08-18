import assert from "node:assert/strict";
import { test } from "node:test";

import {
  codexCapabilityMeta,
  setupActionMeta,
  setupStatusMeta,
} from "../ui/state.mjs";

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
